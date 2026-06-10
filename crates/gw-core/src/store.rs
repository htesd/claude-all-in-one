//! 持久化抽象。
//!
//! 只定义 trait,实现在 gw-store。上层(gw-app)依赖这些抽象,
//! 不依赖具体存储(SQLite/Postgres),便于未来替换与测试 mock。

use async_trait::async_trait;
use serde::Serialize;

/// 用量统计筛选条件。`None` 字段 = 该维度不限。
/// - `since_unix` / `until_unix`:时间窗 [since, until)(Unix 秒);
/// - `client_key_id`:只统计该客户 key(Some("") 表示只看"未归属"桶)。
#[derive(Debug, Clone, Default)]
pub struct UsageFilter {
    pub since_unix: Option<i64>,
    pub until_unix: Option<i64>,
    pub client_key_id: Option<String>,
}

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
    /// 已超限额(quota_tokens 非 NULL 且 used_tokens >= quota_tokens)。
    /// router 据此拒绝(429),计算在 SQL 内完成,鉴权路径零额外查询。
    pub over_quota: bool,
}

/// 一条客户端 API key 元数据(admin 管理页 / CRUD 用)。
/// `key` 即明文密钥本身(也是主键):admin 是密钥的发放方,需要完整值交付客户,
/// 列表接口直接返回明文,前端负责掩码展示。
#[derive(Debug, Clone, Serialize)]
pub struct ApiKeyRow {
    pub key: String,
    pub label: Option<String>,
    pub disabled: bool,
    /// 所属分组(组织/筛选用;'' = 未分组)。
    pub group_name: String,
    /// 限额(单位 token,input+output 计;NULL = 不限)。
    pub quota_tokens: Option<i64>,
    /// 已用量(token,由 UsageSink 落库时原子累加;admin 可重置)。
    pub used_tokens: i64,
    pub created_at: i64,
}

/// API key 部分更新(None 字段不动)。
#[derive(Debug, Clone, Default)]
pub struct ApiKeyPatch {
    /// `Some("")` = 清空备注(落 NULL)。
    pub label: Option<String>,
    pub disabled: Option<bool>,
    /// `Some("")` = 移出分组。
    pub group_name: Option<String>,
    /// `Some(v)`:v > 0 设限额;v <= 0 清除限额(NULL,不限)。
    pub quota_tokens: Option<i64>,
    /// true = 把 used_tokens 归零(限额周期重置)。
    pub reset_used: bool,
}

/// 一个分组(账号组,同时用于 key 的组织归类)。
#[derive(Debug, Clone, Serialize)]
pub struct GroupRow {
    pub name: String,
    /// 前端 chip 颜色(如 "#7c6cf6";'' = 默认)。
    pub color: String,
    pub note: String,
    /// 组内账号数 / 绑定 key 数(列表页直接展示,免前端二次聚合)。
    pub account_count: u64,
    pub key_count: u64,
    pub created_at: i64,
}

/// 一个上游账号(配置态;运行态由 worker 调度器快照提供)。
/// `extra` 是 provider 专属字段的 JSON 文本(refresh_token 等敏感值在内,
/// admin 端点返回前须脱敏)。
#[derive(Debug, Clone, Serialize)]
pub struct AccountRow {
    pub account_id: String,
    pub group_name: String,
    pub provider: String,
    pub max_concurrency: i64,
    pub disabled: bool,
    pub extra: String,
    pub created_at: i64,
}

/// 账号部分更新(None 字段不动)。
#[derive(Debug, Clone, Default)]
pub struct AccountPatch {
    pub group_name: Option<String>,
    pub max_concurrency: Option<i64>,
    pub disabled: Option<bool>,
    /// 整体替换 extra JSON(凭据轮换/修正)。
    pub extra: Option<String>,
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
