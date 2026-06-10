//! 配置 DTO(instances / accounts / system)。
//!
//! 配置驱动(借鉴 ALLinOne 的 YAML 形态)。三份配置:
//! - [`InstancesConfig`] 进程拓扑(多进程核心:router + 各 worker 出口/账号组)
//! - [`AccountsConfig`]  账号(按组分配到 worker)
//! - [`SystemConfig`]    运行开关(缓存/empty-fallback 等热调参数)

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::account::Account;

// ───────────────────────── instances.yaml ─────────────────────────

/// 进程拓扑配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstancesConfig {
    pub router: RouterConfig,
    pub workers: Vec<WorkerConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouterConfig {
    /// 对外监听地址,如 `0.0.0.0:8990`。
    pub listen: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkerConfig {
    /// 实例号(与 `--instance N` 对应)。
    pub instance: u32,
    /// worker 监听地址(localhost 高位端口),如 `127.0.0.1:9000`。
    pub listen: String,
    /// 出口配置(本 worker 所有请求的固定出口)。
    pub egress: EgressConfig,
    /// 该 worker 管理的账号组名(对应 accounts.yaml 的 groups key)。
    pub account_group: String,
}

/// 出口配置 —— 多进程防关联的核心。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum EgressConfig {
    /// 直连(系统默认出口)。
    Direct,
    /// 绑定本机源 IP(reqwest local_address)。单机双 IPv4 场景。
    LocalIp { address: String },
    /// 走外部代理(固定 IP 代理池)。
    Proxy {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        username: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
}

impl InstancesConfig {
    /// 按实例号查 worker 配置。
    pub fn worker(&self, instance: u32) -> Option<&WorkerConfig> {
        self.workers.iter().find(|w| w.instance == instance)
    }

    /// 拓扑约束校验(router/worker 启动时调用,违规直接拒绝启动):
    /// `account_group` 不得被多个 worker 绑定——账号运行态(并发槽/refresh 单飞/
    /// 冷却)都在单 worker 内存,两 worker 共享一组会让 max_concurrency 翻倍、
    /// rolling refresh_token 互相覆盖;instance 号与 listen 地址同理不得重复。
    pub fn validate(&self) -> anyhow::Result<()> {
        let mut groups = std::collections::HashSet::new();
        let mut instances = std::collections::HashSet::new();
        let mut listens = std::collections::HashSet::new();
        for w in &self.workers {
            if !groups.insert(&w.account_group) {
                anyhow::bail!(
                    "instances.yaml 非法:账号组 '{}' 被多个 worker 绑定(并发与凭据刷新会互踩)",
                    w.account_group
                );
            }
            if !instances.insert(w.instance) {
                anyhow::bail!("instances.yaml 非法:instance={} 重复", w.instance);
            }
            if !listens.insert(&w.listen) {
                anyhow::bail!("instances.yaml 非法:listen '{}' 重复", w.listen);
            }
        }
        Ok(())
    }
}

// ───────────────────────── accounts.yaml ─────────────────────────

/// 账号配置(按组组织)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountsConfig {
    /// 组名 → 组定义。
    pub groups: BTreeMap<String, AccountGroup>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountGroup {
    /// 该组使用的 provider 家族。
    pub provider: String,
    /// 组内账号。
    pub accounts: Vec<Account>,
}

impl AccountsConfig {
    /// 取某组的账号列表。
    pub fn group(&self, name: &str) -> Option<&AccountGroup> {
        self.groups.get(name)
    }

    /// 取某组账号,并把组级 `provider` 传播到每个账号(账号级 provider 省略时)。
    pub fn group_accounts_with_provider(&self, name: &str) -> Option<Vec<Account>> {
        let g = self.groups.get(name)?;
        Some(
            g.accounts
                .iter()
                .cloned()
                .map(|mut a| {
                    if a.provider.is_empty() {
                        a.provider = g.provider.clone();
                    }
                    a
                })
                .collect(),
        )
    }
}

// ───────────────────────── system.yaml ─────────────────────────

/// 运行开关(热调参数,沿用旧项目语义)。
///
/// 注:空响应不设配置——v60 起不做任何反代侧重试/兜底(实战证明换 ID 重发救不回
/// 且 error 放大触发封号),行为固定为:provider 终态 Err(EmptyResponse) →
/// worker report_failure 阈值冷却 → 终态 SSE error → 客户端自重试。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemConfig {
    #[serde(default)]
    pub cache: CacheConfig,
    #[serde(default)]
    pub admin: AdminConfig,
}

