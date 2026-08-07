//! Provider registry —— 名字 → 工厂函数表。
//!
//! 借鉴 ALLinOne 的"扩展改三处":新增 provider 在这里注册一行即可。
//! 用编译期静态函数表(非 Python 的动态发现)。见 docs/ARCHITECTURE.md §3.2。

use std::collections::HashMap;
use std::sync::Arc;

use gw_core::provider::Provider;

/// provider 工厂:从配置 + worker 的 egress HTTP client 构造一个 Provider 实例。
///
/// egress client 由 worker 按其固定出口 IP 构造,在此注入 provider——保证
/// 该 provider 所有上游请求走同一出口(防关联封号)。不需要 client 的 provider
/// (如 claude-subprocess)忽略它即可。
pub type ProviderFactory =
    fn(&serde_json::Value, reqwest::Client) -> anyhow::Result<Arc<dyn Provider>>;

/// provider 注册表。
pub struct Registry {
    map: HashMap<&'static str, ProviderFactory>,
}

impl Registry {
    /// 构造并注册所有内置 provider。
    ///
    /// **新增 provider:在这里加一行 `reg.register("name", XxxProvider::from_config)`。**
    pub fn with_builtins() -> Self {
        let mut reg = Self {
            map: HashMap::new(),
        };
        reg.register("kiro", gw_kiro::KiroProvider::from_config);
        reg.register(
            "claude-subprocess",
            gw_claude_subprocess::ClaudeSubprocessProvider::from_config,
        );
        reg.register("claude-dario", gw_dario::DarioProvider::from_config);
        reg.register("cursor", gw_cursor::CursorProvider::from_config);
        reg
    }

    pub fn register(&mut self, family: &'static str, factory: ProviderFactory) {
        self.map.insert(family, factory);
    }

    /// 按家族名构造一个 provider 实例,注入 worker 的 egress client。
    pub fn build(
        &self,
        family: &str,
        cfg: &serde_json::Value,
        egress_client: reqwest::Client,
    ) -> anyhow::Result<Arc<dyn Provider>> {
        let factory = self
            .map
            .get(family)
            .ok_or_else(|| anyhow::anyhow!("未知 provider 家族: {family}"))?;
        factory(cfg, egress_client)
    }

    pub fn families(&self) -> Vec<&'static str> {
        self.map.keys().copied().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_include_kiro() {
        let reg = Registry::with_builtins();
        assert!(reg.families().contains(&"kiro"));
    }

    #[test]
    fn builtins_include_claude_subprocess() {
        let reg = Registry::with_builtins();
        assert!(reg.families().contains(&"claude-subprocess"));
    }

    #[test]
    fn builtins_include_claude_dario() {
        let reg = Registry::with_builtins();
        assert!(reg.families().contains(&"claude-dario"));
    }

    #[test]
    fn builtins_include_cursor() {
        let reg = Registry::with_builtins();
        assert!(reg.families().contains(&"cursor"));
    }

    #[test]
    fn build_kiro_provider() {
        let reg = Registry::with_builtins();
        let p = reg
            .build("kiro", &serde_json::Value::Null, reqwest::Client::new())
            .unwrap();
        assert_eq!(p.family(), "kiro");
    }

    #[test]
    fn build_unknown_fails() {
        let reg = Registry::with_builtins();
        assert!(reg
            .build("nope", &serde_json::Value::Null, reqwest::Client::new())
            .is_err());
    }
}
