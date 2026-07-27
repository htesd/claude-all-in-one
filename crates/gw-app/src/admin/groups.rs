//! admin 分组管理端点(切片③)。
//!
//! 分组 = **成员集合**:哪些账号可以被本组的客户用,以及每个账号**在本组里**排第几。
//! 成员边存在 `account_groups`(N:M),与 `accounts.group_name`(账号归哪个 worker 独占
//! 管理)是两件不同的事 —— 前者是权限与排序,后者是物理归属。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use gw_store::DeleteGroupOutcome;
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

/// 加/改一条成员边。
#[derive(Debug, Deserialize)]
pub struct AddMemberBody {
    account_id: String,
    /// 该号**在这个组里**的优先级(数值越小越优先;不传 = 100 兜底层)。
    /// 同一个号在别的组里可以是另一个值 —— 这正是本模型的意义所在。
    #[serde(default = "default_member_priority")]
    priority: i64,
}

/// 按条件批量加成员(220 个号手工点不现实)。
#[derive(Debug, Deserialize)]
pub struct BulkAddBody {
    /// 只加归属于该 owner 的号(不传 = 不限)。
    #[serde(default)]
    owner: Option<String>,
    /// 只加该订阅档位的号,如 "KIRO POWER" / "KIRO PRO MAX"(不传 = 不限)。
    #[serde(default)]
    subscription_title: Option<String>,
    #[serde(default = "default_member_priority")]
    priority: i64,
}

fn default_member_priority() -> i64 {
    100
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/groups", get(list_groups).post(create_group))
        .route("/groups/{name}", patch(update_group).delete(delete_group))
        .route("/groups/{name}/members", get(list_members).post(add_member))
        .route("/groups/{name}/members/bulk", post(bulk_add_members))
        .route("/groups/{name}/members/{account_id}", delete(remove_member))
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
    match st.store.update_group(&name, body.color.as_deref(), body.note.as_deref()) {
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
        Ok(DeleteGroupOutcome::Deleted) => StatusCode::NO_CONTENT.into_response(),
        Ok(DeleteGroupOutcome::NotFound) => api_error(StatusCode::NOT_FOUND, "分组不存在"),
        // 删组会把 key 的 group_name 清空,而 router 把空组名回落到主组 ——
        // 这些客户当场变成主组的不受限访问,且无任何告警。
        Ok(DeleteGroupOutcome::HasKeys(n)) => api_error(
            StatusCode::CONFLICT,
            &format!(
                "本组仍有 {n} 把 key 绑定,直接删除会让这些客户回落到主组、拿到全部账号。\
                 下线请改成清空本组成员(该组随即 503),或先把 key 迁走/禁用"
            ),
        ),
        Err(e) => internal_error(e),
    }
}

// ───────── 成员边(账号↔分组 N:M) ─────────

async fn list_members(
    State(st): State<AdminState>,
    Path(name): Path<String>,
) -> axum::response::Response {
    match st.store.list_group_members(&name) {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|(account_id, priority)| {
                    serde_json::json!({"account_id": account_id, "priority": priority})
                })
                .collect::<Vec<_>>(),
        )
        .into_response(),
        Err(e) => internal_error(e),
    }
}

async fn add_member(
    State(st): State<AdminState>,
    Path(name): Path<String>,
    Json(body): Json<AddMemberBody>,
) -> axum::response::Response {
    match st.store.upsert_membership(&body.account_id, &name, body.priority) {
        // 悬空边不会报错、只会让该组静默少一个号,所以写入侧就要挡住。
        Ok(false) => api_error(StatusCode::NOT_FOUND, "账号或分组不存在"),
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal_error(e),
    }
}

async fn remove_member(
    State(st): State<AdminState>,
    Path((name, account_id)): Path<(String, String)>,
) -> axum::response::Response {
    match st.store.remove_membership(&account_id, &name) {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => api_error(StatusCode::NOT_FOUND, "该账号不在本组"),
        Err(e) => internal_error(e),
    }
}

