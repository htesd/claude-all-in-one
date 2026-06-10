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
use gw_core::store::UsageFilter;
use serde::Deserialize;

use super::{internal_error, AdminState};

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
    match st.store.usage_summary(&q.to_filter()) {
        Ok(s) => Json(s).into_response(),
        Err(e) => internal_error(e),
    }
}

async fn by_model(State(st): State<AdminState>, Query(q): Query<RangeQuery>) -> axum::response::Response {
    match st.store.usage_by_model(&q.to_filter()) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal_error(e),
    }
}

async fn by_key(State(st): State<AdminState>, Query(q): Query<RangeQuery>) -> axum::response::Response {
    match st.store.usage_by_key(&q.to_filter()) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal_error(e),
    }
}
