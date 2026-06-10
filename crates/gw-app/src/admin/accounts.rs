//! admin 上游账号管理端点(切片④)。
//!
//! - `GET    /accounts`          配置态列表(extra 中含 token 的字段已脱敏)
//! - `GET    /accounts/runtime`  运行态聚合:逐 worker 拉 /health 的调度器快照
//! - `POST   /accounts`          新增(凭据 JSON 进 extra;重复 409)
//! - `PATCH  /accounts/{id}`     部分更新 group/并发/禁用/extra(404 = 不存在)
//! - `DELETE /accounts/{id}`     删除(usage 历史归属保留)
//!
//! 改动经 worker 的 30s 周期 sync 生效,无需重启。DB 是账号事实源;
//! 运行态(冷却/封禁/在途并发)只存在于 worker 内存,经 /health 暴露。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, patch};
use axum::{Json, Router};
use gw_core::store::{AccountPatch, AccountRow};
use serde::Deserialize;

use super::{internal_error, AdminState};

/// account_id 规则:1–64 个 URL-safe 字符(进路径段)。
fn validate_account_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty() || id.len() > 64 {
        return Err("account_id 长度须在 1–64 字符之间");
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
    {
        return Err("account_id 只能含字母、数字及 - _ . ~");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct CreateAccountBody {
    account_id: String,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    max_concurrency: Option<i64>,
    /// provider 专属凭据字段(refresh_token 等),原样存为 extra JSON。
    #[serde(default)]
    extra: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAccountBody {
    #[serde(default)]
    group_name: Option<String>,
    #[serde(default)]
    max_concurrency: Option<i64>,
    #[serde(default)]
    disabled: Option<bool>,
    /// 整体替换 extra(凭据轮换);缺省不动。
    #[serde(default)]
    extra: Option<serde_json::Map<String, serde_json::Value>>,
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/accounts", get(list_accounts).post(create_account))
        .route("/accounts/runtime", get(runtime))
        .route("/accounts/{id}", patch(update_account).delete(delete_account))
}

fn api_error(status: StatusCode, msg: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({"type":"error","error":{"message": msg}})),
    )
        .into_response()
}

/// 把 AccountRow 转为对外视图:extra 解析成对象并把含 token/secret/password
/// 的字段脱敏(保尾 4 位)。凭据只进不出——admin 页展示概要即可,完整值
/// 留在库里供 worker 用。
fn redacted_view(row: AccountRow) -> serde_json::Value {
    let extra: serde_json::Value =
        serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);
    let extra = match extra {
        serde_json::Value::Object(map) => {
            let redacted: serde_json::Map<String, serde_json::Value> = map
                .into_iter()
                .map(|(k, v)| {
                    let lk = k.to_lowercase();
                    let sensitive =
                        lk.contains("token") || lk.contains("secret") || lk.contains("password");
                    let v = if sensitive {
                        match v.as_str() {
                            Some(s) if s.len() > 6 => {
                                serde_json::json!(format!("***{}", &s[s.len() - 4..]))
                            }
                            _ => serde_json::json!("***"),
                        }
                    } else {
                        v
                    };
                    (k, v)
                })
                .collect();
            serde_json::Value::Object(redacted)
        }
        other => other,
    };
    serde_json::json!({
        "account_id": row.account_id,
        "group_name": row.group_name,
        "provider": row.provider,
        "max_concurrency": row.max_concurrency,
        "disabled": row.disabled,
        "extra": extra,
        "created_at": row.created_at,
    })
}

async fn list_accounts(State(st): State<AdminState>) -> axum::response::Response {
    match st.store.list_accounts() {
        Ok(rows) => Json(rows.into_iter().map(redacted_view).collect::<Vec<_>>()).into_response(),
        Err(e) => internal_error(e),
    }
}

async fn create_account(
    State(st): State<AdminState>,
    Json(body): Json<CreateAccountBody>,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&body.account_id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    let extra_json = match &body.extra {
        Some(map) => match serde_json::to_string(map) {
            Ok(s) => s,
            Err(e) => return internal_error(e),
        },
        None => "{}".to_string(),
    };
    let group = body.group.as_deref().unwrap_or("");
    let provider = body.provider.as_deref().filter(|p| !p.is_empty()).unwrap_or("kiro");
    let conc = body.max_concurrency.unwrap_or(1);
    match st
        .store
        .create_account(&body.account_id, group, provider, conc, &extra_json)
    {
        Ok(true) => match st.store.get_account(&body.account_id) {
            Ok(Some(row)) => (StatusCode::CREATED, Json(redacted_view(row))).into_response(),
            Ok(None) => internal_error("创建后读取不到账号"),
            Err(e) => internal_error(e),
        },
        Ok(false) => api_error(StatusCode::CONFLICT, "account_id 已存在"),
        Err(e) => internal_error(e),
    }
}

