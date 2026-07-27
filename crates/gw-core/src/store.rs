//! 持久化抽象。
//!
//! 只定义 trait,实现在 gw-store。上层(gw-app)依赖这些抽象,
//! 不依赖具体存储(SQLite/Postgres),便于未来替换与测试 mock。

use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

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
    /// 真实口径缓存读(Kiro 金标准;存量行/非 Kiro 恒 0)。
    pub real_cache_read_tokens: u64,
    /// Kiro 真实积分消耗合计(存量行恒 0,部署后才累计)。
    pub metering_credit: f64,
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
    pub real_cache_read_tokens: u64,
    pub metering_credit: f64,
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
    pub real_cache_read_tokens: u64,
    pub metering_credit: f64,
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
    /// 真:上游 tokenUsageEvent 真实 prefix cache 命中(诊断/真实口径成本;非 Kiro 恒 0)。
    pub real_cache_read_tokens: u64,
    /// credit:Kiro meteringEvent.usage 本次真实积分消耗(成本看板"每积分成本";非 Kiro 恒 0.0)。
    pub metering_credit: f64,
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
    /// 客户 key 所属分组('' = 未分组)。router 据此把请求派发到对应账号组的
    /// worker(G0→kiro / DARIO→dario);未分组回落到 router 自身主组。
    pub group_name: String,
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
    /// 组内成员数(成员边条数)。与 `account_count`(**归属**本组的账号数)是两件事:
    /// 一个号可以归属 G0、同时是低价组的成员,那时它只计入 G0 的 account_count,
    /// 但同时出现在两个组的 member_count 里。
    pub member_count: u64,
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
    /// 累计成功请求数(监控用,非计费)。每次上游调用**终态**收尾 +1(见 worker::write_request_log)。
    pub success_count: i64,
    /// 累计失败请求数(监控用,非计费)。含首包前 400/429/网络/额度耗尽等终态失败。
    pub failure_count: i64,
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

// === 请求日志(调试用:存最新 N 条的报文 + 去重的媒体 blob)===

/// 从报文中抽出的媒体 blob(用户上传的图片/文档 base64)。**内容寻址**:`hash` 为 base64
/// 文本的 sha256,入库 `INSERT OR IGNORE` 自动去重——同一张图在一个会话里每轮都发,只存一份。
#[derive(Debug, Clone, Serialize)]
pub struct LogBlob {
    /// sha256(base64 文本) 十六进制,作内容寻址主键。
    pub hash: String,
    /// MIME 类型(如 `image/png` / `application/pdf`),取自同级 `media_type`,缺失为空。
    pub media_type: String,
    /// base64 原文(前端据此渲染 `data:` URI / 下载)。
    pub data: String,
    /// base64 文本字节数(原始二进制体积 ≈ 本值 ×3/4;前端据此估算原图大小)。
    pub bytes: i64,
}

/// 媒体 blob 抽取阈值:base64 字符串短于此(典型 1x1 占位/小图标)不抽,留原样省去引用开销。
const LOG_BLOB_MIN_LEN: usize = 256;

/// 从 JSON 报文里**就地**抽出媒体 base64,替换为 `"blob:<hash>"` 短引用,返回抽出的 blob 列表
/// (未去重;去重在入库时按 hash 做)。
///
/// **字段感知**:只动键为 `data` / `bytes` 的长字符串值——Anthropic `image/document.source.data`、
/// Kiro `source.bytes`、`images[].source.bytes`。对话正文走 `text` 键,**绝不会被误抽**(修正
/// 旧版按字符集猜测会误伤无空格长正文的问题)。`media_type` 取同级字段(Anthropic 有;Kiro
/// 线缆无 → 留空,前端按 base64 magic 字节兜底推断)。
pub fn extract_log_blobs(v: &mut Value) -> Vec<LogBlob> {
    let mut out = Vec::new();
    extract_blobs_into(v, &mut out);
    out
}

