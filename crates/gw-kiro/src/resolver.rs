//! 出口 client 解析器 —— 按账号选择正确的 HTTP client(每账号代理的落点)。
//!
//! ## 为什么需要
//!
//! 旧模型:worker 进程一个固定 egress client(绑源 IP),provider 所有上游请求共用。
//! 新需求:**每个账号可指定专属出口代理**,且该账号从 token 刷新 → 配额 → profileArn
//! 发现 → chat 全链路都走**同一个**出口(防封铁律:刷新与发包必须同 IP)。
//!
//! 本解析器按账号返回 client,解析优先级:
//! 1. `account.extra["proxy"]`(账号专属代理,非空);
//! 2. 全局 `default_proxy`(热可调,admin 设置面板);
//! 3. `base_client`(worker 进程绑定的源 IP,即 instances.yaml 的 egress)。
//!
//! 代理 client 用 [`reqwest::Proxy::all`]——出口 IP 由代理决定,故**不**再绑
//! `local_address`;只有走 base(无代理)时才用 worker 那条绑了源 IP 的 client。
//! 二者天然不冲突。client 按 proxy URL 串缓存(连接池复用);构建失败 warn 回退
//! base,**绝不 panic**(配置错误不该让账号彻底不可用)。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gw_core::account::Account;
use parking_lot::{Mutex, RwLock};

/// 按账号解析出口 HTTP client。线程安全,`Arc` 共享。
pub struct EgressResolver {
    /// worker 进程的基础 client(绑了 instances.yaml 配的源 IP/直连)。无代理时用它。
    base_client: reqwest::Client,
    /// 全局默认代理 URL(热可调)。`None`/空 = 不设默认代理。
    default_proxy: RwLock<Option<String>>,
    /// proxy URL → 已构建的代理 client。distinct 代理数实际很小(O(账号代理种类))。
    proxy_cache: Mutex<HashMap<String, reqwest::Client>>,
}

impl EgressResolver {
    /// 用 worker 基础 client + 初始默认代理构造。
    pub fn new(base_client: reqwest::Client, initial_default_proxy: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            base_client,
            default_proxy: RwLock::new(normalize(initial_default_proxy)),
            proxy_cache: Mutex::new(HashMap::new()),
        })
    }

    /// 返回该账号应使用的 client。解析:账号代理 → 默认代理 → base。
    pub fn client_for(&self, account: &Account) -> reqwest::Client {
        let proxy_url = account
            .extra
            .get("proxy")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .or_else(|| self.default_proxy.read().clone());

        match proxy_url {
            None => self.base_client.clone(),
            Some(url) => self.proxy_client(&url),
        }
    }

    /// 热更新全局默认代理(worker 30s 轮询经 apply_hot_settings 调用)。
    pub fn update_default_proxy(&self, proxy: Option<String>) {
        *self.default_proxy.write() = normalize(proxy);
    }

    /// 取/建某 proxy URL 的 client(带缓存,双检锁;构建失败回退 base)。
    fn proxy_client(&self, url: &str) -> reqwest::Client {
        // 快路径:已缓存。
        if let Some(c) = self.proxy_cache.lock().get(url) {
            return c.clone();
        }
        // 慢路径:锁外构建(含系统调用),再双检插入。
        let built = match build_proxy_client(url) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(%url, "构建代理 client 失败,回退 base 出口: {e}");
                return self.base_client.clone();
            }
        };
        let mut cache = self.proxy_cache.lock();
        cache.entry(url.to_string()).or_insert(built).clone()
    }
}