async fn update_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAccountBody>,
) -> axum::response::Response {
    let extra = match &body.extra {
        Some(map) => match serde_json::to_string(map) {
            Ok(s) => Some(s),
            Err(e) => return internal_error(e),
        },
        None => None,
    };
    let patch = AccountPatch {
        group_name: body.group_name.clone(),
        max_concurrency: body.max_concurrency,
        disabled: body.disabled,
        extra,
    };
    match st.store.update_account(&id, &patch) {
        Ok(true) => match st.store.get_account(&id) {
            Ok(Some(row)) => Json(redacted_view(row)).into_response(),
            Ok(None) => internal_error("更新后读取不到账号"),
            Err(e) => internal_error(e),
        },
        Ok(false) => api_error(StatusCode::NOT_FOUND, "账号不存在"),
        Err(e) => internal_error(e),
    }
}

async fn delete_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match st.store.delete_account(&id) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "账号不存在"),
        Err(e) => internal_error(e),
    }
}

/// 运行态聚合:逐 worker 拉 `/health`(2s 超时),离线 worker 标 `online:false`。
/// 串行拉取(worker 数通常 ≤ 个位数,简单优先;多了再并发)。
async fn runtime(State(st): State<AdminState>) -> axum::response::Response {
    let mut out = Vec::with_capacity(st.workers.len());
    for w in st.workers.iter() {
        let url = format!("http://{}/health", w.listen);
        let entry = match st.http.get(&url).send().await {
            Ok(resp) => match resp.json::<serde_json::Value>().await {
                Ok(v) => serde_json::json!({
                    "instance": w.instance,
                    "group": w.account_group,
                    "online": true,
                    "accounts_status": v.get("accounts_status").cloned()
                        .unwrap_or(serde_json::Value::Array(vec![])),
                }),
                Err(e) => {
                    tracing::warn!(instance = w.instance, "worker /health 响应解析失败: {e}");
                    serde_json::json!({
                        "instance": w.instance, "group": w.account_group, "online": false,
                    })
                }
            },
            Err(e) => {
                tracing::debug!(instance = w.instance, "worker 不在线: {e}");
                serde_json::json!({
                    "instance": w.instance, "group": w.account_group, "online": false,
                })
            }
        };
        out.push(entry);
    }
    Json(out).into_response()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::admin::tests_support::{app, req};

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn accounts_require_admin_token() {
        let (app, _) = app();
        let r = Request::builder()
            .method("GET")
            .uri("/accounts")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(r).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn account_crud_roundtrip_with_redaction() {
        let (app, store) = app();
        // 创建(带敏感 extra)。
        let body = r#"{"account_id":"kiro-01","group":"G0","max_concurrency":2,
            "extra":{"refresh_token":"rt-secret-12345678","region":"us-east-1"}}"#;
        let resp = app.clone().oneshot(req("POST", "/accounts", Some(body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = json_body(resp).await;
        assert_eq!(v["account_id"], "kiro-01");
        assert_eq!(v["group_name"], "G0");
        assert_eq!(v["provider"], "kiro", "缺省 provider = kiro");
        let rt = v["extra"]["refresh_token"].as_str().unwrap();
        assert!(!rt.contains("rt-secret"), "refresh_token 必须脱敏,实际 {rt}");
        assert!(rt.ends_with("5678"), "保尾 4 位便于识别");
        assert_eq!(v["extra"]["region"], "us-east-1", "非敏感字段原样");
        // 库里存的是完整值(worker 要用)。
        let raw = store.get_account("kiro-01").unwrap().unwrap();
        assert!(raw.extra.contains("rt-secret-12345678"));

        // 重复 409;非法 id 400。
        let resp = app.clone().oneshot(req("POST", "/accounts", Some(body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let resp = app
            .clone()
            .oneshot(req("POST", "/accounts", Some(r#"{"account_id":"bad id/x"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // 列表也脱敏。
        let resp = app.clone().oneshot(req("GET", "/accounts", None)).await.unwrap();
        let v = json_body(resp).await;
        assert!(!v[0]["extra"]["refresh_token"].as_str().unwrap().contains("rt-secret"));

        // PATCH:禁用 + 换组,extra 不动。
        let resp = app
            .clone()
            .oneshot(req(
                "PATCH",
                "/accounts/kiro-01",
                Some(r#"{"disabled":true,"group_name":"G1"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["disabled"], true);
        assert_eq!(v["group_name"], "G1");
        let raw = store.get_account("kiro-01").unwrap().unwrap();
        assert!(raw.extra.contains("rt-secret-12345678"), "PATCH 未传 extra 不得改凭据");

        // PATCH 不存在 404;删除;二次删除 404。
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/accounts/ghost", Some(r#"{"disabled":true}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = app.clone().oneshot(req("DELETE", "/accounts/kiro-01", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = app.oneshot(req("DELETE", "/accounts/kiro-01", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn runtime_with_no_workers_returns_empty() {
        let (app, _) = app();
        let resp = app.oneshot(req("GET", "/accounts/runtime", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v.as_array().unwrap().len(), 0);
    }
}