fn extract_blobs_into(v: &mut Value, out: &mut Vec<LogBlob>) {
    match v {
        Value::Object(map) => {
            // 同级 media_type 给 data/bytes 用(不可变借用,在进入 iter_mut 前 clone 出)。
            let media_type = map
                .get("media_type")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .to_string();
            // 是否为媒体 source 对象:有 media_type 或 type=="base64"。用于收紧 "data" 抽取——
            // 只有真正的 image/document.source 才抽,避免把工具入参里名为 "data" 的长正文误抽。
            let looks_like_source = !media_type.is_empty()
                || map.get("type").and_then(|t| t.as_str()) == Some("base64");
            for (k, val) in map.iter_mut() {
                // "bytes":Kiro source.bytes(该键几乎只用于二进制),按长度判即可。
                // "data":Anthropic source.data,需同级是媒体 source 才抽(防误伤同名正文)。
                let is_media_field = ((k == "bytes") || (k == "data" && looks_like_source))
                    && val.as_str().is_some_and(|s| s.len() >= LOG_BLOB_MIN_LEN);
                if is_media_field {
                    // 取出原 base64 文本(替换为 Null 释放借用),再回填 blob 引用。
                    let data = match std::mem::replace(val, Value::Null) {
                        Value::String(s) => s,
                        other => {
                            *val = other;
                            continue;
                        }
                    };
                    let bytes = data.len() as i64;
                    let hash = sha256_hex(data.as_bytes());
                    *val = Value::String(format!("blob:{hash}"));
                    out.push(LogBlob {
                        hash,
                        media_type: media_type.clone(),
                        data,
                        bytes,
                    });
                } else {
                    extract_blobs_into(val, out);
                }
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| extract_blobs_into(x, out)),
        _ => {}
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let digest = hasher.finalize();
    let mut s = String::with_capacity(digest.len() * 2);
    for b in digest {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// 一条请求日志(写入 DTO)。保留**发 Kiro 前的报文**与**用户原始 Anthropic 报文**(文本完整,
/// 媒体 base64 已抽到 `blobs` 去重存储,正文以 `blob:<hash>` 引用),便于调试工具调用/转换问题。
/// 环形保留最新 N 条(见 `SqliteStore::insert_request_log`)。
#[derive(Debug, Clone)]
pub struct RequestLog {
    pub client_key_id: String,
    pub account_id: String,
    pub model: String,
    /// 客户端是否请求流式。
    pub stream: bool,
    pub success: bool,
    /// 我方/上游返回码(可空)。
    pub status_code: Option<i64>,
    /// 错误分类(可空,如 rate_limited / quota_exhausted)。
    pub error_kind: Option<String>,
    /// 总耗时毫秒(可空)。
    pub duration_ms: Option<i64>,
    /// 首字节毫秒(流式,可空)。
    pub ttfb_ms: Option<i64>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_creation_tokens: u64,
    /// 上报给客户的总 token("报",cache-sim 上报口径)。
    pub reported_tokens: u64,
    /// 真:上游真实 cacheReadInputTokens(Kiro tokenUsageEvent;0=miss/无该信号)。
    pub real_cache_read_tokens: u64,
    /// credit:Kiro meteringEvent.usage(真号本次真实计费;0=无该信号)。
    pub metering_credit: f64,
    /// 用户发来的原始 Anthropic 请求体(JSON 文本,媒体已替换为 `blob:<hash>` 引用)。
    pub client_payload: String,
    /// 转换后、发往 Kiro 前的请求体(JSON 文本,媒体已替换为 `blob:<hash>` 引用)。
    pub kiro_payload: String,
    /// 模型回复(我方把上游 SSE 折叠成的单条 Anthropic Messages 响应 JSON;失败/无回复=空串)。
    /// 便于复盘"用户问了什么→模型答了什么";正文为纯文本/思考/工具调用,不含媒体 blob。
    pub response_payload: String,
    /// 从两份报文抽出的媒体 blob(图片/文档),入库按 hash 去重。
    pub blobs: Vec<LogBlob>,
}

/// 请求日志列表项(**不含**大 payload,列表页用——避免一次拉回上百万字符)。
#[derive(Debug, Clone, Serialize)]
pub struct RequestLogRow {
    pub id: i64,
    pub created_at: i64,
    pub client_key_id: String,
    pub account_id: String,
    pub model: String,
    pub stream: bool,
    pub success: bool,
    pub status_code: Option<i64>,
    pub error_kind: Option<String>,
    pub duration_ms: Option<i64>,
    pub ttfb_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    pub reported_tokens: i64,
    /// 真:上游真实命中(0=miss)。
    pub real_cache_read_tokens: i64,
    /// credit:Kiro 原生计费(真号真实消耗)。
    pub metering_credit: f64,
}

/// 请求日志详情(含报文 + 该日志引用的去重媒体 blob,详情页/调试用)。
#[derive(Debug, Clone, Serialize)]
pub struct RequestLogDetail {
    #[serde(flatten)]
    pub row: RequestLogRow,
    pub client_payload: String,
    pub kiro_payload: String,
    /// 模型回复(折叠后的 Anthropic Messages 响应 JSON;旧日志/失败请求=空串)。
    pub response_payload: String,
    /// 本条日志报文里 `blob:<hash>` 引用到的媒体(图片/文档),前端据此渲染。
    pub blobs: Vec<LogBlob>,
}

/// 请求日志筛选(列表查询)。
#[derive(Debug, Clone, Default)]
pub struct RequestLogFilter {
    pub since_unix: Option<i64>,
    pub until_unix: Option<i64>,
    pub account_id: Option<String>,
    pub model: Option<String>,
    /// `Some(true)`=只看成功;`Some(false)`=只看失败;`None`=全部。
    pub success: Option<bool>,
    /// 返回条数上限(0 或负 → 用调用方默认)。
    pub limit: i64,
    /// 分页偏移(跳过前 N 条,id 降序;0=第一页)。
    pub offset: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_pulls_anthropic_image_keeps_text() {
        let big = "A".repeat(LOG_BLOB_MIN_LEN + 10);
        let mut v = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "看这张图,a+b/c=d 是什么"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big}}
                ]
            }]
        });
        let blobs = extract_log_blobs(&mut v);
        assert_eq!(blobs.len(), 1, "应抽出 1 个 blob");
        assert_eq!(blobs[0].media_type, "image/png");
        assert_eq!(blobs[0].bytes, (LOG_BLOB_MIN_LEN + 10) as i64);
        // 报文里 data 变成 blob 引用,正文原样保留。
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.contains(&format!("blob:{}", blobs[0].hash)));
        assert!(s.contains("看这张图"));
        assert!(s.contains("a+b/c=d"), "含标点正文不应被误抽");
        assert!(!s.contains(&"A".repeat(LOG_BLOB_MIN_LEN)), "原始 base64 不应留在报文");
    }

    #[test]
    fn extract_dedup_same_image_same_hash() {
        // 同一张图出现两次(模拟多轮重复发) → 同 hash,前端/入库据此去重。
        let big = "Zm9v".repeat(100);
        let mut v = serde_json::json!({
            "a": {"source": {"bytes": big.clone()}},
            "b": {"source": {"bytes": big}},
        });
        let blobs = extract_log_blobs(&mut v);
        assert_eq!(blobs.len(), 2);
        assert_eq!(blobs[0].hash, blobs[1].hash, "相同内容 → 相同 hash");
    }

    #[test]
    fn extract_skips_bare_data_without_media_sibling() {
        // 工具入参里名为 "data" 的长正文(无 media_type / type=base64 同级)不应被误抽。
        let long = "x".repeat(LOG_BLOB_MIN_LEN + 10);
        let mut v = serde_json::json!({ "input": { "data": long.clone() } });
        let blobs = extract_log_blobs(&mut v);
        assert!(blobs.is_empty(), "非媒体 source 的 data 不抽");
        assert!(serde_json::to_string(&v).unwrap().contains(&long), "长正文原样保留");
    }

    #[test]
    fn extract_pulls_kiro_bytes() {
        let big = "Qk0".to_string() + &"B".repeat(LOG_BLOB_MIN_LEN);
        let mut v = serde_json::json!({ "source": { "bytes": big } });
        let blobs = extract_log_blobs(&mut v);
        assert_eq!(blobs.len(), 1);
        assert_eq!(blobs[0].media_type, "", "Kiro 线缆无同级 media_type → 空");
    }

    #[test]
    fn extract_skips_short_and_text_fields() {
        let long_text = "The quick brown fox. ".repeat(50); // 长正文(含空格)在 text 键
        let mut v = serde_json::json!({
            "text": long_text.clone(),
            "data": "c2hvcnQ=",  // 短 base64 < 阈值
        });
        let blobs = extract_log_blobs(&mut v);
        assert!(blobs.is_empty(), "text 键与短 data 都不抽");
        let s = serde_json::to_string(&v).unwrap();
        assert!(s.contains("The quick brown fox."));
        assert!(s.contains("c2hvcnQ="));
    }
}