/// 修剪空串为 None(空字符串等价于"未设代理")。
fn normalize(proxy: Option<String>) -> Option<String> {
    proxy.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// 按 proxy URL 构建 client(socks5/http/https 均由 reqwest::Proxy::all 处理)。
/// 超时/连接池参数对齐 [`crate`] worker base client(见 gw-app egress.rs)。
fn build_proxy_client(url: &str) -> anyhow::Result<reqwest::Client> {
    let proxy = reqwest::Proxy::all(url)?;
    // 超时对齐 worker base client 默认(见 gw-app egress.rs / DEFAULT_UPSTREAM_TIMEOUT_SECS):
    // 旧 300s 会腰斩长 Opus 流式响应,统一抬到 720s。
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(
            gw_core::config::DEFAULT_UPSTREAM_TIMEOUT_SECS,
        ))
        .pool_idle_timeout(Duration::from_secs(90))
        .tcp_keepalive(Duration::from_secs(60))
        .proxy(proxy)
        .build()?;
    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn acct(extra: &[(&str, &str)]) -> Account {
        let mut map = BTreeMap::new();
        for (k, v) in extra {
            map.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
        }
        Account {
            account_id: "k".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            extra: map,
        }
    }

    /// 无账号代理 + 无默认代理 → base client(用同一指针确认是 base)。
    #[test]
    fn no_proxy_returns_base() {
        let base = reqwest::Client::new();
        let r = EgressResolver::new(base.clone(), None);
        let got = r.client_for(&acct(&[]));
        // reqwest::Client 内部 Arc,克隆共享同一连接池;用类型行为代替指针等价:
        // 无代理路径不进缓存。
        assert!(r.proxy_cache.lock().is_empty(), "无代理不应建缓存");
        let _ = got;
    }

    /// 账号专属代理 → 进缓存且可复用(第二次同 URL 命中缓存,不重复构建)。
    #[test]
    fn account_proxy_is_cached_and_reused() {
        let r = EgressResolver::new(reqwest::Client::new(), None);
        let a = acct(&[("proxy", "socks5://127.0.0.1:1080")]);
        let _c1 = r.client_for(&a);
        assert_eq!(r.proxy_cache.lock().len(), 1);
        let _c2 = r.client_for(&a);
        assert_eq!(r.proxy_cache.lock().len(), 1, "同 URL 复用,不应新增缓存项");
    }

    /// 默认代理生效:无账号代理时走默认代理(进缓存)。
    #[test]
    fn default_proxy_applies_when_account_has_none() {
        let r = EgressResolver::new(reqwest::Client::new(), Some("http://127.0.0.1:8888".into()));
        let _c = r.client_for(&acct(&[]));
        assert_eq!(r.proxy_cache.lock().len(), 1, "默认代理应建一个代理 client");
    }

    /// 账号代理优先于默认代理(两者不同 URL → 缓存键是账号那个)。
    #[test]
    fn account_proxy_overrides_default() {
        let r = EgressResolver::new(reqwest::Client::new(), Some("http://default:8888".into()));
        let a = acct(&[("proxy", "socks5://acct:1080")]);
        let _c = r.client_for(&a);
        let cache = r.proxy_cache.lock();
        assert!(cache.contains_key("socks5://acct:1080"));
        assert!(!cache.contains_key("http://default:8888"), "应用账号代理,不用默认");
    }

    /// 热更新默认代理:下一次调用立即反映。
    #[test]
    fn update_default_proxy_takes_effect_immediately() {
        let r = EgressResolver::new(reqwest::Client::new(), None);
        let _ = r.client_for(&acct(&[]));
        assert!(r.proxy_cache.lock().is_empty());
        r.update_default_proxy(Some("socks5://new:1080".into()));
        let _ = r.client_for(&acct(&[]));
        assert!(r.proxy_cache.lock().contains_key("socks5://new:1080"));
        // 清空(空串归一为 None)→ 回到 base。
        r.update_default_proxy(Some("  ".into()));
        assert!(r.default_proxy.read().is_none(), "空串应归一为 None");
    }

    /// 空账号 proxy 串等价无代理(归一去空)。
    #[test]
    fn empty_account_proxy_treated_as_none() {
        let r = EgressResolver::new(reqwest::Client::new(), None);
        let _ = r.client_for(&acct(&[("proxy", "   ")]));
        assert!(r.proxy_cache.lock().is_empty(), "空白 proxy 串应当无代理");
    }

    /// 非法代理 URL → 回退 base,**绝不 panic**(reqwest 是否拒绝某串由其内部决定,
    /// 这里只验证不崩溃这条硬保证)。
    #[test]
    fn invalid_proxy_falls_back_without_panic() {
        let r = EgressResolver::new(reqwest::Client::new(), None);
        let a = acct(&[("proxy", "not a valid url ::: %%%")]);
        let _c = r.client_for(&a); // 不 panic 即可
    }
}
