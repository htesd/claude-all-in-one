//! gw-core —— 契约层
//!
//! 纯类型 + trait,**无 I/O、无 axum、无 reqwest**。
//! 所有上层(gw-kiro / gw-store / gw-app)依赖此 crate 的抽象,
//! 而非彼此的具体实现。
//!
//! 模块:
//! - [`error`]   上游错误分类(调度层据此决定重试/切号)
//! - [`model`]   模型元数据、设备指纹
//! - [`account`] 账号、字段 schema(驱动前端表单 + YAML)
//! - [`routing`] 路由上下文、session/cache key 派生
//! - [`config`]  配置 DTO(instances/accounts/system)
//! - [`provider`] Provider trait —— 新增上游只需实现它
//! - [`store`]   持久化抽象(实现在 gw-store)

pub mod account;
pub mod config;
pub mod error;
pub mod fold;
pub mod model;
pub mod pricing;
pub mod provider;

/// 读一个**布尔** env 开关。只有明确的真值才算开:`1` / `true` / `yes` / `on`
/// (大小写与前后空白不敏感)。
///
/// 为什么不用 `env::var(..).is_ok()`:部署系统常用 `FOO=0` / `FOO=false` 表达"关",
/// 而"存在即开"会把它读成开。对回退开关来说,这意味着**以为关着其实开着** ——
/// 比如配额端点会静默切回旧域名、旧 UA 并多发一个参数,正好造出我们要消除的形态。
pub fn env_flag(name: &str) -> bool {
    std::env::var(name).map(|v| truthy(&v)).unwrap_or(false)
}

#[cfg(test)]
mod env_flag_tests {
    #[test]
    fn only_explicit_truthy_values_enable() {
        for v in ["1", "true", "TRUE", " yes ", "On"] {
            assert!(super::truthy(v), "{v:?} 应当被视为开");
        }
        // 关键:这些都必须是**关**。此前 `is_ok()` 会把它们全读成开。
        for v in ["0", "false", "no", "off", "", "  ", "2", "enabled"] {
            assert!(!super::truthy(v), "{v:?} 不该被视为开");
        }
    }
}

/// `env_flag` 的纯判定部分(不读 env,便于测试;env 用例并行时会互相污染)。
fn truthy(v: &str) -> bool {
    matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")
}
pub mod routing;
pub mod store;

// 重导出最常用类型,方便上层 `use gw_core::{...}`
pub use account::{Account, FieldSpec, FieldType};
pub use error::{UpstreamError, UpstreamErrorKind};
pub use model::{MachineIdentity, ModelInfo};
pub use provider::{
    CallCtx, ChatRequest, ChatStream, ChatUsage, Provider, SseEvent, StreamItem,
};
pub use routing::RoutingContext;
