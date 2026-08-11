//! Kiro 模型目录(`ListAvailableModels`)—— **只读**,拿计费倍率与思考档位。
//!
//! 用途有两个,都直接影响钱和形态:
//! 1. **定价**:`rateMultiplier` 是上游按模型收积分的倍率(实测 opus 系 2.2、sonnet 1.3、
//!    haiku 0.4、qwen3-coder-next 0.05)。它会随上游调整,写死在代码里迟早对不上账。
//! 2. **思考档位**:`additionalModelRequestFieldsSchema` 里的 `effort.enum` **逐模型不同**
//!    (4.6 系没有 `xhigh`)。发一个该模型 enum 里没有的档位既可能 400,也是形态差异。
//!
//! 与配额查询同为只读控制面调用,不发推理包、不触发计费,可安全用于验号
//! (见 memory:no-chat-test-on-real-accounts)。
//!
//! 线缆形态逐字对齐拆包 Kiro 1.0.212(`extensions/kiro.kiro-agent/dist/extension.js`):
//! - 域名解析器 `bi2` `:217247` → `https://management.{region}.kiro.dev`;
//! - 操作 schema `:218526` → `{ http: ["GET", "/List-Available-Models", 200] }`
//!   (连字符大驼峰路径,不是 `/ListAvailableModels`);
//! - 调用点 `:223705-223750`:bearer token + `{origin, profileArn, nextToken}`,
//!   翻页上限 `ne11 = 10`(`:223703`),超限**丢弃后续页并告警**——这里逐条照搬。

use gw_core::account::Account;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use serde::{Deserialize, Serialize};

use crate::headers;
use crate::machine_id;
use crate::usage_limits;

const DEFAULT_REGION: &str = "us-east-1";

/// 翻页上限。对齐客户端 `ne11 = 10`(`extension.js:223703`)。
const MAX_PAGES: usize = 10;

/// 一个上游模型条目(只保留我们真正会用的字段)。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpstreamModel {
    pub model_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// **计费倍率** —— 定价的唯一权威来源。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_multiplier: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<i64>,
    /// 该模型可用的 thinking 档位(升序)。空 = 上游没给 schema → 不该发
    /// `additionalModelRequestFields`。
    #[serde(default)]
    pub effort_levels: Vec<String>,
    /// `output_config` 或 `reasoning`。决定 `additionalModelRequestFields` 的外层键名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort_schema_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort_level: Option<String>,
}

/// 一次抓取的完整目录。落库就存它的 JSON。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ModelCatalog {
    pub models: Vec<UpstreamModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// 抓取时刻(unix 秒)。倍率会随上游调整,读的时候要知道它有多旧。
    pub fetched_at: i64,
    /// 用哪个账号抓的。不同订阅档位看到的模型集可能不同,出问题要能回溯。
    pub fetched_by: String,
}

