//! router 角色 —— 对外唯一入口。
//!
//! 职责:① API key 鉴权;② 从 metadata 提 session_id → 选 worker(会话亲和);
//! ③ 反向代理转发到选中的 worker。账号归属是 worker 静态决定的,router 只管
//! "同 session 稳定打同 worker"。见 docs/ARCHITECTURE.md §1.3。
//!
//! Phase 0:鉴权用 SQLite(可空库时放行 stub);亲和表纯内存。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use gw_core::config::{InstancesConfig, SystemConfig};
use gw_core::routing::extract_session_from_metadata;
use gw_core::store::ControlStore;
use gw_store::SqliteStore;
use parking_lot::Mutex;

use crate::admin::{self, AdminState};
use crate::CLIENT_KEY_HEADER;

/// 一个 worker 的转发目标。
#[derive(Clone)]
struct WorkerTarget {
    instance: u32,
    base_url: String, // http://127.0.0.1:900N
}

/// session → worker 亲和表项。
struct AffinityEntry {
    instance: u32,
    last_seen: Instant,
}

const AFFINITY_TTL: Duration = Duration::from_secs(30 * 60);

struct RouterState {
    workers: Vec<WorkerTarget>,
    /// session_id → worker 亲和(纯内存,P0)。
    affinity: Mutex<HashMap<String, AffinityEntry>>,
    /// 各 worker 当前活跃 session 数(选最空 worker 用)。
    load: Mutex<HashMap<u32, u64>>,
    store: Option<Arc<SqliteStore>>,
    http: reqwest::Client,
}

pub async fn run(instances_path: &Path, db_path: &Path, system_path: &Path) -> anyhow::Result<()> {
    let instances: InstancesConfig = {
        let text = std::fs::read_to_string(instances_path)
            .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", instances_path.display()))?;
        serde_yaml::from_str(&text)?
    };
    let system: SystemConfig = std::fs::read_to_string(system_path)
        .ok()
        .and_then(|t| serde_yaml::from_str(&t).ok())
        .unwrap_or_default();

    let workers: Vec<WorkerTarget> = instances
        .workers
        .iter()
        .map(|w| WorkerTarget {
            instance: w.instance,
            base_url: format!("http://{}", w.listen),
        })
        .collect();

    if workers.is_empty() {
        anyhow::bail!("instances.yaml 未定义任何 worker");
    }

    // 鉴权库:打不开就降级为放行(P0 容忍),P4 强制要求。
    let store = match SqliteStore::open(db_path) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            tracing::warn!("控制面库打开失败,P0 降级放行鉴权: {e}");
            None
        }
    };

    // admin 控制面:仅当配置了非空 admin_token 且控制面库可用时启用。
    let admin_state = match (system.admin.token(), &store) {
        (Some(token), Some(store)) => Some(AdminState {
            token: Arc::new(token.to_string()),
            store: store.clone(),
        }),
        (Some(_), None) => {
            tracing::warn!("配置了 admin_token 但控制面库不可用,admin 未启用");
            None
        }
        (None, _) => None,
    };

    tracing::info!(
        listen = %instances.router.listen,
        workers = workers.len(),
        "router 就绪"
    );

    let state = Arc::new(RouterState {
        workers,
        affinity: Mutex::new(HashMap::new()),
        load: Mutex::new(HashMap::new()),
        store,
        http: reqwest::Client::new(),
    });

    let mut app = Router::new()
        .route("/v1/messages", post(forward))
        .route("/v1/models", get(forward_models))
        .route("/health", get(health))
        .with_state(state);

    // admin API 挂在 /admin/api(自带 AdminState + 鉴权中间件)。SPA 静态资源后续接 /admin。
    if let Some(admin_state) = admin_state {
        tracing::info!("admin 控制面已启用: /admin/api/*");
        app = app.nest("/admin/api", admin::admin_api_router(admin_state));
    }

    let listener = tokio::net::TcpListener::bind(&instances.router.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(st): State<Arc<RouterState>>) -> impl IntoResponse {
    // 聚合各 worker 的 /health。
    let mut worker_status = Vec::new();
    for w in &st.workers {
        let url = format!("{}/health", w.base_url);
        let status = match st.http.get(&url).timeout(Duration::from_secs(3)).send().await {
            Ok(r) if r.status().is_success() => r
                .json::<serde_json::Value>()
                .await
                .unwrap_or_else(|_| serde_json::json!({"status": "bad_json"})),
            Ok(r) => serde_json::json!({"status": format!("http_{}", r.status().as_u16())}),
            Err(_) => serde_json::json!({"instance": w.instance, "status": "unreachable"}),
        };
        worker_status.push(status);
    }
    Json(serde_json::json!({
        "role": "router",
        "workers": worker_status,
    }))
}

