//! admin 系统设置端点 —— 热调参数面板的后端。
//!
//! 设置模型:`system.yaml`([`SystemConfig`])是不可变基线(改它需重启),DB `settings`
//! 表存**字段级 overlay**([`SystemSettings`],仅非 None 字段覆盖)。worker 30s 轮询 DB
//! 后热应用,无需重启。
//!
//! - `GET /settings`:返回**有效全量**(YAML 基线叠 DB overlay,每字段都有值),前端据此
//!   渲染当前生效值。
//! - `PUT /settings`:吃**部分** patch,`null` 字段 = 删该 overlay 回 YAML 默认、非 null =
//!   存 overlay。返回叠加后的新有效全量。
//!
//! 进程拓扑(端口/每 worker 源 IP)**不在此**(留 instances.yaml,需重启)。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use gw_core::config::{SystemConfig, SystemSettings};

use super::{internal_error, AdminState};

pub fn router() -> Router<AdminState> {
    Router::new().route("/settings", get(get_settings).put(put_settings))
}

/// 4xx 错误响应(请求体问题)。
fn api_error(status: StatusCode, msg: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({"type":"error","error":{"message": msg}})),
    )
        .into_response()
}

/// 由 DB overlay JSON 解析出 [`SystemSettings`](解析失败/无行 → 默认空 overlay)。
fn read_overlay(st: &AdminState) -> anyhow::Result<SystemSettings> {
    match st.store.get_settings()? {
        Some(j) => Ok(serde_json::from_str(&j).unwrap_or_default()),
        None => Ok(SystemSettings::default()),
    }
}

/// 把 overlay 叠到 YAML 基线得"有效"配置,再回灌成全量 [`SystemSettings`](含 default_proxy)。
fn effective(st: &AdminState, overlay: &SystemSettings) -> SystemSettings {
    let mut cfg: SystemConfig = (*st.yaml_config).clone();
    overlay.apply_to(&mut cfg);
    let mut full = SystemSettings::from_effective(&cfg, overlay.default_proxy.clone());
    // egress_pool 与 default_proxy 同属「不进 SystemConfig」的出口字段,from_effective 不含它,
    // 这里从 overlay 原样回灌(供前端展示当前池)。
    full.egress_pool = overlay.egress_pool.clone();
    full
}

/// 把有效设置序列化为响应,并掩码 default_proxy 的密码段(防 user:pass@ 经接口泄漏,
/// 审查 Architect#3)。真实值仍在库里,worker 用真实值。
fn respond(settings: SystemSettings) -> axum::response::Response {
    let mut v = serde_json::to_value(&settings).unwrap_or(serde_json::Value::Null);
    if let Some(dp) = v.get("default_proxy").and_then(|d| d.as_str()) {
        let masked = super::redact_proxy_url(dp);
        v["default_proxy"] = serde_json::json!(masked);
    }
    // egress_pool 每条 URL 同样可含 user:pass@,逐条掩码密码段(真实值仍在库里)。
    if let Some(arr) = v.get("egress_pool").and_then(|d| d.as_array()) {
        let masked: Vec<serde_json::Value> = arr
            .iter()
            .map(|item| match item.as_str() {
                Some(s) => serde_json::json!(super::redact_proxy_url(s)),
                None => item.clone(),
            })
            .collect();
        v["egress_pool"] = serde_json::Value::Array(masked);
    }
    Json(v).into_response()
}

/// `GET /settings` → 当前有效全量设置。
async fn get_settings(State(st): State<AdminState>) -> axum::response::Response {
    let overlay = match read_overlay(&st) {
        Ok(o) => o,
        Err(e) => return internal_error(e),
    };
    respond(effective(&st, &overlay))
}

/// `PUT /settings` ← 部分 patch(null=删该 overlay 字段回 YAML 默认,非 null=存)。
/// 返回叠加后的新有效全量。
async fn put_settings(
    State(st): State<AdminState>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let serde_json::Value::Object(patch) = body else {
        return api_error(StatusCode::BAD_REQUEST, "请求体应为 JSON 对象");
    };

    // 现有 overlay(原始 JSON map)→ 合并 patch:null 删键、非 null 设键。
    let mut overlay_map: serde_json::Map<String, serde_json::Value> = match st.store.get_settings()
    {
        Ok(Some(j)) => serde_json::from_str(&j).unwrap_or_default(),
        Ok(None) => serde_json::Map::new(),
        Err(e) => return internal_error(e),
    };
    for (k, v) in patch {
        if v.is_null() {
            overlay_map.remove(&k);
            continue;
        }
        // default_proxy:空串=清除;非空=写入边界校验 + 归一(fail-closed,审查 Skeptic#2)。
        if k == "default_proxy" {
            match v.as_str() {
                Some(s) if s.trim().is_empty() => {
                    overlay_map.remove(&k);
                }
                Some(s) => match super::validate_proxy_url(s) {
                    Ok(valid) => {
                        overlay_map.insert(k, serde_json::json!(valid));
                    }
                    Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
                },
                None => return api_error(StatusCode::BAD_REQUEST, "default_proxy 须为字符串"),
            }
            continue;
        }
        // egress_pool:出口代理池(数组)。空数组=清除;非空=逐条 validate_proxy_url(fail-closed,
        // 同 default_proxy:拒绝含 *** 掩码的回传值,绝不把脱敏形态当真值存)。
        if k == "egress_pool" {
            match v.as_array() {
                Some(arr) if arr.is_empty() => {
                    overlay_map.remove(&k);
                }
                Some(arr) => {
                    let mut validated = Vec::with_capacity(arr.len());
                    for item in arr {
                        let Some(s) = item.as_str() else {
                            return api_error(
                                StatusCode::BAD_REQUEST,
                                "egress_pool 每项须为字符串",
                            );
                        };
                        match super::validate_proxy_url(s) {
                            Ok(valid) => validated.push(serde_json::Value::String(valid)),
                            Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
                        }
                    }
                    overlay_map.insert(k, serde_json::Value::Array(validated));
                }
                None => return api_error(StatusCode::BAD_REQUEST, "egress_pool 须为数组"),
            }
            continue;
        }
        overlay_map.insert(k, v);
    }

    // 校验合并后的 overlay 能解析为 SystemSettings(deny_unknown_fields:拼错 key 直接拒,
    // 类型不符也拒,不写坏库)。
    let overlay: SystemSettings =
        match serde_json::from_value(serde_json::Value::Object(overlay_map.clone())) {
            Ok(s) => s,
            Err(e) => return api_error(StatusCode::BAD_REQUEST, &format!("设置字段不合法: {e}")),
        };

    let json = serde_json::to_string(&overlay_map).unwrap_or_else(|_| "{}".into());
    if let Err(e) = st.store.upsert_settings(&json) {
        return internal_error(e);
    }
    respond(effective(&st, &overlay))
}

