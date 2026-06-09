//! 持久化抽象。
//!
//! 只定义 trait,实现在 gw-store。上层(gw-app)依赖这些抽象,
//! 不依赖具体存储(SQLite/Postgres),便于未来替换与测试 mock。

use async_trait::async_trait;

/// 一条 usage 记录(发往 UsageSink)。
#[derive(Debug, Clone)]
pub struct UsageRecord {
    pub client_key_id: String,
    pub account_id: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
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
