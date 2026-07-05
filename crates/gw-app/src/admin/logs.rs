//! admin 请求日志端点(调试用):列表 + 详情(含完整报文)。
//!
//! - `GET /logs`      列表(按成功/失败、账号、模型、时间窗筛选;不含 payload)
//! - `GET /logs/{id}`  单条详情(含用户原始报文 + 发 Kiro 前报文 + 去重的媒体 blob)
//!
//! 数据由 worker 在每次 chat 收尾时环形落库(最新 N 条,见 worker::write_request_log)。
//! 时间筛选优先级:`from`/`to`(Unix 秒)> `all=true` > `days`(默认 7,钳 [0,3650])。

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use gw_core::store::RequestLogFilter;
use serde::Deserialize;

use super::{internal_error, AdminState};

/// 每页默认条数(前端未显式传 page_size/limit 时)。
const DEFAULT_PAGE_SIZE: i64 = 50;
/// 每页条数硬上限(钳制客户端传入,防一次请求放大查询/JSON;与环形容量同量级)。
const MAX_LIST_LIMIT: i64 = 2000;

#[derive(Debug, Deserialize)]
pub struct LogsQuery {
    #[serde(default)]
    days: Option<i64>,
    #[serde(default)]
    all: Option<bool>,
    #[serde(default)]
    from: Option<i64>,
    #[serde(default)]
    to: Option<i64>,
    /// 按上游账号筛选(空串=不限)。
    #[serde(default)]
    account: Option<String>,
    /// 按模型筛选(空串=不限)。
    #[serde(default)]
    model: Option<String>,
    /// `true`=只看成功;`false`=只看失败;缺省=全部。
    #[serde(default)]
    success: Option<bool>,
    /// 页码(1 起;缺省 1)。
    #[serde(default)]
    page: Option<i64>,
    /// 每页条数(缺省 DEFAULT_PAGE_SIZE,钳 [1, MAX_LIST_LIMIT]);兼容旧 `limit` 参数。
    #[serde(default)]
    page_size: Option<i64>,
    /// 兼容旧客户端:等价于 page_size。
    #[serde(default)]
    limit: Option<i64>,
}

impl LogsQuery {
    /// 解析分页:返回 (page≥1, page_size∈[1,MAX])。page_size 优先 page_size > limit > 默认。
    fn paging(&self) -> (i64, i64) {
        let page_size = self
            .page_size
            .or(self.limit)
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .clamp(1, MAX_LIST_LIMIT);
        let page = self.page.unwrap_or(1).max(1);
        (page, page_size)
    }

    fn to_filter(&self) -> RequestLogFilter {
        let (since, until) = if self.from.is_some() || self.to.is_some() {
            (self.from, self.to)
        } else if self.all.unwrap_or(false) {
            (None, None)
        } else {
            let days = self.days.unwrap_or(7).clamp(0, 3650);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            (Some(now.saturating_sub(days.saturating_mul(86_400))), None)
        };
        let (page, page_size) = self.paging();
        RequestLogFilter {
            since_unix: since,
            until_unix: until,
            account_id: self.account.clone().filter(|s| !s.is_empty()),
            model: self.model.clone().filter(|s| !s.is_empty()),
            success: self.success,
            limit: page_size,
            offset: (page - 1).saturating_mul(page_size),
        }
    }
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/logs", get(list))
        .route("/logs/{id}", get(detail))
}

async fn list(
    State(st): State<AdminState>,
    Query(q): Query<LogsQuery>,
) -> axum::response::Response {
    let filter = q.to_filter();
    let (page, page_size) = q.paging();
    let items = match st.store.list_request_logs(&filter, DEFAULT_PAGE_SIZE) {
        Ok(rows) => rows,
        Err(e) => return internal_error(e),
    };
    let total = match st.store.count_request_logs(&filter) {
        Ok(n) => n,
        Err(e) => return internal_error(e),
    };
    Json(serde_json::json!({
        "items": items,
        "total": total,
        "page": page,
        "page_size": page_size,
    }))
    .into_response()
}

async fn detail(State(st): State<AdminState>, Path(id): Path<i64>) -> axum::response::Response {
    match st.store.get_request_log(id) {
        Ok(Some(d)) => Json(d).into_response(),
        Ok(None) => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"type":"error","error":{"message":"请求日志不存在"}})),
        )
            .into_response(),
        Err(e) => internal_error(e),
    }
}
