//! admin 分组管理端点(切片③)。
//!
//! 分组同时承担两种归类:上游账号组(对应 instances.yaml 的 account_group,
//! 决定 worker 服务哪些账号)与客户 key 的组织归类。删除组不级联删成员,
//! 只把成员的 group_name 清空(未分组)。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, patch};
use axum::{Json, Router};
use serde::Deserialize;

use super::{internal_error, AdminState};

/// 组名规则:1–64 个 URL-safe 字符(进 PATCH/DELETE 路径段,同 key 的约束)。
fn validate_group_name(name: &str) -> Result<(), &'static str> {
    if name.is_empty() || name.len() > 64 {
        return Err("组名长度须在 1–64 字符之间");
    }
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
    {
        return Err("组名只能含字母、数字及 - _ . ~");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct CreateGroupBody {
    name: String,
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateGroupBody {
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/{name}", patch(update_group).delete(delete_group))
}

fn api_error(status: StatusCode, msg: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({"type":"error","error":{"message": msg}})),
    )
        .into_response()
}

async fn list_groups(State(st): State<AdminState>) -> axum::response::Response {
    match st.store.list_groups() {
        Ok(rows) => Json(rows).into_response(),
        Err(e) => internal_error(e),
    }
}

async fn create_group(
    State(st): State<AdminState>,
    Json(body): Json<CreateGroupBody>,
) -> axum::response::Response {
    if let Err(msg) = validate_group_name(&body.name) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    let color = body.color.as_deref().unwrap_or("");
    let note = body.note.as_deref().unwrap_or("");
    if color.len() > 32 || note.len() > 200 {
        return api_error(StatusCode::BAD_REQUEST, "color/note 过长");
    }
    match st.store.create_group(&body.name, color, note) {
        Ok(true) => match st.store.list_groups() {
            Ok(rows) => match rows.into_iter().find(|g| g.name == body.name) {
                Some(g) => (StatusCode::CREATED, Json(g)).into_response(),
                None => internal_error("创建后读取不到分组"),
            },
            Err(e) => internal_error(e),
        },
        Ok(false) => api_error(StatusCode::CONFLICT, "分组已存在"),
        Err(e) => internal_error(e),
    }
}

async fn update_group(
    State(st): State<AdminState>,
    Path(name): Path<String>,
    Json(body): Json<UpdateGroupBody>,
) -> axum::response::Response {
    if body.color.as_deref().map(|c| c.len() > 32).unwrap_or(false)
        || body.note.as_deref().map(|n| n.len() > 200).unwrap_or(false)
    {
        return api_error(StatusCode::BAD_REQUEST, "color/note 过长");
    }
    match st
        .store
        .update_group(&name, body.color.as_deref(), body.note.as_deref())
    {
        Ok(true) => match st.store.list_groups() {
            Ok(rows) => match rows.into_iter().find(|g| g.name == name) {
                Some(g) => Json(g).into_response(),
                None => internal_error("更新后读取不到分组"),
            },
            Err(e) => internal_error(e),
        },
        Ok(false) => api_error(StatusCode::NOT_FOUND, "分组不存在"),
        Err(e) => internal_error(e),
    }
}

async fn delete_group(
    State(st): State<AdminState>,
    Path(name): Path<String>,
) -> axum::response::Response {
    match st.store.delete_group(&name) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "分组不存在"),
        Err(e) => internal_error(e),
    }
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
    async fn groups_require_admin_token() {
        let (app, _) = app();
        let r = Request::builder()
            .method("GET")
            .uri("/groups")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(r).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn group_crud_roundtrip() {
        let (app, store) = app();
        // 创建。
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/groups",
                Some(r##"{"name":"G0","color":"#7c6cf6","note":"主组"}"##),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = json_body(resp).await;
        assert_eq!(v["name"], "G0");
        assert_eq!(v["color"], "#7c6cf6");
        assert_eq!(v["account_count"], 0);

        // 重名 409。
        let resp = app
            .clone()
            .oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // 非法名 400。
        let resp = app
            .clone()
            .oneshot(req("POST", "/groups", Some(r#"{"name":"G 0/bad"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // 计数:挂 1 个 key。
        store
            .create_api_key("sk-in-g0-1", None, Some("G0"))
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req("GET", "/groups", None))
            .await
            .unwrap();
        let v = json_body(resp).await;
        assert_eq!(v[0]["key_count"], 1);

        // PATCH 部分更新。
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/groups/G0", Some(r#"{"note":"改备注"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["note"], "改备注");
        assert_eq!(v["color"], "#7c6cf6", "未传字段不动");

        // PATCH 不存在 404。
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/groups/GX", Some(r#"{"note":"x"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // 删除:key 的 group_name 被清空。
        let resp = app
            .clone()
            .oneshot(req("DELETE", "/groups/G0", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            store.get_api_key("sk-in-g0-1").unwrap().unwrap().group_name,
            ""
        );
        let resp = app
            .oneshot(req("DELETE", "/groups/G0", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }
}