/// admin 控制面配置。`token` 未设(None / 空串)→ admin API 关闭(router 不挂 /admin)。
/// 与对外客户 apikey 完全分离:admin_token 是单一管理密钥(system.yaml 持有)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AdminConfig {
    #[serde(default)]
    pub token: Option<String>,
}

impl AdminConfig {
    /// 非空 admin token(启用 admin 的充要条件)。
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref().filter(|t| !t.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    pub read_multiplier: f64,
    pub cap_ratio: f64,
    pub floor_ratio: f64,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            read_multiplier: 1.0,
            cap_ratio: 0.9,
            floor_ratio: 0.1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_instances_with_local_ip_and_proxy() {
        let yaml = r#"
router:
  listen: "0.0.0.0:8990"
workers:
  - instance: 0
    listen: "127.0.0.1:9000"
    egress: { mode: local_ip, address: "203.0.113.10" }
    account_group: "G0"
  - instance: 1
    listen: "127.0.0.1:9001"
    egress: { mode: proxy, url: "socks5://127.0.0.1:1080" }
    account_group: "G1"
"#;
        let cfg: InstancesConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.workers.len(), 2);
        match &cfg.worker(0).unwrap().egress {
            EgressConfig::LocalIp { address } => assert_eq!(address, "203.0.113.10"),
            _ => panic!("expected local_ip"),
        }
        match &cfg.worker(1).unwrap().egress {
            EgressConfig::Proxy { url, .. } => assert_eq!(url, "socks5://127.0.0.1:1080"),
            _ => panic!("expected proxy"),
        }
    }

    #[test]
    fn parse_accounts_groups() {
        let yaml = r#"
groups:
  G0:
    provider: kiro
    accounts:
      - { account_id: k1, refresh_token: t1 }
      - { account_id: k2, refresh_token: t2 }
"#;
        let cfg: AccountsConfig = serde_yaml::from_str(yaml).unwrap();
        let g = cfg.group("G0").unwrap();
        assert_eq!(g.provider, "kiro");
        assert_eq!(g.accounts.len(), 2);
        assert_eq!(g.accounts[0].extra_str("refresh_token"), Some("t1"));
    }

    #[test]
    fn instances_validate_rejects_duplicate_group() {
        let yaml = r#"
router: { listen: "0.0.0.0:8990" }
workers:
  - { instance: 0, listen: "127.0.0.1:9000", egress: { mode: direct }, account_group: "G0" }
  - { instance: 1, listen: "127.0.0.1:9001", egress: { mode: direct }, account_group: "G0" }
"#;
        let cfg: InstancesConfig = serde_yaml::from_str(yaml).unwrap();
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("G0"), "应指出重复组,实际: {err}");
    }

    #[test]
    fn instances_validate_accepts_distinct_topology() {
        let yaml = r#"
router: { listen: "0.0.0.0:8990" }
workers:
  - { instance: 0, listen: "127.0.0.1:9000", egress: { mode: direct }, account_group: "G0" }
  - { instance: 1, listen: "127.0.0.1:9001", egress: { mode: direct }, account_group: "G1" }
"#;
        let cfg: InstancesConfig = serde_yaml::from_str(yaml).unwrap();
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn system_config_defaults() {
        let cfg = SystemConfig::default();
        assert_eq!(cfg.cache.read_multiplier, 1.0);
    }

    #[test]
    fn system_config_ignores_legacy_empty_response_section() {
        // 旧 system.yaml 可能残留 v58 的 empty_response 段,解析必须兼容(忽略)。
        let yaml = "cache:\n  read_multiplier: 1.0\n  cap_ratio: 0.9\n  floor_ratio: 0.1\nempty_response:\n  buffered_fallback: true\n";
        let cfg: SystemConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(cfg.cache.cap_ratio, 0.9);
    }
}
