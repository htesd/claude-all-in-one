//! admin 控制面 —— 嵌入 router 进程,`/admin/*` 受 `admin_token` 保护。
//!
//! 仅当 system.yaml 设了非空 `admin.token` 时,router 才挂载本模块(见 router::run)。
//! 鉴权与对外客户 apikey 完全分离:单一管理密钥,经 `x-api-key` 或 `Authorization: Bearer`
//! 传入,与 admin_token **常量时间**比较。SPA 静态资源由 router 在 `/admin` 提供。

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use gw_store::SqliteStore;

mod keys;
mod usage;

/// admin 路由共享态:管理密钥 + 控制面存储(keys / usage / groups / accounts)。
#[derive(Clone)]
pub struct AdminState {
    pub token: Arc<String>,
    pub store: Arc<SqliteStore>,
}

/// 组装 admin API 子路由(全部受鉴权中间件保护)。挂到 `/admin/api` 下。
pub fn admin_api_router(state: AdminState) -> Router {
    Router::new()
        .route("/ping", get(ping))
        .merge(usage::router())
        .merge(keys::router())
        .route_layer(middleware::from_fn_with_state(state.clone(), require_admin))
        .with_state(state)
}

/// 500 错误响应(查询/存储失败)。详细错误只进服务端日志,对外回笼统信息
/// (避免向 admin 调用方泄漏 DB/schema/路径等细节,审查 #4)。
pub(crate) fn internal_error(e: impl std::fmt::Display) -> axum::response::Response {
    tracing::error!("admin 处理失败: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"type":"error","error":{"message":"内部错误"}})),
    )
        .into_response()
}

/// 连通性探针(需鉴权)。前端登录时用它校验 admin_token 是否正确。
async fn ping() -> impl IntoResponse {
    Json(serde_json::json!({"ok": true, "role": "admin"}))
}

/// admin 鉴权中间件:凭证与 admin_token 常量时间相等才放行,否则 401。
async fn require_admin(
    State(st): State<AdminState>,
    headers: HeaderMap,
    req: Request,
    next: Next,
) -> axum::response::Response {
    match extract_admin_key(&headers) {
        Some(key) if ct_eq(key.as_bytes(), st.token.as_bytes()) => next.run(req).await,
        _ => unauthorized(),
    }
}

fn unauthorized() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"type":"error","error":{"type":"authentication_error","message":"admin 鉴权失败"}})),
    )
        .into_response()
}

/// 从请求头提取 admin 凭证:优先 `x-api-key`,否则 `Authorization: Bearer <token>`。
fn extract_admin_key(headers: &HeaderMap) -> Option<String> {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    auth.strip_prefix("Bearer ")
        .or_else(|| auth.strip_prefix("bearer "))
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// 常量时间比较两个字节串是否相等(避免 token 比较的时序侧信道)。
/// 长度不等直接 false(长度本身非敏感);等长则全程异或累积,不短路。
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderValue;
    use axum::http::HeaderName;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            let name = HeaderName::from_bytes(k.as_bytes()).unwrap();
            h.insert(name, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn ct_eq_matches_only_identical() {
        assert!(ct_eq(b"secret-token", b"secret-token"));
        assert!(!ct_eq(b"secret-token", b"secret-toketX"));
        assert!(!ct_eq(b"short", b"longer-token"));
        assert!(!ct_eq(b"", b"x"));
        assert!(ct_eq(b"", b""));
    }

    #[test]
    fn extract_x_api_key() {
        assert_eq!(
            extract_admin_key(&headers(&[("x-api-key", "tok123")])),
            Some("tok123".to_string())
        );
    }

    #[test]
    fn extract_bearer_token() {
        assert_eq!(
            extract_admin_key(&headers(&[("authorization", "Bearer tok456")])),
            Some("tok456".to_string())
        );
        assert_eq!(
            extract_admin_key(&headers(&[("authorization", "bearer tok789")])),
            Some("tok789".to_string())
        );
    }

    #[test]
    fn extract_prefers_x_api_key_over_bearer() {
        let h = headers(&[("x-api-key", "fromkey"), ("authorization", "Bearer frombearer")]);
        assert_eq!(extract_admin_key(&h), Some("fromkey".to_string()));
    }

    #[test]
    fn extract_none_when_absent_or_empty() {
        assert_eq!(extract_admin_key(&headers(&[])), None);
        assert_eq!(extract_admin_key(&headers(&[("x-api-key", "")])), None);
        assert_eq!(extract_admin_key(&headers(&[("authorization", "Basic xxx")])), None);
        assert_eq!(extract_admin_key(&headers(&[("authorization", "Bearer ")])), None);
    }
}