async fn forward(
    State(st): State<Arc<RouterState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    // ① 鉴权(并拿到客户 key,用于用量归属)。
    let client_key = match authorize(&st, &headers).await {
        Ok(k) => k,
        Err(resp) => return resp,
    };

    // ② 选 worker(会话亲和)。
    let session_id = parse_session_id(&body);
    let target = pick_worker(&st, session_id.as_deref());
    let Some(target) = target else {
        return (StatusCode::SERVICE_UNAVAILABLE, "无可用 worker").into_response();
    };

    // ③ 转发(流式透传)。
    let url = format!("{}/v1/messages", target.base_url);
    let mut req = st.http.post(&url).body(body.clone());
    // 透传 content-type / accept;Authorization 不必再传给内网 worker。
    if let Some(ct) = headers.get(axum::http::header::CONTENT_TYPE) {
        req = req.header(axum::http::header::CONTENT_TYPE, ct);
    }
    if let Some(ac) = headers.get(axum::http::header::ACCEPT) {
        req = req.header(axum::http::header::ACCEPT, ac);
    }
    // 客户 key 归属:经内网头透传给 worker(worker 据此把 usage 归到该客户 #v61)。
    if let Some(k) = &client_key {
        req = req.header(CLIENT_KEY_HEADER, k);
    }

    match req.send().await {
        Ok(resp) => {
            let status = resp.status();
            let mut builder = axum::response::Response::builder().status(status);
            if let Some(ct) = resp.headers().get(reqwest::header::CONTENT_TYPE) {
                builder = builder.header(axum::http::header::CONTENT_TYPE, ct);
            }
            // 流式透传 worker 的响应体。
            let stream = resp.bytes_stream();
            let body = axum::body::Body::from_stream(stream);
            builder.body(body).unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "构造响应失败").into_response()
            })
        }
        Err(e) => {
            tracing::error!(instance = target.instance, "转发到 worker 失败: {e}");
            (StatusCode::BAD_GATEWAY, "worker 不可达").into_response()
        }
    }
}

/// 鉴权(forward / forward_models 共用)。
/// `Ok(Some(key_id))`=通过且拿到客户 key;`Ok(None)`=放行但无归属(store=None 的 P0 降级);
/// `Err(resp)`=拒绝(401/500)。key_id 用于把客户用量归属(X-Gw-Client-Key 透传给 worker)。
async fn authorize(
    st: &RouterState,
    headers: &HeaderMap,
) -> Result<Option<String>, axum::response::Response> {
    let Some(store) = st.store.as_ref() else {
        return Ok(None); // P0:无控制面库,放行且无归属。
    };
    match extract_bearer(headers) {
        Some(k) => match store.authenticate(&k).await {
            Ok(Some(auth)) if !auth.disabled => Ok(Some(auth.key_id)),
            Ok(Some(_)) => Err(unauthorized("API key 已禁用")),
            Ok(None) => Err(unauthorized("无效 API key")),
            Err(e) => {
                tracing::error!("鉴权查询失败: {e}");
                Err((StatusCode::INTERNAL_SERVER_ERROR, "鉴权失败").into_response())
            }
        },
        None => Err(unauthorized("缺少 Authorization")),
    }
}

/// `GET /v1/models` —— 模型目录与会话无关,鉴权后转发到任一(最空)worker。
///
/// 已知局限(单 provider 框架下正确,多 provider 待补):每个 worker 绑定一个账号组、
/// 一个 provider,这里只问最空的那个 worker。一旦插入第二个 provider 家族,对外目录会
/// 取决于当下哪个 worker 最空,而非聚合所有可路由 provider。多 provider 部署需改为按
/// 家族聚合(各 family 取一个代表 worker 并合并),留作 provider 真正异构时再做。
async fn forward_models(
    State(st): State<Arc<RouterState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    if let Err(resp) = authorize(&st, &headers).await {
        return resp;
    }
    let Some(target) = least_loaded(&st) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "无可用 worker").into_response();
    };
    let url = format!("{}/v1/models", target.base_url);
    match st.http.get(&url).timeout(Duration::from_secs(10)).send().await {
        Ok(resp) => {
            let status = resp.status();
            let mut builder = axum::response::Response::builder().status(status);
            if let Some(ct) = resp.headers().get(reqwest::header::CONTENT_TYPE) {
                builder = builder.header(axum::http::header::CONTENT_TYPE, ct);
            }
            let body = axum::body::Body::from_stream(resp.bytes_stream());
            builder.body(body).unwrap_or_else(|_| {
                (StatusCode::INTERNAL_SERVER_ERROR, "构造响应失败").into_response()
            })
        }
        Err(e) => {
            tracing::error!(instance = target.instance, "转发 models 到 worker 失败: {e}");
            (StatusCode::BAD_GATEWAY, "worker 不可达").into_response()
        }
    }
}

