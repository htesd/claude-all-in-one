//! admin API key 管理端点(切片②:按客户计费的控制面)。
//!
//! - `GET    /keys`        全量列表(created_at 倒序;明文 key——admin 是发放方)
//! - `POST   /keys`        新建(缺 key 则服务端生成 `sk-gw-<32hex>`;重复 409)
//! - `PATCH  /keys/{key}`  部分更新 label/disabled(缺省字段不动;404 = 不存在)
//! - `DELETE /keys/{key}`  删除(usage 历史归属保留;404 = 不存在)
//!
//! 禁用/删除立即生效:router 每次请求都查库鉴权,无缓存。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde::Deserialize;

use super::{internal_error, AdminState};

/// 自定义 key 的合法长度区间(过短易撞/易猜,过长防滥用)。
const KEY_LEN: std::ops::RangeInclusive<usize> = 8..=128;

#[derive(Debug, Deserialize)]
pub struct CreateKeyBody {
    /// 备注(客户名等)。空串视同未填。
    #[serde(default)]
    label: Option<String>,
    /// 自定义 key(迁移已有 key 用);缺省 = 服务端生成。
    #[serde(default)]
    key: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateKeyBody {
    /// `None` = 不改;`Some("")` = 清空备注。
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    disabled: Option<bool>,
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/keys", get(list_keys).post(create_key))
        .route("/keys/{key}", patch(update_key).delete(delete_key))
}

/// 业务错误响应(400/404/409 等,消息可安全外露)。
fn api_error(status: StatusCode, msg: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({"type":"error","error":{"message": msg}})),
    )
        .into_response()
}

/// 校验自定义 key:8–128 个 ASCII 可见字符(无空格/控制符/非 ASCII)。
fn validate_custom_key(key: &str) -> Result<(), &'static str> {
    if !KEY_LEN.contains(&key.len()) {
        return Err("key 长度须在 8–128 字符之间");
    }
    if !key.bytes().all(|b| b.is_ascii_graphic()) {
        return Err("key 只能含 ASCII 可见字符(不含空格)");
    }
    Ok(())
}

async fn list_keys(State(st): State<AdminState>) -> axum::response::Response {
    match st.store.list_api_keys() {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal_error(e),
    }
}

async fn create_key(
    State(st): State<AdminState>,
    Json(body): Json<CreateKeyBody>,
) -> axum::response::Response {
    let key = match &body.key {
        Some(k) => {
            if let Err(msg) = validate_custom_key(k) {
                return api_error(StatusCode::BAD_REQUEST, msg);
            }
            k.clone()
        }
        // 128 bit CSPRNG(uuid v4 经 getrandom),足以抗在线猜测。
        None => format!("sk-gw-{}", uuid::Uuid::new_v4().simple()),
    };
    let label = body.label.as_deref().filter(|s| !s.is_empty());
    match st.store.create_api_key(&key, label) {
        Ok(true) => match st.store.get_api_key(&key) {
            Ok(Some(row)) => (StatusCode::CREATED, Json(row)).into_response(),
            Ok(None) => internal_error("创建后读取不到 key"),
            Err(e) => internal_error(e),
        },
        Ok(false) => api_error(StatusCode::CONFLICT, "key 已存在"),
        Err(e) => internal_error(e),
    }
}

async fn update_key(
    State(st): State<AdminState>,
    Path(key): Path<String>,
    Json(body): Json<UpdateKeyBody>,
) -> axum::response::Response {
    match st
        .store
        .update_api_key(&key, body.label.as_deref(), body.disabled)
    {
        Ok(true) => match st.store.get_api_key(&key) {
            Ok(Some(row)) => Json(row).into_response(),
            Ok(None) => internal_error("更新后读取不到 key"),
            Err(e) => internal_error(e),
        },
        Ok(false) => api_error(StatusCode::NOT_FOUND, "key 不存在"),
        Err(e) => internal_error(e),
    }
}