/// 拉取模型目录。account 须已带有效 access_token(调用方先 ensure_credentialed)。
pub async fn list_available_models(
    client: &reqwest::Client,
    account: &Account,
) -> Result<ModelCatalog, UpstreamError> {
    let access_token = headers::bearer_token(account).ok_or_else(|| {
        UpstreamError::new(
            UpstreamErrorKind::TokenInvalid,
            "模型目录查询缺少凭据(access_token / kiro_api_key)",
        )
    })?;

    let region = account
        .extra_str("region")
        .filter(|s| !s.is_empty())
        .unwrap_or(DEFAULT_REGION);
    let host = format!("management.{region}.kiro.dev");
    // 注意:本调用**不随** `KIRO_LEGACY_WIRE` 回退域名 —— ListAvailableModels 是 1.0.212
    // 时代才有的操作,旧 `q.*` 域上没有对应物;且它只由 admin 手动触发(账号页"拉取模型"),
    // 不在逐请求热路径上,不构成形态指纹。回退旧形态时这里是有意保留的例外(见 wire_profile)。
    let machine = machine_id::generate_from_account(account);
    let version = headers::kiro_version(account);
    // 与 getUsageLimits 同一个 control-plane client(同 serviceId / 同包版本)。
    let (x_amz_ua, ua) = usage_limits::control_plane_user_agents(&version, &machine);
    let profile_arn = headers::resolve_profile_arn(account);

    let mut models: Vec<UpstreamModel> = Vec::new();
    let mut default_model: Option<String> = None;
    let mut next_token: Option<String> = None;

    for page in 0..MAX_PAGES {
        let mut url = reqwest::Url::parse(&format!("https://{host}/List-Available-Models"))
            .map_err(|e| UpstreamError::network(format!("模型目录 URL 构造失败: {e}")))?;
        {
            let mut q = url.query_pairs_mut();
            q.append_pair("origin", "AI_EDITOR");
            if let Some(arn) = &profile_arn {
                q.append_pair("profileArn", arn);
            }
            if let Some(t) = &next_token {
                q.append_pair("nextToken", t);
            }
        }

        let rb = client
            .get(url)
            .header("x-amz-user-agent", &x_amz_ua)
            .header("user-agent", &ua)
            .header("host", &host)
            .header("amz-sdk-invocation-id", uuid::Uuid::new_v4().to_string())
            .header("amz-sdk-request", "attempt=1; max=1")
            .header("authorization", format!("Bearer {access_token}"))
            .header("connection", "close");
        // 与配额查询同款条件头:external_idp / API Key 号缺了会 403 或被当 OAuth 处理。
        let rb = headers::apply_external_idp_token_type(rb, account);
        let rb = if machine_id::is_api_key_credential(account) {
            rb.header("TokenType", "API_KEY")
        } else {
            rb
        };

        let resp = rb
            .send()
            .await
            .map_err(|e| UpstreamError::network(format!("模型目录请求失败: {e}")))?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            let kind = match status.as_u16() {
                403 if crate::error_map::is_account_suspended(&body) => {
                    UpstreamErrorKind::TemporarilyBlocked
                }
                401 | 403 => UpstreamErrorKind::TokenInvalid,
                429 => UpstreamErrorKind::RateLimited,
                500..=599 => UpstreamErrorKind::ServerError,
                _ => UpstreamErrorKind::Other,
            };
            return Err(UpstreamError::new(
                kind,
                format!("模型目录查询失败: {} {}", status.as_u16(), body),
            )
            .with_status(status.as_u16()));
        }

        let data: ListModelsResponse = resp
            .json()
            .await
            .map_err(|e| UpstreamError::network(format!("模型目录响应解析失败: {e}")))?;

        models.extend(data.models.into_iter().filter_map(|m| m.into_upstream()));
        if default_model.is_none() {
            default_model = data.default_model.and_then(|d| d.model_id);
        }
        next_token = data.next_token.filter(|t| !t.is_empty());
        if next_token.is_none() {
            break;
        }
        // 到达上限仍有下一页 → 与客户端同样丢弃并告警,而不是无限翻。
        if page + 1 >= MAX_PAGES {
            tracing::warn!(
                pages = MAX_PAGES,
                "模型目录翻页达上限，后续页已丢弃(与客户端同款行为)"
            );
        }
    }

    Ok(ModelCatalog {
        models,
        default_model,
        fetched_at: now_unix(),
        fetched_by: account.account_id.clone(),
    })
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ===== 响应模型 =====

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListModelsResponse {
    #[serde(default)]
    models: Vec<RawModel>,
    #[serde(default)]
    default_model: Option<RawDefaultModel>,
    #[serde(default)]
    next_token: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDefaultModel {
    #[serde(default)]
    model_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawModel {
    #[serde(default)]
    model_id: Option<String>,
    #[serde(default)]
    model_name: Option<String>,
    #[serde(default)]
    model_provider: Option<String>,
    #[serde(default)]
    rate_multiplier: Option<f64>,
    #[serde(default)]
    rate_unit: Option<String>,
    #[serde(default)]
    token_limits: Option<RawTokenLimits>,
    #[serde(default)]
    additional_model_request_fields_schema: Option<serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawTokenLimits {
    #[serde(default)]
    max_input_tokens: Option<i64>,
    #[serde(default)]
    max_output_tokens: Option<i64>,
}

impl RawModel {
    /// 无 `modelId` 的条目直接丢弃(对齐客户端 `filter(x => !!x.modelId)` `:223728`)。
    fn into_upstream(self) -> Option<UpstreamModel> {
        let model_id = self.model_id.filter(|s| !s.trim().is_empty())?;
        let effort = self
            .additional_model_request_fields_schema
            .as_ref()
            .and_then(extract_effort);
        let (levels, path, default) = match effort {
            Some(e) => (e.levels, Some(e.schema_path.to_string()), e.default_level),
            None => (Vec::new(), None, None),
        };
        Some(UpstreamModel {
            model_id,
            model_name: self.model_name,
            provider: self.model_provider,
            rate_multiplier: self.rate_multiplier,
            rate_unit: self.rate_unit,
            max_input_tokens: self.token_limits.as_ref().and_then(|t| t.max_input_tokens),
            max_output_tokens: self.token_limits.as_ref().and_then(|t| t.max_output_tokens),
            effort_levels: levels,
            effort_schema_path: path,
            default_effort_level: default,
        })
    }
}

struct EffortSchema {
    levels: Vec<String>,
    schema_path: &'static str,
    default_level: Option<String>,
}

/// 候选路径,顺序与客户端 `He10`(`extension.js:223753-223759`)一致 ——
/// **先 `output_config` 后 `reasoning`**,取第一个 `enum` 非空的。
const EFFORT_SCHEMA_PATHS: &[&str] = &["output_config", "reasoning"];

/// 复刻客户端 `Be7`(`:222568-222578`)。
fn extract_effort(schema: &serde_json::Value) -> Option<EffortSchema> {
    if !schema.is_object() {
        return None;
    }
    for path in EFFORT_SCHEMA_PATHS {
        let node = schema
            .get("properties")
            .and_then(|p| p.get(path))
            .and_then(|n| n.get("properties"))
            .and_then(|p| p.get("effort"));
        let Some(node) = node else { continue };
        let levels: Vec<String> = node
            .get("enum")
            .and_then(|e| e.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if levels.is_empty() {
            continue;
        }
        return Some(EffortSchema {
            levels,
            schema_path: path,
            default_level: node
                .get("default")
                .and_then(|d| d.as_str())
                .map(str::to_string),
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 用 2026-07-28 真实抓取的 opus-5 条目做夹具(存档
    /// `/root/caio-backup/kiro-models-1.0.212.json`)。
    fn opus5_raw() -> serde_json::Value {
        serde_json::json!({
            "modelId": "claude-opus-5",
            "modelName": "Claude Opus 5",
            "rateMultiplier": 2.2,
            "rateUnit": "Credit",
            "tokenLimits": {"maxInputTokens": 1000000, "maxOutputTokens": 128000},
            "additionalModelRequestFieldsSchema": {
                "type": "object",
                "properties": {
                    "thinking": {"type": "object", "properties": {
                        "type": {"type": "string", "enum": ["adaptive", "disabled"]}
                    }},
                    "output_config": {"type": "object", "properties": {
                        "effort": {"type": "string",
                                   "enum": ["low", "medium", "high", "xhigh", "max"],
                                   "default": "high"}
                    }},
                    "max_tokens": {"type": "integer", "minimum": 1024, "maximum": 128000}
                },
                "additionalProperties": false
            }
        })
    }

    #[test]
    fn parses_rate_multiplier_and_effort_levels() {
        let raw: RawModel = serde_json::from_value(opus5_raw()).unwrap();
        let m = raw.into_upstream().unwrap();
        assert_eq!(m.model_id, "claude-opus-5");
        assert_eq!(m.rate_multiplier, Some(2.2), "定价靠它,解析错了直接亏钱");
        assert_eq!(m.rate_unit.as_deref(), Some("Credit"));
        assert_eq!(m.max_input_tokens, Some(1_000_000));
        assert_eq!(m.max_output_tokens, Some(128_000));
        assert_eq!(m.effort_levels, ["low", "medium", "high", "xhigh", "max"]);
        assert_eq!(m.effort_schema_path.as_deref(), Some("output_config"));
        assert_eq!(m.default_effort_level.as_deref(), Some("high"));
        // `max` 必须出现在档位表末位 —— 它是最高档,不是同义词。
        assert_eq!(m.effort_levels.last().map(String::as_str), Some("max"));
    }

    #[test]
    fn model_without_schema_has_no_effort_levels() {
        // haiku-4.5 实测 additionalModelRequestFieldsSchema 为 null。
        let raw: RawModel = serde_json::from_value(serde_json::json!({
            "modelId": "claude-haiku-4.5",
            "rateMultiplier": 0.4,
            "additionalModelRequestFieldsSchema": null
        }))
        .unwrap();
        let m = raw.into_upstream().unwrap();
        assert!(m.effort_levels.is_empty(), "无 schema 就该是空表,调用方据此不发字段");
        assert!(m.effort_schema_path.is_none());
    }

    #[test]
    fn reasoning_schema_path_is_recognised() {
        // gpt-5.6 系走 `reasoning` 而非 `output_config`(caio 暂不转发它们,但解析要认)。
        let raw: RawModel = serde_json::from_value(serde_json::json!({
            "modelId": "gpt-5.6-sol",
            "additionalModelRequestFieldsSchema": {"type": "object", "properties": {
                "reasoning": {"type": "object", "properties": {
                    "effort": {"enum": ["none", "low", "medium", "high", "xhigh", "max"],
                               "default": "high"}
                }}
            }}
        }))
        .unwrap();
        let m = raw.into_upstream().unwrap();
        assert_eq!(m.effort_schema_path.as_deref(), Some("reasoning"));
        assert_eq!(m.effort_levels.len(), 6, "reasoning 系多一个 none 档");
    }

    #[test]
    fn output_config_wins_over_reasoning_when_both_present() {
        // 复刻客户端 He10 的顺序语义:先 output_config,取第一个 enum 非空的。
        let schema = serde_json::json!({"type": "object", "properties": {
            "output_config": {"properties": {"effort": {"enum": ["low", "high"]}}},
            "reasoning": {"properties": {"effort": {"enum": ["a", "b", "c"]}}}
        }});
        let e = extract_effort(&schema).unwrap();
        assert_eq!(e.schema_path, "output_config");
        assert_eq!(e.levels, ["low", "high"]);
        // 反向:只有 reasoning 时才落到它。
        let only_reasoning = serde_json::json!({"properties": {
            "reasoning": {"properties": {"effort": {"enum": ["a"]}}}
        }});
        assert_eq!(extract_effort(&only_reasoning).unwrap().schema_path, "reasoning");
        // 空 enum 视为没有(客户端 `i30?.enum && i30.enum.length > 0`)。
        let empty = serde_json::json!({"properties": {
            "output_config": {"properties": {"effort": {"enum": []}}}
        }});
        assert!(extract_effort(&empty).is_none());
    }

    #[test]
    fn entries_without_model_id_are_dropped() {
        let raw: RawModel =
            serde_json::from_value(serde_json::json!({"modelName": "无 id 的脏条目"})).unwrap();
        assert!(raw.into_upstream().is_none());
        let blank: RawModel =
            serde_json::from_value(serde_json::json!({"modelId": "   "})).unwrap();
        assert!(blank.into_upstream().is_none(), "空白 id 也要丢");
    }

    #[test]
    fn catalog_json_roundtrips() {
        // 落库存的就是这个 JSON,读回来必须逐字段相等。
        let raw: RawModel = serde_json::from_value(opus5_raw()).unwrap();
        let cat = ModelCatalog {
            models: vec![raw.into_upstream().unwrap()],
            default_model: Some("auto".into()),
            fetched_at: 1_780_000_000,
            fetched_by: "a@b.c".into(),
        };
        let s = serde_json::to_string(&cat).unwrap();
        let back: ModelCatalog = serde_json::from_str(&s).unwrap();
        assert_eq!(cat, back);
    }
}
