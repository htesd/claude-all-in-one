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
