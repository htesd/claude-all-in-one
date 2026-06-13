//! admin 用量统计端点(看板数据源,支持时间窗 + 按 key 筛选)。
//!
//! 三个维度,共用筛选:
//! - `GET /usage/summary`   总览卡
//! - `GET /usage/by-model`  按模型表
//! - `GET /usage/by-key`    按客户 apikey 表(按 apikey 统计)
//!
//! 时间筛选(优先级):`from`/`to`(Unix 秒,自定义区间)> `all=true`(全部)>
//! `days`(默认 30,钳 [0,3650])。`key` = 只看该客户(空串=只看未归属桶)。

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use gw_core::pricing::price_for;
use gw_core::store::{UsageByModel, UsageFilter, UsageSummary};
use serde::{Deserialize, Serialize};

use super::{internal_error, AdminState};

/// 单模型行的双口径 USD 成本(计费 = 上报 cache_read,真实 = Kiro 金标准 real_cache_read)。
/// 未识别模型(无价)→ (0, 0)。
///
/// ⚠️ caio 的 `input_tokens` 存的是**总上下文**(含 cache_read 子集,见 chat.rs 收尾
/// "库里记总量"),不是 Anthropic 口径的"未命中输入"。故按标准价折算前必须**扣掉缓存命中
/// 部分**得到全价输入(`uncached = input - cache_read`),再单独按命中价计 cache_read,
/// 否则缓存 token 会被既按 input 全价、又按 cache 命中价重复计费。与 kiro.rs 成本面板口径一致。
/// 两口径只在"算哪部分是命中"上不同:计费用上报 cache_read,真实用 Kiro 金标准 real_cache_read。
/// cache_creation 在 v53 统一模型恒 0。
fn model_cost(row: &UsageByModel) -> (f64, f64) {
    match price_for(&row.model) {
        Some(p) => {
            let billed_uncached = row.input_tokens.saturating_sub(row.cache_read_tokens);
            let real_uncached = row.input_tokens.saturating_sub(row.real_cache_read_tokens);
            (
                p.cost_usd(
                    billed_uncached,
                    row.output_tokens,
                    row.cache_read_tokens,
                    row.cache_creation_tokens,
                ),
                p.cost_usd(
                    real_uncached,
                    row.output_tokens,
                    row.real_cache_read_tokens,
                    row.cache_creation_tokens,
                ),
            )
        }
        None => (0.0, 0.0),
    }
}

/// 总览 + 双口径成本。成本按**模型分别计价后求和**(不同模型不同价,不能混算),
/// 故 summary 内部也拉 by_model。token/请求总数仍来自 usage_summary(同筛选,口径一致)。
#[derive(Serialize)]
struct SummaryResp {
    #[serde(flatten)]
    base: UsageSummary,
    cost_billed_usd: f64,
    cost_real_usd: f64,
    /// 命中无法计价模型的请求数(>0 表示成本被低估,前端可提示)。
    unpriced_requests: u64,
}

/// 按模型行 + 双口径成本 + 是否已计价。
#[derive(Serialize)]
struct ModelResp {
    #[serde(flatten)]
    base: UsageByModel,
    cost_billed_usd: f64,
    cost_real_usd: f64,
    priced: bool,
}

#[derive(Debug, Deserialize)]
pub struct RangeQuery {
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    all: Option<bool>,
    /// 自定义区间起点(Unix 秒,含);前端日期选择器换算后传入。
    #[serde(default)]
    from: Option<i64>,
    /// 自定义区间终点(Unix 秒,不含)。
    #[serde(default)]
    to: Option<i64>,
    /// 只看某客户 key(空串=只看未归属桶;缺省=不限)。
    #[serde(default)]
    key: Option<String>,
}

