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
    #[serde(default)]
    pub scheduler: SchedulerConfig,
    #[serde(default)]
    pub image: ImageConfig,
    #[serde(default)]
    pub experimental: ExperimentalConfig,
}

/// 实验性开关(默认关)。两个 on/off 可经设置面板热控;env(`KIRO_TOOLS_IN_PREFIX` /
/// `KIRO_CACHE_POINT`)作启动默认(后向兼容)。详见 `gw-kiro` converter/cache_point.rs。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExperimentalConfig {
    /// 把工具定义放进 history[0] 前缀(蹭 Kiro 缓存)。⚠️ 实测会让部分客户端工具调用失效,
    /// 默认关。
    #[serde(default = "default_tools_in_prefix")]
    pub tools_in_prefix: bool,
    /// 把 Anthropic `cache_control` 翻成 Kiro `cachePoint`(实测 no-op,dormant)。
    #[serde(default = "default_cache_point")]
    pub cache_point: bool,
}

fn env_experimental_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}
fn default_tools_in_prefix() -> bool {
    env_experimental_flag("KIRO_TOOLS_IN_PREFIX")
}
fn default_cache_point() -> bool {
    env_experimental_flag("KIRO_CACHE_POINT")
}

impl Default for ExperimentalConfig {
    fn default() -> Self {
        Self {
            tools_in_prefix: default_tools_in_prefix(),
            cache_point: default_cache_point(),
        }
    }
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
    /// cache_sim 会话条目 TTL(秒);worker 启动时同步到全局 sim store。
    /// 带 serde 默认,旧 system.yaml(无此字段)仍可解析。
    #[serde(default = "default_sim_ttl_secs")]
    pub sim_ttl_secs: u64,
    /// cache_sim 最大会话数(LRU 容量);worker 启动时同步到全局 sim store。
    #[serde(default = "default_max_sessions")]
    pub max_sessions: usize,
}

fn default_sim_ttl_secs() -> u64 {
    300
}
fn default_max_sessions() -> usize {
    4096
}

/// 账号调度/冷却参数(worker 启动时注入 AccountScheduler)。
///
/// 默认值对齐 kiro.rs 生产配置(rateLimitCooldownSecs=300 / emptyResponse 60s·3次·60s窗 /
/// affinityMapTtlSecs=1800)。`max_failures` 例外:kiro.rs 是 3 但其 5xx/网络错误**不计数**
/// (重试同号);本项目 5xx/网络计入 failure_count 并换号,故默认放宽到 5,避免上游抖动
/// 误禁健康号(有全灭自愈兜底)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    /// 429 限流冷却秒数(到期自愈)。
    #[serde(default = "default_rate_limit_cooldown_secs")]
    pub rate_limit_cooldown_secs: u64,
    /// 空响应冷却秒数(达阈值后)。
    #[serde(default = "default_empty_response_cooldown_secs")]
    pub empty_response_cooldown_secs: u64,
    /// 空响应计数固定窗口秒数。
    #[serde(default = "default_empty_response_window_secs")]
    pub empty_response_window_secs: u64,
    /// 窗口内空响应达此次数才冷却(避免误伤偶发 empty 的健康号)。
    #[serde(default = "default_empty_response_threshold")]
    pub empty_response_threshold: u32,
    /// 连续失败(5xx/网络)达此次数自动禁用(TooManyFailures,可全灭自愈)。
    #[serde(default = "default_max_failures")]
    pub max_failures: u32,
    /// 会话亲和条目 TTL 秒数(超时未访问 = 会话重开,自然再平衡)。
    #[serde(default = "default_affinity_ttl_secs")]
    pub affinity_ttl_secs: u64,
    /// worker 后台配额轮询开关(默认开)。复刻真实 Kiro IDE 每 ~5min 一次 getUsageLimits 的
    /// ambient 流量(防封)+ 让配额面板无人看 dashboard 时也新鲜。与冷却阈值同放 SchedulerConfig
    /// 是为复用既有 30s 热应用路径——admin 设置面板可即时启停轮询,无需重启(对抗审查 Architect#5)。
    #[serde(default = "default_quota_poll_enabled")]
    pub quota_poll_enabled: bool,
}

fn default_rate_limit_cooldown_secs() -> u64 {
    300
}
fn default_empty_response_cooldown_secs() -> u64 {
    60
}
fn default_empty_response_window_secs() -> u64 {
    60
}
fn default_empty_response_threshold() -> u32 {
    3
}
fn default_max_failures() -> u32 {
    5
}
fn default_quota_poll_enabled() -> bool {
    true
}
fn default_affinity_ttl_secs() -> u64 {
    1800
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            rate_limit_cooldown_secs: default_rate_limit_cooldown_secs(),
            empty_response_cooldown_secs: default_empty_response_cooldown_secs(),
            empty_response_window_secs: default_empty_response_window_secs(),
            empty_response_threshold: default_empty_response_threshold(),
            max_failures: default_max_failures(),
            affinity_ttl_secs: default_affinity_ttl_secs(),
            quota_poll_enabled: default_quota_poll_enabled(),
        }
    }
}