async fn delete_key(
    State(st): State<AdminState>,
    Path(key): Path<String>,
) -> axum::response::Response {
    match st.store.delete_api_key(&key) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "key 不存在"),
        Err(e) => internal_error(e),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use gw_store::SqliteStore;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::admin::{admin_api_router, AdminState};

    const TOKEN: &str = "admt-test";

    fn app() -> (axum::Router, Arc<SqliteStore>) {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let st = AdminState {
            token: Arc::new(TOKEN.to_string()),
            store: store.clone(),
        };
        (admin_api_router(st), store)
    }

    fn req(method: &str, uri: &str, body: Option<&str>) -> Request<Body> {
        let b = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-api-key", TOKEN);
        match body {
            Some(s) => b
                .header("content-type", "application/json")
                .body(Body::from(s.to_string()))
                .unwrap(),
            None => b.body(Body::empty()).unwrap(),
        }
    }

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn keys_routes_require_admin_token() {
        let (app, _) = app();
        let r = Request::builder()
            .method("GET")
            .uri("/keys")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(r).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn list_keys_returns_seeded_rows() {
        let (app, store) = app();
        store.create_api_key("sk-alice-prod-7788", Some("alice")).unwrap();
        let resp = app.oneshot(req("GET", "/keys", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["key"], "sk-alice-prod-7788");
        assert_eq!(arr[0]["label"], "alice");
        assert_eq!(arr[0]["disabled"], false);
        assert!(arr[0]["created_at"].as_i64().unwrap() > 0);
    }

    #[tokio::test]
    async fn create_generates_key_when_absent() {
        let (app, store) = app();
        let resp = app
            .oneshot(req("POST", "/keys", Some(r#"{"label":"新客户"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = json_body(resp).await;
        let key = v["key"].as_str().unwrap();
        assert!(key.starts_with("sk-gw-"), "服务端生成前缀 sk-gw-,实际 {key}");
        assert_eq!(key.len(), "sk-gw-".len() + 32, "uuid simple = 32 hex");
        assert_eq!(v["label"], "新客户");
        // 生成的 key 立即可用于鉴权。
        use gw_core::store::ControlStore;
        assert!(store.authenticate(key).await.unwrap().is_some());
    }

    #[tokio::test]
    async fn create_custom_key_then_conflict() {
        let (app, _) = app();
        let body = r#"{"key":"sk-custom-12345678","label":null}"#;
        let resp = app.clone().oneshot(req("POST", "/keys", Some(body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = json_body(resp).await;
        assert_eq!(v["key"], "sk-custom-12345678");
        assert_eq!(v["label"], serde_json::Value::Null, "空 label 落 NULL");

        let resp = app.oneshot(req("POST", "/keys", Some(body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT, "重复 key 应 409");
    }

    #[tokio::test]
    async fn create_rejects_invalid_custom_key() {
        let (app, _) = app();
        for bad in [
            r#"{"key":"short"}"#,                  // 太短
            r#"{"key":"sk has space"}"#,           // 含空格
            r#"{"key":"sk-中文键-8888"}"#,          // 非 ASCII
        ] {
            let resp = app.clone().oneshot(req("POST", "/keys", Some(bad))).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "应拒绝 {bad}");
        }
        // 超长(>128)。
        let long = format!(r#"{{"key":"sk-{}"}}"#, "x".repeat(130));
        let resp = app.oneshot(req("POST", "/keys", Some(&long))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn patch_updates_fields_and_404s() {
        let (app, store) = app();
        store.create_api_key("sk-patch-me-1", Some("旧备注")).unwrap();

        // 禁用,label 不动。
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/keys/sk-patch-me-1", Some(r#"{"disabled":true}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["disabled"], true);
        assert_eq!(v["label"], "旧备注");

        // 清空备注("")。
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/keys/sk-patch-me-1", Some(r#"{"label":""}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["label"], "");
        assert_eq!(v["disabled"], true, "改 label 不得动 disabled");

        // 不存在 → 404。
        let resp = app
            .oneshot(req("PATCH", "/keys/sk-ghost", Some(r#"{"disabled":true}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn delete_then_404() {
        let (app, store) = app();
        store.create_api_key("sk-del-me-99", None).unwrap();
        let resp = app
            .clone()
            .oneshot(req("DELETE", "/keys/sk-del-me-99", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = app.oneshot(req("DELETE", "/keys/sk-del-me-99", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
