//! admin 用量统计端点(看板数据源)。
//!
//! 三个维度,各支持时间范围 `?days=N`(默认 30)或 `?all=true`(全部):
//! - `GET /usage/summary`   总览卡
//! - `GET /usage/by-model`  按模型表
//! - `GET /usage/by-key`    按客户 apikey 表(按 apikey 统计)

use axum::extract::{Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Deserialize;

use super::{internal_error, AdminState};

/// 时间范围查询:`all=true` → 全部;否则 `days`(默认 30)天内。
#[derive(Debug, Deserialize)]
pub struct RangeQuery {
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    all: Option<bool>,
}

impl RangeQuery {
    /// 起始 Unix 秒(None = 不限)。
    fn since_unix(&self) -> Option<i64> {
        if self.all.unwrap_or(false) {
            return None;
        }
        let days = self.days.unwrap_or(30).max(0);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        Some(now - days * 86_400)
    }
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/usage/summary", get(summary))
        .route("/usage/by-model", get(by_model))
        .route("/usage/by-key", get(by_key))
}

async fn summary(State(st): State<AdminState>, Query(q): Query<RangeQuery>) -> axum::response::Response {
    match st.store.usage_summary(q.since_unix()) {
        Ok(s) => Json(s).into_response(),
        Err(e) => internal_error(e),
    }
}

async fn by_model(State(st): State<AdminState>, Query(q): Query<RangeQuery>) -> axum::response::Response {
    match st.store.usage_by_model(q.since_unix()) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal_error(e),
    }
}

async fn by_key(State(st): State<AdminState>, Query(q): Query<RangeQuery>) -> axum::response::Response {
    match st.store.usage_by_key(q.since_unix()) {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal_error(e),
    }
}