/// 图像压缩参数(🔵 移植 kiro.rs/xkiro 的四档阈值;缩放规则对齐 Anthropic 官方建议)。
///
/// 多模态请求里 base64 原图会显著撑大请求体(撞上游字节上限/抬高成本),且恶意构造的
/// 解压炸弹可 OOM 整个 worker——压缩模块自带解码前护栏。`Copy`:压缩在 blocking
/// 线程池执行,按值携带配置。
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ImageConfig {
    /// 总开关。关闭后所有图片原样透传,不解码不缩放(护栏也随之失效)。
    #[serde(default = "default_image_enabled")]
    pub enabled: bool,
    /// 长边像素上限,超过则等比缩放。
    #[serde(default = "default_image_max_long_edge")]
    pub max_long_edge: u32,
    /// 单图模式总像素上限。
    #[serde(default = "default_image_max_pixels_single")]
    pub max_pixels_single: u32,
    /// 多图模式总像素上限(图片数 ≥ multi_threshold 时生效)。默认与单图相等
    /// (开箱不产生差异,仅预留;对齐 xkiro 默认)。
    #[serde(default = "default_image_max_pixels_multi")]
    pub max_pixels_multi: u32,
    /// 多图档阈值:请求内图片数达此值即用多图像素上限。
    #[serde(default = "default_image_multi_threshold")]
    pub multi_threshold: usize,
}

fn default_image_enabled() -> bool {
    true
}
fn default_image_max_long_edge() -> u32 {
    4000
}
fn default_image_max_pixels_single() -> u32 {
    4_000_000
}
fn default_image_max_pixels_multi() -> u32 {
    4_000_000
}
fn default_image_multi_threshold() -> usize {
    20
}

impl Default for ImageConfig {
    fn default() -> Self {
        Self {
            enabled: default_image_enabled(),
            max_long_edge: default_image_max_long_edge(),
            max_pixels_single: default_image_max_pixels_single(),
            max_pixels_multi: default_image_max_pixels_multi(),
            multi_threshold: default_image_multi_threshold(),
        }
    }
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            read_multiplier: 1.0,
            cap_ratio: 0.9,
            floor_ratio: 0.1,
            sim_ttl_secs: default_sim_ttl_secs(),
            max_sessions: default_max_sessions(),
        }
    }
}

// ───────────────────────── 热调设置 overlay(DB 持久) ─────────────────────────

/// DB 持久的**热调设置 overlay**:叠在 `system.yaml`([`SystemConfig`])基线之上。
///
/// 每个字段 `Option<T>`:`None` = 不覆盖(用 YAML 默认),`Some` = 覆盖。router 经
/// admin API 写库,worker 30s 轮询读库后 [`Self::apply_to`] 叠加并热应用——改参无需重启。
/// 进程拓扑(端口/每 worker 源 IP)**不在此**(留 instances.yaml,需重启)。
///
/// `default_proxy` 是唯一不进 [`SystemConfig`] 的字段:它不属于运行开关,而是出口选择,
/// 由 worker 单独取出注入 provider 的 egress resolver(账号无专属代理时的兜底出口)。
///
/// `deny_unknown_fields`:PUT 进来的设置若拼错 key(如 `max_failure`)直接拒绝,
/// 而非静默落库一个永不生效的死 overlay(对抗审查 Skeptic#5/Architect#8/Minimalist#2)。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_proxy: Option<String>,
    // —— cache ——
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read_multiplier: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_cap_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_floor_ratio: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_sim_ttl_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_max_sessions: Option<usize>,
    // —— scheduler ——
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_cooldown_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_response_cooldown_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_response_window_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub empty_response_threshold: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_failures: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affinity_ttl_secs: Option<u64>,
    /// worker 后台配额轮询热开关(None = 用 yaml 基线默认 true)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_poll_enabled: Option<bool>,
    // —— image ——
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_max_long_edge: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_max_pixels_single: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_max_pixels_multi: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_multi_threshold: Option<usize>,
    // —— experimental ——
    /// 工具放 history[0] 前缀实验(默认关;⚠️ 部分客户端工具调用会失效)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_in_prefix: Option<bool>,
    /// cache_control→cachePoint 实验(实测 no-op,dormant)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_point: Option<bool>,
}