#[cfg(test)]
mod tests {
    use super::super::tests_support::{app, req};
    use axum::body::to_bytes;
    use tower::ServiceExt;

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn get_returns_yaml_defaults_when_no_overlay() {
        let (app, _store) = app();
        let resp = app.oneshot(req("GET", "/settings", None)).await.unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        // 默认 SystemConfig:max_failures=5、cache_read_multiplier=1.0、无 default_proxy。
        assert_eq!(v["max_failures"], 5);
        assert_eq!(v["cache_read_multiplier"], 1.0);
        assert!(v.get("default_proxy").map(|d| d.is_null()).unwrap_or(true));
    }

    #[tokio::test]
    async fn put_stores_overlay_and_get_reflects_it() {
        let (app, _store) = app();
        let resp = app
            .clone()
            .oneshot(req(
                "PUT",
                "/settings",
                Some(r#"{"rate_limit_cooldown_secs": 600, "default_proxy": "socks5://h:1080"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(v["rate_limit_cooldown_secs"], 600);
        assert_eq!(v["default_proxy"], "socks5://h:1080");
        // 再 GET 应持久反映。
        let resp2 = app.oneshot(req("GET", "/settings", None)).await.unwrap();
        let v2 = body_json(resp2).await;
        assert_eq!(v2["rate_limit_cooldown_secs"], 600);
        assert_eq!(v2["default_proxy"], "socks5://h:1080");
    }

    #[tokio::test]
    async fn put_null_resets_field_to_yaml_default() {
        let (app, _store) = app();
        // 先设 overlay。
        app.clone()
            .oneshot(req("PUT", "/settings", Some(r#"{"max_failures": 1}"#)))
            .await
            .unwrap();
        // null 重置回 YAML 默认(5)。
        let resp = app
            .clone()
            .oneshot(req("PUT", "/settings", Some(r#"{"max_failures": null}"#)))
            .await
            .unwrap();
        let v = body_json(resp).await;
        assert_eq!(v["max_failures"], 5, "null 应回退到 YAML 默认");
    }

    #[tokio::test]
    async fn put_rejects_wrong_type() {
        let (app, _store) = app();
        let resp = app
            .oneshot(req(
                "PUT",
                "/settings",
                Some(r#"{"max_failures": "not-a-number"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "类型不合法应 400,不写坏库");
    }

    #[tokio::test]
    async fn put_rejects_unknown_field() {
        let (app, _store) = app();
        // 拼错 key(max_failure 少了 s):deny_unknown_fields 应 400,不静默落库死 overlay。
        let resp = app
            .oneshot(req("PUT", "/settings", Some(r#"{"max_failure": 1}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "未知字段应 400");
    }

    #[tokio::test]
    async fn put_rejects_invalid_default_proxy() {
        let (app, _store) = app();
        // 含掩码占位的回传值必须拒绝(fail-closed,绝不把脱敏形态当真值存)。
        let resp = app
            .oneshot(req(
                "PUT",
                "/settings",
                Some(r#"{"default_proxy": "socks5://u:***@h:1080"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 400, "非法/掩码代理应 400");
    }

    #[tokio::test]
    async fn put_default_proxy_password_redacted_in_response() {
        let (app, _store) = app();
        let resp = app
            .oneshot(req(
                "PUT",
                "/settings",
                Some(r#"{"default_proxy": "socks5://user:pass@host:1080"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(
            v["default_proxy"], "socks5://user:***@host:1080",
            "响应里默认代理的密码段必须掩码"
        );
    }

    #[tokio::test]
    async fn settings_requires_auth() {
        let (app, _store) = app();
        let unauth = axum::http::Request::builder()
            .method("GET")
            .uri("/settings")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(unauth).await.unwrap();
        assert_eq!(resp.status(), 401);
    }
}