async fn bulk_add_members(
    State(st): State<AdminState>,
    Path(name): Path<String>,
    Json(body): Json<BulkAddBody>,
) -> axum::response::Response {
    match st.store.bulk_add_members(
        &name,
        body.owner.as_deref(),
        body.subscription_title.as_deref(),
        body.priority,
    ) {
        Ok(n) => Json(serde_json::json!({"added_or_updated": n})).into_response(),
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
        assert_eq!(app.oneshot(r).await.unwrap().status(), StatusCode::UNAUTHORIZED);
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
        store.create_api_key("sk-in-g0-1", None, Some("G0")).unwrap();
        let resp = app.clone().oneshot(req("GET", "/groups", None)).await.unwrap();
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

        // 仍有 key 绑定 → 409(删了这些客户会回落主组、拿到全部账号,见 delete_group)。
        let resp = app.clone().oneshot(req("DELETE", "/groups/G0", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);

        // key 全部移出后才允许删。
        for k in ["sk-in-g0-1", "sk-in-g0-2"] {
            if store.get_api_key(k).unwrap().is_some() {
                store
                    .update_api_key(
                        k,
                        &gw_core::store::ApiKeyPatch {
                            group_name: Some(String::new()),
                            ..Default::default()
                        },
                    )
                    .unwrap();
            }
        }
        let resp = app.clone().oneshot(req("DELETE", "/groups/G0", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(store.get_api_key("sk-in-g0-1").unwrap().unwrap().group_name, "");
        let resp = app.oneshot(req("DELETE", "/groups/G0", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ───────── 成员边(账号↔分组 N:M) ─────────

    /// 成员边的完整生命周期:加 → 改优先级 → 列 → 删。
    #[tokio::test]
    async fn member_crud_roundtrip() {
        let (app, store) = app();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#))).await.unwrap();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"LOW"}"#))).await.unwrap();
        store.create_account("promax", "G0", "kiro", 2, "{}").unwrap();

        // 加成员:低价组里让小号当主力(0)。
        let resp = app
            .clone()
            .oneshot(req("POST", "/groups/LOW/members", Some(r#"{"account_id":"promax","priority":0}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(store.list_group_members("LOW").unwrap(), vec![("promax".to_string(), 0)]);

        // 重复 POST = 改组内优先级(幂等 upsert),不是报冲突。
        app.clone()
            .oneshot(req("POST", "/groups/LOW/members", Some(r#"{"account_id":"promax","priority":50}"#)))
            .await
            .unwrap();
        let resp = app.clone().oneshot(req("GET", "/groups/LOW/members", None)).await.unwrap();
        let v = json_body(resp).await;
        assert_eq!(v[0]["account_id"], "promax");
        assert_eq!(v[0]["priority"], 50);

        // 同一个号在归属组 G0 里仍是它自己的优先级 —— 两条边互不干扰。
        assert_eq!(store.list_group_members("G0").unwrap(), vec![("promax".to_string(), 100)]);

        let resp = app
            .clone()
            .oneshot(req("DELETE", "/groups/LOW/members/promax", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert!(store.list_group_members("LOW").unwrap().is_empty());
        let resp = app.oneshot(req("DELETE", "/groups/LOW/members/promax", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND, "重复删应报 404");
    }

    /// 悬空边必须在写入侧挡住:它不会报错,只会让该组静默少一个号 —— 比立即失败难查得多。
    #[tokio::test]
    async fn member_rejects_dangling_refs() {
        let (app, store) = app();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#))).await.unwrap();
        store.create_account("a", "G0", "kiro", 2, "{}").unwrap();

        for (path, body, why) in [
            ("/groups/G0/members", r#"{"account_id":"ghost"}"#, "账号不存在"),
            ("/groups/NOPE/members", r#"{"account_id":"a"}"#, "分组不存在"),
        ] {
            let resp = app.clone().oneshot(req("POST", path, Some(body))).await.unwrap();
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "应拒绝:{why}");
        }
    }

    /// 批量按条件加成员(220 个号手工点不现实),且筛选维度要真的生效。
    #[tokio::test]
    async fn bulk_add_members_filters() {
        let (app, store) = app();
        for g in ["G0", "LOW"] {
            app.clone()
                .oneshot(req("POST", "/groups", Some(&format!(r#"{{"name":"{g}"}}"#))))
                .await
                .unwrap();
        }
        store
            .create_account("p1", "G0", "kiro", 2, r#"{"subscription_title":"KIRO POWER"}"#)
            .unwrap();
        store
            .create_account("m1", "G0", "kiro", 2, r#"{"subscription_title":"KIRO PRO MAX"}"#)
            .unwrap();

        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/groups/LOW/members/bulk",
                Some(r#"{"subscription_title":"KIRO PRO MAX","priority":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(json_body(resp).await["added_or_updated"], 1);
        assert_eq!(
            store.list_group_members("LOW").unwrap(),
            vec![("m1".to_string(), 0)],
            "只该加进 PRO MAX,POWER 主力号不得被带进低价组"
        );
    }

    /// 仍有 key 绑定的组**不得删除**:删组会把 key 的 group_name 清空,而 router 把空组名
    /// 回落到主组 —— 这些客户当场拿到全部账号,且无任何告警。
    #[tokio::test]
    async fn delete_group_with_bound_keys_conflicts() {
        let (app, store) = app();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"LOW"}"#))).await.unwrap();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#))).await.unwrap();
        store.create_api_key("sk-low", None, Some("LOW")).unwrap();

        let resp = app.clone().oneshot(req("DELETE", "/groups/LOW", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        assert_eq!(
            store.get_api_key("sk-low").unwrap().unwrap().group_name,
            "LOW",
            "被拒的删除不得留下任何痕迹"
        );

        // key 迁走后可以删 —— 保护不是永久锁死。
        store
            .update_api_key(
                "sk-low",
                &gw_core::store::ApiKeyPatch {
                    group_name: Some("G0".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let resp = app.oneshot(req("DELETE", "/groups/LOW", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    }
}