impl SystemSettings {
    /// 把非 None 字段叠加到 `base`(原地)。`default_proxy` 不属 SystemConfig,不在此处理。
    pub fn apply_to(&self, base: &mut SystemConfig) {
        if let Some(v) = self.cache_read_multiplier { base.cache.read_multiplier = v; }
        if let Some(v) = self.cache_cap_ratio { base.cache.cap_ratio = v; }
        if let Some(v) = self.cache_floor_ratio { base.cache.floor_ratio = v; }
        if let Some(v) = self.cache_sim_ttl_secs { base.cache.sim_ttl_secs = v; }
        if let Some(v) = self.cache_max_sessions { base.cache.max_sessions = v; }
        if let Some(v) = self.rate_limit_cooldown_secs { base.scheduler.rate_limit_cooldown_secs = v; }
        if let Some(v) = self.empty_response_cooldown_secs { base.scheduler.empty_response_cooldown_secs = v; }
        if let Some(v) = self.empty_response_window_secs { base.scheduler.empty_response_window_secs = v; }
        if let Some(v) = self.empty_response_threshold { base.scheduler.empty_response_threshold = v; }
        if let Some(v) = self.max_failures { base.scheduler.max_failures = v; }
        if let Some(v) = self.affinity_ttl_secs { base.scheduler.affinity_ttl_secs = v; }
        if let Some(v) = self.quota_poll_enabled { base.scheduler.quota_poll_enabled = v; }
        if let Some(v) = self.image_enabled { base.image.enabled = v; }
        if let Some(v) = self.image_max_long_edge { base.image.max_long_edge = v; }
        if let Some(v) = self.image_max_pixels_single { base.image.max_pixels_single = v; }
        if let Some(v) = self.image_max_pixels_multi { base.image.max_pixels_multi = v; }
        if let Some(v) = self.image_multi_threshold { base.image.multi_threshold = v; }
        if let Some(v) = self.tools_in_prefix { base.experimental.tools_in_prefix = v; }
        if let Some(v) = self.cache_point { base.experimental.cache_point = v; }
    }

    /// 由**有效** SystemConfig + 独立的 default_proxy 反构出全量(每字段都 Some)。
    /// admin `GET /settings` 用它把"有效值"回灌给前端(前端展示当前生效值)。
    pub fn from_effective(cfg: &SystemConfig, default_proxy: Option<String>) -> Self {
        Self {
            default_proxy,
            cache_read_multiplier: Some(cfg.cache.read_multiplier),
            cache_cap_ratio: Some(cfg.cache.cap_ratio),
            cache_floor_ratio: Some(cfg.cache.floor_ratio),
            cache_sim_ttl_secs: Some(cfg.cache.sim_ttl_secs),
            cache_max_sessions: Some(cfg.cache.max_sessions),
            rate_limit_cooldown_secs: Some(cfg.scheduler.rate_limit_cooldown_secs),
            empty_response_cooldown_secs: Some(cfg.scheduler.empty_response_cooldown_secs),
            empty_response_window_secs: Some(cfg.scheduler.empty_response_window_secs),
            empty_response_threshold: Some(cfg.scheduler.empty_response_threshold),
            max_failures: Some(cfg.scheduler.max_failures),
            affinity_ttl_secs: Some(cfg.scheduler.affinity_ttl_secs),
            quota_poll_enabled: Some(cfg.scheduler.quota_poll_enabled),
            image_enabled: Some(cfg.image.enabled),
            image_max_long_edge: Some(cfg.image.max_long_edge),
            image_max_pixels_single: Some(cfg.image.max_pixels_single),
            image_max_pixels_multi: Some(cfg.image.max_pixels_multi),
            image_multi_threshold: Some(cfg.image.multi_threshold),
            tools_in_prefix: Some(cfg.experimental.tools_in_prefix),
            cache_point: Some(cfg.experimental.cache_point),
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

    #[test]
    fn settings_apply_to_overwrites_only_present_fields() {
        let mut base = SystemConfig::default();
        let before_cooldown = base.scheduler.empty_response_cooldown_secs;
        let s = SystemSettings {
            max_failures: Some(1),
            cache_read_multiplier: Some(2.5),
            image_enabled: Some(false),
            ..Default::default()
        };
        s.apply_to(&mut base);
        // 覆盖的字段变了。
        assert_eq!(base.scheduler.max_failures, 1);
        assert_eq!(base.cache.read_multiplier, 2.5);
        assert!(!base.image.enabled);
        // 未提供的字段不动。
        assert_eq!(base.scheduler.empty_response_cooldown_secs, before_cooldown);
        assert_eq!(base.cache.cap_ratio, 0.9);
        // default_proxy 不进 SystemConfig(此处仅确认 apply_to 不 panic 即可)。
    }

    #[test]
    fn settings_from_effective_is_identity_under_apply_to() {
        // from_effective(默认配置) 再 apply_to 回默认配置,应得到同一份配置。
        let base = SystemConfig::default();
        let full = SystemSettings::from_effective(&base, Some("socks5://h:1".into()));
        assert_eq!(full.default_proxy.as_deref(), Some("socks5://h:1"));
        let mut target = SystemConfig::default();
        // 先扰动,再用 full 叠回,验证全字段都被 Some 覆盖。
        target.scheduler.max_failures = 999;
        target.cache.read_multiplier = 999.0;
        full.apply_to(&mut target);
        assert_eq!(target.scheduler.max_failures, base.scheduler.max_failures);
        assert_eq!(target.cache.read_multiplier, base.cache.read_multiplier);
    }
}
