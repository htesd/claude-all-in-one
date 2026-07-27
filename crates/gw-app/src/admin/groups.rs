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
    /// 影子组:源组名(不传/空 = 普通组)。见 `gw_core::store::TierPolicy`。
    #[serde(default)]
    shadow_of: Option<String>,
    /// 影子组可见档位的**下界**(只允许 priority >= 此值;不传 = 不限)。
    /// 数值越小越优先,所以这是"把主力号挡在档位外"的那一侧。
    #[serde(default)]
    tier_min_priority: Option<i64>,
    /// 影子组可见的最高档位(只允许 priority <= 此值;不传 = 不限)。
    #[serde(default)]
    tier_max_priority: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateGroupBody {
    #[serde(default)]
    color: Option<String>,
    #[serde(default)]
    note: Option<String>,
    /// `Some("")` = 取消影子身份 —— **这是低价档的正确下线姿势**(该组随即无 worker
    /// → 全 503,而 key 仍留在组内)。用 DELETE 会把 key 打回未分组、回落主组,
    /// 等于静默提权,见 `SqliteStore::delete_group`。
    #[serde(default)]
    shadow_of: Option<String>,
    /// 双层 Option:外层 None = 不动;`Some(None)` = 清除档位下界(不限)。
    #[serde(default, deserialize_with = "crate::admin::double_option")]
    tier_min_priority: Option<Option<i64>>,
    /// 双层 Option:外层 None = 不动;`Some(None)` = 清除档位上限(不限)。
    #[serde(default, deserialize_with = "crate::admin::double_option")]
    tier_max_priority: Option<Option<i64>>,
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

/// 影子组配置校验(create / update 共用)。返回 `Err(消息)` → 400。
///
/// 这些非法状态都会**静默**变成线上事故(路由到没有 worker 的组 = 整组 503,
/// 或账号进了没有任何 worker 加载的组 = 幽灵号),所以在写入侧一次拦掉。
fn validate_shadow(
    st: &AdminState,
    name: &str,
    shadow_of: &str,
    tier_min_priority: Option<i64>,
    tier_max_priority: Option<i64>,
) -> Result<(), String> {
    let rows = st.store.list_groups().map_err(|e| e.to_string())?;
    if shadow_of.is_empty() {
        // 普通组不得携带档位字段:留着会给运维"已经限住了"的错觉,而它根本不生效。
        if tier_min_priority.is_some() || tier_max_priority.is_some() {
            return Err("tier_min_priority/tier_max_priority 仅对影子组有效,请先设置 shadow_of".into());
        }
        return Ok(());
    }
    // 空区间 = 该组永远选不出账号 = 整组 503,且现象和"源组挂了"完全一样,极难定位。
    // 写入侧一次拦掉,别让它变成线上事故。
    if let (Some(lo), Some(hi)) = (tier_min_priority, tier_max_priority) {
        if lo > hi {
            return Err(format!(
                "档位区间为空(min={lo} > max={hi}):该组任何请求都选不出账号,会整组 503"
            ));
        }
    }
    if shadow_of == name {
        return Err("影子组不能指向自己(会路由到没有 worker 的组)".into());
    }
    match rows.iter().find(|g| g.name == shadow_of) {
        None => return Err("源组不存在".into()),
        // 只解一层:链式影子会静默变成"路由到一个同样没有 worker 的组"。
        Some(g) if !g.shadow_of.is_empty() => {
            return Err("源组本身是影子组,不支持链式影子".into())
        }
        Some(_) => {}
    }
    // 影子组没有自己的 worker,分进去的账号永远不会被任何 scheduler 加载(幽灵号)。
    if rows.iter().any(|g| g.name == name && g.account_count > 0) {
        return Err("本组已有账号,不能改为影子组;影子组不持有账号".into());
    }
    // 把一个**已有客户在用**的普通组就地转成影子组,会让那些客户下一个请求就被套上档位
    // 限制、改路由到源组 —— 是一次**无声的降级**(丢掉低优兜底层),且不像"账号变幽灵号"
    // 那样有迹可循,只能等客户投诉。与 `delete_group` 的 `ShadowHasKeys` 同一条原则:
    // 影响到既有 key 的分组语义变更必须显式,不能当作改配置的副作用发生。
    //
    // 注意这条只挡"普通组 → 影子组";反方向(`shadow_of = ""`,低价档下线)在函数开头
    // 就 return 了,不受影响 —— 那是解除限制,也是文档里的标准回退姿势。
    if let Some(n) = rows
        .iter()
        .find(|g| g.name == name && g.key_count > 0)
        .map(|g| g.key_count)
    {
        return Err(format!(
            "本组仍有 {n} 把 key 在用,就地转成影子组会让这些客户无声地被降级到受限档位;\
             请先把 key 移到别的组,或新建影子组再迁移"
        ));
    }
    Ok(())
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
    let shadow_of = body.shadow_of.as_deref().unwrap_or("").trim();
    if let Err(msg) = validate_shadow(
        &st,
        &body.name,
        shadow_of,
        body.tier_min_priority,
        body.tier_max_priority,
    ) {
        return api_error(StatusCode::BAD_REQUEST, &msg);
    }
    match st.store.create_group(
        &body.name,
        color,
        note,
        shadow_of,
        body.tier_min_priority,
        body.tier_max_priority,
    ) {
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
    // 影子字段被触碰时才校验,且必须按**打完补丁后的有效值**校验(只改 note 的 PATCH
    // 不该因为历史配置被拒)。
    let shadow_of = body.shadow_of.as_deref().map(str::trim);
    let mut tier_min_priority = body.tier_min_priority;
    let mut tier_max_priority = body.tier_max_priority;
    if shadow_of.is_some() || tier_min_priority.is_some() || tier_max_priority.is_some() {
        let cur = match st.store.list_groups() {
            Ok(rows) => rows.into_iter().find(|g| g.name == name),
            Err(e) => return internal_error(e),
        };
        let Some(cur) = cur else {
            return api_error(StatusCode::NOT_FOUND, "分组不存在");
        };
        let eff_shadow = shadow_of.unwrap_or(&cur.shadow_of);
        // 摘掉影子身份(低价档下线)时,顺手把只对影子组有意义的档位字段一并清掉。
        // 否则"取消影子"会被下面"普通组不得带档位字段"的校验挡住,运维必须记得同时
        // 传 tier_*_priority:null 才能下线 —— 应急路径上不该有这种隐含步骤。
        if eff_shadow.is_empty() {
            if cur.tier_min_priority.is_some() {
                tier_min_priority = Some(None);
            }
            if cur.tier_max_priority.is_some() {
                tier_max_priority = Some(None);
            }
        }
        let eff_min = tier_min_priority.unwrap_or(cur.tier_min_priority);
        let eff_max = tier_max_priority.unwrap_or(cur.tier_max_priority);
        if let Err(msg) = validate_shadow(&st, &name, eff_shadow, eff_min, eff_max) {
            return api_error(StatusCode::BAD_REQUEST, &msg);
        }
    }
    match st.store.update_group(
        &name,
        body.color.as_deref(),
        body.note.as_deref(),
        shadow_of,
        tier_min_priority,
        tier_max_priority,
    ) {
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
        Ok(DeleteGroupOutcome::HasShadowChildren(kids)) => api_error(
            StatusCode::CONFLICT,
            &format!(
                "影子组 {} 以本组为源组,删除会让它们路由到不存在的组;请先处理这些影子组",
                kids.join("、")
            ),
        ),
        // 删影子组会把它的 key 打回"未分组",而未分组会被 router 回落到主组 ——
        // 低价客户当场变成不受档位限制的主组客户,且无任何告警。
        Ok(DeleteGroupOutcome::ShadowHasKeys(n)) => api_error(
            StatusCode::CONFLICT,
            &format!(
                "本影子组仍有 {n} 把 key 绑定,直接删除会让这些客户回落到主组、失去档位限制。\
                 下线请改用 PATCH {{\"shadow_of\":\"\"}},或先把 key 移出/禁用"
            ),
        ),
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

        // 删除:key 的 group_name 被清空。
        let resp = app.clone().oneshot(req("DELETE", "/groups/G0", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        assert_eq!(store.get_api_key("sk-in-g0-1").unwrap().unwrap().group_name, "");
        let resp = app.oneshot(req("DELETE", "/groups/G0", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    // ───────── 影子组(低价档)配置校验 ─────────

    /// 建一个正常的影子组,并确认响应带出影子字段(前端要据此显示徽章)。
    #[tokio::test]
    async fn create_shadow_group_roundtrip() {
        let (app, _) = app();
        app.clone()
            .oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#)))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/groups",
                Some(r#"{"name":"GLOW","shadow_of":"G0","tier_max_priority":0}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = json_body(resp).await;
        assert_eq!(v["shadow_of"], "G0");
        assert_eq!(v["tier_max_priority"], 0);
    }

    /// 每一种非法影子配置都会**静默**变成线上事故(路由到没有 worker 的组 = 整组 503),
    /// 所以必须在写入侧一次拦掉。
    #[tokio::test]
    async fn create_shadow_validates() {
        let (app, _) = app();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#))).await.unwrap();
        app.clone()
            .oneshot(req("POST", "/groups", Some(r#"{"name":"GLOW","shadow_of":"G0"}"#)))
            .await
            .unwrap();

        for (body, why) in [
            (r#"{"name":"X","shadow_of":"NOPE"}"#, "源组不存在"),
            (r#"{"name":"X","shadow_of":"X"}"#, "自指(会路由到没有 worker 的组)"),
            (r#"{"name":"X","shadow_of":"GLOW"}"#, "链式影子(router 只解一层)"),
            (r#"{"name":"X","tier_max_priority":0}"#, "普通组带档位字段(不生效却给人错觉)"),
            (r#"{"name":"X","tier_min_priority":1}"#, "普通组带下界字段(同上)"),
            (
                r#"{"name":"X","shadow_of":"G0","tier_min_priority":10,"tier_max_priority":0}"#,
                "空区间 min>max(该组任何请求都选不出号 = 整组 503,现象与源组挂掉无法区分)",
            ),
        ] {
            let resp = app.clone().oneshot(req("POST", "/groups", Some(body))).await.unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "应拒绝:{why}");
        }
    }

    /// "只要小号"的低价档:只给下界。这是保护高价用户主力号的那一档。
    #[tokio::test]
    async fn create_low_tier_with_min_bound_only() {
        let (app, _) = app();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#))).await.unwrap();
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/groups",
                Some(r#"{"name":"GECO","shadow_of":"G0","tier_min_priority":1}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = json_body(resp).await;
        assert_eq!(v["tier_min_priority"], 1);
        assert!(v["tier_max_priority"].is_null(), "只给下界时上界必须保持不限");

        // 相等端点是合法闭区间(只放行恰好那一档),不能被空区间校验误伤。
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/groups",
                Some(r#"{"name":"GONE","shadow_of":"G0","tier_min_priority":100,"tier_max_priority":100}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "min == max 是单档位,合法");
    }

    /// 摘掉影子身份时必须把**两个**档位字段一起清掉,否则残留的下界会被
    /// "普通组不得带档位字段"挡住 —— 应急下线路径上不该有隐含步骤。
    #[tokio::test]
    async fn detach_clears_both_tier_bounds() {
        let (app, _) = app();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#))).await.unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                "/groups",
                Some(r#"{"name":"GECO","shadow_of":"G0","tier_min_priority":1,"tier_max_priority":90}"#),
            ))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/groups/GECO", Some(r#"{"shadow_of":""}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "下线路径必须一步走通");
        let v = json_body(resp).await;
        assert_eq!(v["shadow_of"], "");
        assert!(v["tier_min_priority"].is_null() && v["tier_max_priority"].is_null());
    }

    /// 下线低价档的正确姿势:PATCH shadow_of="" (该组随即无 worker → 全 503,
    /// key 仍留在组内)。DELETE 会让 key 回落主组 = 静默提权,单独由 store 层拦。
    #[tokio::test]
    async fn patch_can_detach_shadow() {
        let (app, _) = app();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#))).await.unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                "/groups",
                Some(r#"{"name":"GLOW","shadow_of":"G0","tier_max_priority":0}"#),
            ))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/groups/GLOW", Some(r#"{"shadow_of":""}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["shadow_of"], "");
        assert!(
            v["tier_max_priority"].is_null(),
            "摘掉影子身份时必须顺手清掉档位字段,否则下线要靠运维记住多传一个参数"
        );
    }

    /// 只改 note 的 PATCH 不该因为历史影子配置被拒(校验必须按打完补丁后的有效值)。
    #[tokio::test]
    async fn patch_note_only_keeps_shadow_config() {
        let (app, _) = app();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#))).await.unwrap();
        app.clone()
            .oneshot(req(
                "POST",
                "/groups",
                Some(r#"{"name":"GLOW","shadow_of":"G0","tier_max_priority":0}"#),
            ))
            .await
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/groups/GLOW", Some(r#"{"note":"低价档"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["shadow_of"], "G0", "只改 note 不得动影子配置");
        assert_eq!(v["tier_max_priority"], 0);
    }

    /// 把一个**仍有客户 key 在用**的普通组就地转成影子组 = 无声降级那批客户
    /// (丢掉低优兜底层、改路由)。与删组侧的 `ShadowHasKeys` 同一条原则,必须拒绝。
    #[tokio::test]
    async fn convert_group_with_bound_keys_to_shadow_is_rejected() {
        let (app, store) = app();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#))).await.unwrap();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"OTHER"}"#))).await.unwrap();
        store.add_api_key("sk-live", None).unwrap();
        store
            .update_api_key(
                "sk-live",
                &gw_core::store::ApiKeyPatch {
                    group_name: Some("G0".into()),
                    ..Default::default()
                },
            )
            .unwrap();

        let resp = app
            .clone()
            .oneshot(req("PATCH", "/groups/G0", Some(r#"{"shadow_of":"OTHER"}"#)))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "有客户在用的组不得被就地转成受限档位"
        );

        // 反向:key 移走之后允许转(保护不是永久锁死)。
        store
            .update_api_key(
                "sk-live",
                &gw_core::store::ApiKeyPatch {
                    group_name: Some("".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/groups/G0", Some(r#"{"shadow_of":"OTHER"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// 低价档下线(`shadow_of:""`)必须**不受**上面那条 key 守卫影响 ——
    /// 它是解除限制、且是文档写明的标准回退姿势,组里当然还有 key。
    #[tokio::test]
    async fn detach_shadow_is_allowed_even_with_bound_keys() {
        let (app, store) = app();
        app.clone().oneshot(req("POST", "/groups", Some(r#"{"name":"G0"}"#))).await.unwrap();
        app.clone()
            .oneshot(req("POST", "/groups", Some(r#"{"name":"GLOW","shadow_of":"G0"}"#)))
            .await
            .unwrap();
        store.add_api_key("sk-glow", None).unwrap();
        store
            .update_api_key(
                "sk-glow",
                &gw_core::store::ApiKeyPatch {
                    group_name: Some("GLOW".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/groups/GLOW", Some(r#"{"shadow_of":""}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "回退路径不得被 key 守卫挡住");
    }
}