/// 选 worker:命中亲和则复用,否则选当前负载最低的 worker 并记亲和。
fn pick_worker(st: &RouterState, session_id: Option<&str>) -> Option<WorkerTarget> {
    let now = Instant::now();

    if let Some(sid) = session_id {
        let mut aff = st.affinity.lock();
        // 清理过期。
        aff.retain(|_, e| now.duration_since(e.last_seen) < AFFINITY_TTL);
        if let Some(entry) = aff.get_mut(sid) {
            entry.last_seen = now;
            let instance = entry.instance;
            if let Some(w) = st.workers.iter().find(|w| w.instance == instance) {
                return Some(w.clone());
            }
        }
        // 未命中:选最空 worker,记亲和。
        let chosen = least_loaded(st)?;
        aff.insert(
            sid.to_string(),
            AffinityEntry {
                instance: chosen.instance,
                last_seen: now,
            },
        );
        bump_load(st, chosen.instance);
        return Some(chosen);
    }

    // 无 session_id:仍选最空 worker(无亲和记忆)。
    least_loaded(st)
}

fn least_loaded(st: &RouterState) -> Option<WorkerTarget> {
    let load = st.load.lock();
    st.workers
        .iter()
        .min_by_key(|w| load.get(&w.instance).copied().unwrap_or(0))
        .cloned()
}

fn bump_load(st: &RouterState, instance: u32) {
    *st.load.lock().entry(instance).or_insert(0) += 1;
}

/// 从 Anthropic body 提 session_id 作为 worker 亲和键。
///
/// 优先 `metadata.user_id` 里的显式 session_id;缺失时(Claude Code 等常不传)回退到
/// **与 worker 选号同源**的 conversationId 派生(system 锚点 + 前2条 user 哈希)。这样
/// router(选 worker)和 worker(选组内账号)、cache_sim(命中估算)三处用**同一会话键**,
/// 同会话稳定钉同 worker→同账号→缓存热(审查 #131①:统一身份链)。仍提不到才 None(轮转)。
fn parse_session_id(body: &Bytes) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    if let Some(user_id) = v.get("metadata").and_then(|m| m.get("user_id")).and_then(|u| u.as_str()) {
        if let Some(sid) = extract_session_from_metadata(user_id) {
            return Some(sid);
        }
    }
    // 回退:Kiro conversationId 派生(与 worker 亲和键同源)。
    gw_kiro::converter::affinity_key_from_body(&v)
}

fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    raw.strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .map(|s| s.to_string())
        .or_else(|| {
            // 兼容 x-api-key 风格直接放 key。
            Some(raw.to_string())
        })
}

fn unauthorized(msg: &str) -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(serde_json::json!({"type":"error","error":{"type":"authentication_error","message": msg}})),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_session_from_body() {
        let body = Bytes::from(
            serde_json::json!({
                "model": "m",
                "metadata": {"user_id": "{\"session_id\":\"sess-xyz\"}"}
            })
            .to_string(),
        );
        assert_eq!(parse_session_id(&body), Some("sess-xyz".into()));
    }

    #[test]
    fn parse_session_none_when_absent() {
        let body = Bytes::from(serde_json::json!({"model": "m"}).to_string());
        assert_eq!(parse_session_id(&body), None);
    }

    #[test]
    fn parse_session_falls_back_to_conversation_id() {
        // 无 metadata 但有 messages → 回退到 conversationId 派生(与 worker 亲和同源)。
        let mk = || {
            Bytes::from(
                serde_json::json!({
                    "model": "claude-opus-4-8",
                    "max_tokens": 1024,
                    "messages": [{"role": "user", "content": "hello world"}]
                })
                .to_string(),
            )
        };
        let k1 = parse_session_id(&mk());
        let k2 = parse_session_id(&mk());
        assert!(k1.is_some(), "有 messages 时应派生出会话键");
        assert_eq!(k1, k2, "同内容派生的会话键必须稳定");
    }

    fn mk_state(n: u32) -> RouterState {
        RouterState {
            workers: (0..n)
                .map(|i| WorkerTarget {
                    instance: i,
                    base_url: format!("http://127.0.0.1:{}", 9000 + i),
                })
                .collect(),
            affinity: Mutex::new(HashMap::new()),
            load: Mutex::new(HashMap::new()),
            store: None,
            http: reqwest::Client::new(),
        }
    }

    #[test]
    fn affinity_sticks_same_session_to_same_worker() {
        let st = mk_state(3);
        let first = pick_worker(&st, Some("s1")).unwrap();
        for _ in 0..5 {
            let again = pick_worker(&st, Some("s1")).unwrap();
            assert_eq!(again.instance, first.instance, "同 session 必须钉同 worker");
        }
    }

    #[test]
    fn new_sessions_spread_across_workers() {
        let st = mk_state(2);
        let a = pick_worker(&st, Some("sa")).unwrap();
        let b = pick_worker(&st, Some("sb")).unwrap();
        // 2 worker、各空,第二个新 session 应落到另一个(负载均衡)。
        assert_ne!(a.instance, b.instance);
    }
}
