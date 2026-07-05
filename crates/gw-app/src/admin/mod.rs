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

mod accounts;
mod groups;
mod keys;
mod logs;
mod settings;
mod usage;

/// admin 路由共享态:管理密钥 + 控制面存储(keys / usage / groups / accounts / settings)。
#[derive(Clone)]
pub struct AdminState {
    pub token: Arc<String>,
    pub store: Arc<SqliteStore>,
    /// worker 拓扑(来自 instances.yaml):账号运行态聚合时逐个拉 /health。
    pub workers: Arc<Vec<gw_core::config::WorkerConfig>>,
    /// 聚合 worker /health 用的内网客户端(短超时,worker 掉线不拖死页面)。
    pub http: reqwest::Client,
    /// system.yaml 启动快照(不可变基线)。settings GET/PUT 在它之上叠 DB overlay
    /// 算"有效值";YAML 本身改动需重启,故快照即足够。
    pub yaml_config: Arc<gw_core::config::SystemConfig>,
}

impl AdminState {
    pub fn new(
        token: Arc<String>,
        store: Arc<SqliteStore>,
        workers: Vec<gw_core::config::WorkerConfig>,
        yaml_config: gw_core::config::SystemConfig,
    ) -> Self {
        let http = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .expect("reqwest client 构造失败");
        Self {
            token,
            store,
            workers: Arc::new(workers),
            http,
            yaml_config: Arc::new(yaml_config),
        }
    }
}

/// 组装 admin API 子路由(全部受鉴权中间件保护)。挂到 `/admin/api` 下。
pub fn admin_api_router(state: AdminState) -> Router {
    Router::new()
        .route("/ping", get(ping))
        .merge(usage::router())
        .merge(keys::router())
        .merge(groups::router())
        .merge(accounts::router())
        .merge(settings::router())
        .merge(logs::router())
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

/// 校验出口代理 URL(写入边界 fail-closed,审查 Skeptic#2/Architect#2):trim 后用
/// `reqwest::Proxy` 解析;非法或含掩码占位 `***` → `Err(消息)`。空串=清除,调用方先判空跳过。
/// 返回 trim 后的合法 URL。在写库前校验,避免"配了代理却静默回退裸 IP"的防封语义破坏。
pub(crate) fn validate_proxy_url(raw: &str) -> Result<String, &'static str> {
    let s = raw.trim();
    if s.is_empty() {
        return Err("代理 URL 为空");
    }
    if s.contains("***") {
        return Err("代理含掩码占位 ***,请输入完整 URL 或留空清除");
    }
    reqwest::Proxy::all(s).map_err(|_| "代理 URL 非法(支持 socks5:// / http:// / https://)")?;
    Ok(s.to_string())
}

/// 掩码代理 URL 的密码段(`scheme://user:pass@host` → `scheme://user:***@host`),
/// 保留 scheme/user/host/port 供识别。无 userinfo 原样返回。GET 响应用,防 user:pass@
/// 明文经接口/日志泄漏(审查 Architect#3)。真实值仍存库,resolver 用真实值。
pub(crate) fn redact_proxy_url(s: &str) -> String {
    let Some(scheme_end) = s.find("://") else {
        return s.to_string();
    };
    let after = &s[scheme_end + 3..];
    let Some(at) = after.find('@') else {
        return s.to_string();
    };
    let userinfo = &after[..at];
    let rest = &after[at..]; // 含 '@' 起的 host:port[/path]
    let masked = match userinfo.find(':') {
        Some(colon) => format!("{}:***", &userinfo[..colon]),
        None => userinfo.to_string(), // 只有 user 无 password,无需掩码
    };
    format!("{}{}{}", &s[..scheme_end + 3], masked, rest)
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

/// 各端点测试共用的 app/req 构造(in-memory 库 + 空 worker 拓扑)。
#[cfg(test)]
pub(crate) mod tests_support {
    use std::sync::Arc;

    use axum::body::Body;
    use axum::http::Request;
    use gw_store::SqliteStore;

    use super::{admin_api_router, AdminState};

    pub const TOKEN: &str = "admt-test";

    pub fn app() -> (axum::Router, Arc<SqliteStore>) {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let st = AdminState::new(
            Arc::new(TOKEN.to_string()),
            store.clone(),
            vec![],
            gw_core::config::SystemConfig::default(),
        );
        (admin_api_router(st), store)
    }

    /// 同 [`app`],但注入 worker 拓扑(OAuth 上号/运行态聚合等需要 worker 的测试用)。
    pub fn app_with_workers(
        workers: Vec<gw_core::config::WorkerConfig>,
    ) -> (axum::Router, Arc<SqliteStore>) {
        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        let st = AdminState::new(
            Arc::new(TOKEN.to_string()),
            store.clone(),
            workers,
            gw_core::config::SystemConfig::default(),
        );
        (admin_api_router(st), store)
    }

    pub fn req(method: &str, uri: &str, body: Option<&str>) -> Request<Body> {
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
        let h = headers(&[
            ("x-api-key", "fromkey"),
            ("authorization", "Bearer frombearer"),
        ]);
        assert_eq!(extract_admin_key(&h), Some("fromkey".to_string()));
    }

    #[test]
    fn extract_none_when_absent_or_empty() {
        assert_eq!(extract_admin_key(&headers(&[])), None);
        assert_eq!(extract_admin_key(&headers(&[("x-api-key", "")])), None);
        assert_eq!(
            extract_admin_key(&headers(&[("authorization", "Basic xxx")])),
            None
        );
        assert_eq!(
            extract_admin_key(&headers(&[("authorization", "Bearer ")])),
            None
        );
    }

    #[test]
    fn validate_proxy_url_accepts_valid_rejects_garbage_and_masked() {
        assert_eq!(
            validate_proxy_url("  socks5://u:p@h:1080  ").unwrap(),
            "socks5://u:p@h:1080"
        );
        assert!(validate_proxy_url("http://1.2.3.4:8888").is_ok());
        assert!(validate_proxy_url("").is_err());
        assert!(validate_proxy_url("   ").is_err());
        // 含掩码占位的回传值必须拒绝(防把脱敏形态当真值存进库)。
        assert!(validate_proxy_url("socks5://u:***@h:1080").is_err());
    }

    #[test]
    fn redact_proxy_url_masks_only_password() {
        assert_eq!(
            redact_proxy_url("socks5://user:secret@host:1080"),
            "socks5://user:***@host:1080"
        );
        // 无 password(只有 user)不掩。
        assert_eq!(
            redact_proxy_url("socks5://user@host:1080"),
            "socks5://user@host:1080"
        );
        // 无 userinfo 原样返回。
        assert_eq!(redact_proxy_url("http://host:8888"), "http://host:8888");
        assert_eq!(
            redact_proxy_url("socks5://1.2.3.4:1080"),
            "socks5://1.2.3.4:1080"
        );
    }
}
