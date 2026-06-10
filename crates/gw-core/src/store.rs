//! 持久化抽象。
//!
//! 只定义 trait,实现在 gw-store。上层(gw-app)依赖这些抽象,
//! 不依赖具体存储(SQLite/Postgres),便于未来替换与测试 mock。

use async_trait::async_trait;
use serde::Serialize;

/// 用量总览(admin 看板顶部卡)。
#[derive(Debug, Clone, Default, Serialize)]
pub struct UsageSummary {
    pub requests: u64,
    pub success_requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

/// 按模型聚合行。
#[derive(Debug, Clone, Serialize)]
pub struct UsageByModel {
    pub model: String,
    pub requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

/// 按客户 apikey(client_key_id)聚合行。空 client_key_id = 未归属桶。
#[derive(Debug, Clone, Serialize)]
pub struct UsageByKey {
    pub client_key_id: String,
    pub requests: u64,
    pub success_requests: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
}

/// 一条 usage 记录(发往 UsageSink)。
///
/// 字段与 [`crate::provider::ChatUsage`] 对齐(忠实原始事件日志,不丢任何计费维度):
/// uncached/cache_read/cache_creation 三类 token 经济性都要落库,上层(v59 聚合)才能
/// 重建 cache 折扣成本。
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub client_key_id: String,
    pub account_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// 是否成功。
    pub success: bool,
}

/// 客户端 API key 鉴权结果。
#[derive(Debug, Clone)]
pub struct AuthenticatedKey {
    pub key_id: String,
    pub disabled: bool,
}

/// 控制面存储:鉴权、账号/key/组元数据。
#[async_trait]
pub trait ControlStore: Send + Sync {
    /// 校验一个客户端 API key,返回鉴权信息(None = 无效)。
    async fn authenticate(&self, api_key: &str) -> anyhow::Result<Option<AuthenticatedKey>>;
}

/// usage 写入汇。
#[async_trait]
pub trait UsageSink: Send + Sync {
    async fn record(&self, usage: UsageRecord) -> anyhow::Result<()>;
}