impl RangeQuery {
    fn to_filter(&self) -> UsageFilter {
        let (since, until) = if self.from.is_some() || self.to.is_some() {
            (self.from, self.to) // 自定义区间优先。
        } else if self.all.unwrap_or(false) {
            (None, None)
        } else {
            let days = self.days.unwrap_or(30).clamp(0, 3650);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (Some(now.saturating_sub(days.saturating_mul(86_400))), None)
        };
        UsageFilter {
            since_unix: since,
            until_unix: until,
            client_key_id: self.key.clone(),
        }
    }
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/usage/summary", get(summary))
        .route("/usage/by-model", get(by_model))
        .route("/usage/by-key", get(by_key))
}

async fn summary(State(st): State<AdminState>, Query(q): Query<RangeQuery>) -> axum::response::Response {
    let filter = q.to_filter();
    let base = match st.store.usage_summary(&filter) {
        Ok(s) => s,
        Err(e) => return internal_error(e),
    };
    let models = match st.store.usage_by_model(&filter) {
        Ok(rows) => rows,
        Err(e) => return internal_error(e),
    };
    let (mut cost_billed_usd, mut cost_real_usd, mut unpriced_requests) = (0.0, 0.0, 0u64);
    for m in &models {
        let (b, r) = model_cost(m);
        cost_billed_usd += b;
        cost_real_usd += r;
        if price_for(&m.model).is_none() {
            unpriced_requests = unpriced_requests.saturating_add(m.requests);
        }
    }
    Json(SummaryResp {
        base,
        cost_billed_usd,
        cost_real_usd,
        unpriced_requests,
    })
    .into_response()
}

async fn by_model(State(st): State<AdminState>, Query(q): Query<RangeQuery>) -> axum::response::Response {
    match st.store.usage_by_model(&q.to_filter()) {
        Ok(rows) => {
            let out: Vec<ModelResp> = rows
                .into_iter()
                .map(|m| {
                    let (cost_billed_usd, cost_real_usd) = model_cost(&m);
                    let priced = price_for(&m.model).is_some();
                    ModelResp {
                        base: m,
                        cost_billed_usd,
                        cost_real_usd,
                        priced,
                    }
                })
                .collect();
            Json(out).into_response()
        }
        Err(e) => internal_error(e),
    }
}

async fn by_key(State(st): State<AdminState>, Query(q): Query<RangeQuery>) -> axum::response::Response {
    match st.store.usage_by_key(&q.to_filter()) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal_error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(model: &str, input: u64, cache_read: u64, real_cache_read: u64, output: u64) -> UsageByModel {
        UsageByModel {
            model: model.into(),
            requests: 1,
            input_tokens: input,
            output_tokens: output,
            cache_read_tokens: cache_read,
            cache_creation_tokens: 0,
            real_cache_read_tokens: real_cache_read,
            metering_credit: 0.0,
        }
    }

    #[test]
    fn cost_subtracts_cache_from_total_input() {
        // input_tokens=1000 是总上下文,其中 800 是计费缓存命中、0 是真实命中。
        // 计费:uncached=200 → 200*5 + 100*25 + 800*0.5 = 1000+2500+400 = 3900 / 1e6
        // 真实:uncached=1000 → 1000*5 + 100*25 + 0       = 5000+2500    = 7500 / 1e6
        let (billed, real) = model_cost(&row("claude-opus-4-8", 1000, 800, 0, 100));
        assert!((billed - 0.0039).abs() < 1e-9, "billed={billed}");
        assert!((real - 0.0075).abs() < 1e-9, "real={real}");
        assert!(billed < real, "命中缓存的计费口径应比无折扣的真实口径便宜");
    }

    #[test]
    fn cost_unpriced_model_is_zero() {
        assert_eq!(model_cost(&row("gpt-4o", 1000, 0, 0, 100)), (0.0, 0.0));
    }

    #[test]
    fn cost_cache_exceeding_input_does_not_underflow() {
        // cache_read > input_tokens(异常)→ uncached 饱和到 0,不回绕。
        let (billed, _) = model_cost(&row("claude-sonnet-4-6", 100, 999, 0, 0));
        // uncached=0 → 仅 cache_read 命中价:999*0.3/1e6
        assert!((billed - 999.0 * 0.3 / 1e6).abs() < 1e-9, "billed={billed}");
    }
}
