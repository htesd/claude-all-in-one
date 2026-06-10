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
/// 全表过期清理(O(n) retain)的节流间隔:命中路径的过期判断是 O(1) 精确的,
/// 清理只影响负载统计里陈旧条目的滞留时长(≤ 本间隔),不影响亲和正确性。
const AFFINITY_CLEANUP_INTERVAL: Duration = Duration::from_secs(5);

/// 亲和表 + 节流的清理时钟。负载也从这张表派生:
/// 活跃负载 = 钉在该 worker 上的未过期 session 数,session 过期即回落。
struct AffinityTable {
    entries: HashMap<String, AffinityEntry>,
    last_cleanup: Instant,
}

impl AffinityTable {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            last_cleanup: Instant::now(),
        }
    }

    /// 到期才做全表 retain(审查 Skeptic#3:不能每请求 O(n) 扫几十万 session)。
    fn cleanup_if_due(&mut self, now: Instant) {
        if now.duration_since(self.last_cleanup) < AFFINITY_CLEANUP_INTERVAL {
            return;
        }
        self.last_cleanup = now;
        self.entries
            .retain(|_, e| now.duration_since(e.last_seen) < AFFINITY_TTL);
    }
}

struct RouterState {
    workers: Vec<WorkerTarget>,
    /// session_id → worker 亲和(纯内存,P0)。
    affinity: Mutex<AffinityTable>,
    store: Option<Arc<SqliteStore>>,
    http: reqwest::Client,
}

pub async fn run(instances_path: &Path, db_path: &Path, system_path: &Path) -> anyhow::Result<()> {
    let instances: InstancesConfig = {
        let text = std::fs::read_to_string(instances_path)
            .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", instances_path.display()))?;
        serde_yaml::from_str(&text)?
    };
    instances.validate()?; // 拓扑约束:同组多 worker 等违规直接拒绝启动。
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
        (Some(token), Some(store)) => Some(AdminState::new(
            Arc::new(token.to_string()),
            store.clone(),
            instances.workers.clone(),
        )),
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
        affinity: Mutex::new(AffinityTable::new()),
        store,
        http: reqwest::Client::new(),
    });

    let mut app = Router::new()
        .route("/v1/messages", post(forward))
        .route("/v1/models", get(forward_models))
        .route("/health", get(health))
        .with_state(state);

    // admin 启用时:/admin/api 挂 API(自带鉴权);/admin 挂 SPA 静态资源(更具体的
    // /admin/api 优先匹配)。两种资产来源:
    // - 默认:ServeDir 读运行目录的 admin-ui/dist(开发迭代,改前端 bun build 即生效);
    // - `--features embed-ui`:资源内嵌进二进制(单文件部署,见 embedded_ui 模块)。
    if let Some(admin_state) = admin_state {
        app = app.nest("/admin/api", admin::admin_api_router(admin_state));
        #[cfg(feature = "embed-ui")]
        {
            tracing::info!("admin 控制面已启用: /admin(SPA, 内嵌资源) + /admin/api/*");
            app = app.nest_service("/admin", axum::routing::get(embedded_ui::serve));
        }
        #[cfg(not(feature = "embed-ui"))]
        {
            // 用 .fallback(不是 .not_found_service):后者把兜底响应裹成 404(SetStatus),
            // 会让 SPA 客户端路由(/admin/usage 等)拿到 404+index.html;.fallback 保留
            // ServeFile 原状态 200。
            let spa = tower_http::services::ServeDir::new("admin-ui/dist").fallback(
                tower_http::services::ServeFile::new("admin-ui/dist/index.html"),
            );
            tracing::info!("admin 控制面已启用: /admin(SPA, 磁盘 dist) + /admin/api/*");
            app = app.nest_service("/admin", spa);
        }
    }

    let listener = tokio::net::TcpListener::bind(&instances.router.listen).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::shutdown_signal("router"))
        .await?;
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

    // ③ 转发(流式透传)。send() 失败 = worker 进程可能已挂但仍在配置里(连接拒绝):
    // 对这种**未送达**的请求做一次故障转移——丢弃指向故障实例的亲和、换 worker 重发,
    // 否则该 session 会钉死故障 worker 502 长达 AFFINITY_TTL(审查 Architect#1)。
    // client 未设总超时,Err 基本是 connect 级错误,重复送达上游的风险可忽略。
    let mut target = target;
    let mut failed_over = false;
    loop {
        match send_messages_to_worker(&st, &target, &headers, &body, client_key.as_deref()).await
        {
            Ok(resp) => return proxy_response(resp),
            Err(e) => {
                tracing::error!(instance = target.instance, "转发到 worker 失败: {e}");
                if failed_over {
                    return (StatusCode::BAD_GATEWAY, "worker 不可达").into_response();
                }
                match failover_target(&st, session_id.as_deref(), target.instance) {
                    Some(t) => {
                        tracing::warn!(from = target.instance, to = t.instance,
                            "故障转移:换 worker 重发");
                        target = t;
                        failed_over = true;
                    }
                    None => {
                        return (StatusCode::BAD_GATEWAY, "worker 不可达").into_response()
                    }
                }
            }
        }
    }
}

/// 把客户请求发给指定 worker(透传 content-type/accept;Authorization 不传内网;
/// 客户 key 归属经内网头透传,worker 据此把 usage 归到该客户 #v61)。
async fn send_messages_to_worker(
    st: &RouterState,
    target: &WorkerTarget,
    headers: &HeaderMap,
    body: &Bytes,
    client_key: Option<&str>,
) -> Result<reqwest::Response, reqwest::Error> {
    let url = format!("{}/v1/messages", target.base_url);
    let mut req = st.http.post(&url).body(body.clone());
    if let Some(ct) = headers.get(axum::http::header::CONTENT_TYPE) {
        req = req.header(axum::http::header::CONTENT_TYPE, ct);
    }
    if let Some(ac) = headers.get(axum::http::header::ACCEPT) {
        req = req.header(axum::http::header::ACCEPT, ac);
    }
    if let Some(k) = client_key {
        req = req.header(CLIENT_KEY_HEADER, k);
    }
    req.send().await
}

/// worker 响应 → 客户端响应(状态码 + content-type + 流式透传响应体)。
fn proxy_response(resp: reqwest::Response) -> axum::response::Response {
    let status = resp.status();
    let mut builder = axum::response::Response::builder().status(status);
    if let Some(ct) = resp.headers().get(reqwest::header::CONTENT_TYPE) {
        builder = builder.header(axum::http::header::CONTENT_TYPE, ct);
    }
    let body = axum::body::Body::from_stream(resp.bytes_stream());
    builder
        .body(body)
        .unwrap_or_else(|_| (StatusCode::INTERNAL_SERVER_ERROR, "构造响应失败").into_response())
}

/// 转发失败后的故障转移:丢弃指向故障实例的会话亲和,在**其余** worker 里按活跃
/// 负载重选并重钉亲和。没有其他 worker 可选时返回 None(调用方 502)。
fn failover_target(
    st: &RouterState,
    session_id: Option<&str>,
    failed_instance: u32,
) -> Option<WorkerTarget> {
    let candidates: Vec<WorkerTarget> = st
        .workers
        .iter()
        .filter(|w| w.instance != failed_instance)
        .cloned()
        .collect();
    let mut aff = st.affinity.lock();
    if let Some(sid) = session_id {
        if aff.entries.get(sid).map(|e| e.instance) == Some(failed_instance) {
            aff.entries.remove(sid);
        }
    }
    let chosen = least_loaded_locked(&candidates, &aff.entries)?;
    if let Some(sid) = session_id {
        aff.entries.insert(
            sid.to_string(),
            AffinityEntry {
                instance: chosen.instance,
                last_seen: Instant::now(),
            },
        );
    }
    Some(chosen)
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
            Ok(Some(auth)) if auth.disabled => Err(unauthorized("API key 已禁用")),
            // 限额用尽:429(语义对齐 rate limit 家族,客户端可识别;admin 提额或
            // 重置 used 后立即恢复——逐请求查库,无缓存)。
            Ok(Some(auth)) if auth.over_quota => Err((
                StatusCode::TOO_MANY_REQUESTS,
                Json(serde_json::json!({"type":"error","error":{
                    "type":"rate_limit_error","message":"API key 限额已用尽,请联系管理员"}})),
            )
                .into_response()),
            Ok(Some(auth)) => Ok(Some(auth.key_id)),
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

/// 选 worker:命中亲和则复用,否则选当前活跃负载最低的 worker 并记亲和。
fn pick_worker(st: &RouterState, session_id: Option<&str>) -> Option<WorkerTarget> {
    pick_worker_at(st, session_id, Instant::now())
}

/// `now` 注入便于测试过期语义(Instant 无法伪造,只能向前偏移)。
fn pick_worker_at(st: &RouterState, session_id: Option<&str>, now: Instant) -> Option<WorkerTarget> {
    let mut aff = st.affinity.lock();
    aff.cleanup_if_due(now);

    if let Some(sid) = session_id {
        if let Some(entry) = aff.entries.get_mut(sid) {
            // 命中路径 O(1) 精确判过期:全表清理是节流的,不能依赖它兜底。
            if now.duration_since(entry.last_seen) < AFFINITY_TTL {
                entry.last_seen = now;
                let instance = entry.instance;
                if let Some(w) = st.workers.iter().find(|w| w.instance == instance) {
                    return Some(w.clone());
                }
            }
            // 已过期、或指向已不存在的 worker(拓扑变更):丢弃,走重选。
            aff.entries.remove(sid);
        }
        // 未命中:选最空 worker,记亲和(插入即计入活跃负载)。
        let chosen = least_loaded_locked(&st.workers, &aff.entries)?;
        aff.entries.insert(
            sid.to_string(),
            AffinityEntry {
                instance: chosen.instance,
                last_seen: now,
            },
        );
        return Some(chosen);
    }

    // 无 session_id:仍选最空 worker(无亲和记忆,不计负载)。
    least_loaded_locked(&st.workers, &aff.entries)
}

/// 活跃负载 = 亲和表里钉在该 worker 上的未过期 session 数。
/// (此前用只增不减的累计计数:会话过期后负载不回落,空闲 worker 拿不到新会话。)
fn least_loaded_locked(
    workers: &[WorkerTarget],
    aff: &HashMap<String, AffinityEntry>,
) -> Option<WorkerTarget> {
    let mut counts: HashMap<u32, u64> = HashMap::new();
    for e in aff.values() {
        *counts.entry(e.instance).or_insert(0) += 1;
    }
    workers
        .iter()
        .min_by_key(|w| counts.get(&w.instance).copied().unwrap_or(0))
        .cloned()
}

fn least_loaded(st: &RouterState) -> Option<WorkerTarget> {
    let now = Instant::now();
    let mut aff = st.affinity.lock();
    aff.cleanup_if_due(now);
    least_loaded_locked(&st.workers, &aff.entries)
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

/// `--features embed-ui`:admin SPA 资源内嵌进二进制(单文件部署)。
/// rust-embed 语义:release 编译期嵌入;debug 仍从磁盘实时读(改前端无需重编译)。
#[cfg(feature = "embed-ui")]
mod embedded_ui {
    use axum::http::{header, StatusCode, Uri};
    use axum::response::IntoResponse;

    #[derive(rust_embed::RustEmbed)]
    #[folder = "../../admin-ui/dist"]
    struct AdminAssets;

    /// nest_service 已剥掉 /admin 前缀;空路径(GET /admin)落到 index.html。
    fn asset_key(uri_path: &str) -> &str {
        let p = uri_path.trim_start_matches('/');
        if p.is_empty() {
            "index.html"
        } else {
            p
        }
    }

    /// vite 产物文件名带内容哈希 → assets/ 下永久缓存;index.html 等必须每次再验证,
    /// 否则发布新版后浏览器拿旧 index 引用已不存在的旧哈希资源。
    fn cache_control(path: &str) -> &'static str {
        if path.starts_with("assets/") {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }
    }

    pub async fn serve(uri: Uri) -> axum::response::Response {
        let path = asset_key(uri.path());
        match AdminAssets::get(path) {
            Some(f) => asset_response(path, f),
            // SPA 客户端路由(/admin/usage 等)兜底:回 index.html 保持 200。
            None => match AdminAssets::get("index.html") {
                Some(f) => asset_response("index.html", f),
                None => (
                    StatusCode::NOT_FOUND,
                    "admin UI 资源缺失:构建二进制前未执行 `bun run build`",
                )
                    .into_response(),
            },
        }
    }

    fn asset_response(path: &str, file: rust_embed::EmbeddedFile) -> axum::response::Response {
        let mime = mime_guess::from_path(path).first_or_octet_stream();
        let body = match file.data {
            std::borrow::Cow::Borrowed(b) => axum::body::Body::from(b),
            std::borrow::Cow::Owned(v) => axum::body::Body::from(v),
        };
        (
            [
                (header::CONTENT_TYPE, mime.as_ref()),
                (header::CACHE_CONTROL, cache_control(path)),
            ],
            body,
        )
            .into_response()
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn asset_key_normalizes_paths() {
            assert_eq!(asset_key("/"), "index.html");
            assert_eq!(asset_key(""), "index.html");
            assert_eq!(asset_key("/assets/index-abc123.js"), "assets/index-abc123.js");
            assert_eq!(asset_key("/usage"), "usage"); // 未知路径由 serve 兜底回 index.html
        }

        #[test]
        fn hashed_assets_cache_forever_index_revalidates() {
            assert_eq!(
                cache_control("assets/index-abc123.js"),
                "public, max-age=31536000, immutable"
            );
            assert_eq!(cache_control("index.html"), "no-cache");
            assert_eq!(cache_control("favicon.svg"), "no-cache");
        }
    }
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
            affinity: Mutex::new(AffinityTable::new()),
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

    #[test]
    fn load_recedes_when_sessions_expire() {
        let st = mk_state(2);
        let t0 = Instant::now();
        // 4 个 session 均匀铺开(2/2)。
        let placed: Vec<u32> = ["s1", "s2", "s3", "s4"]
            .iter()
            .map(|sid| pick_worker_at(&st, Some(sid), t0).unwrap().instance)
            .collect();
        let a = placed[0];
        assert_eq!(placed.iter().filter(|&&i| i == a).count(), 2, "前置:应 2/2 均铺");

        // a 上的两个 session 全部过期(模拟时间推进:只刷新另一台的 last_seen)。
        let t1 = t0 + AFFINITY_TTL + Duration::from_secs(1);
        {
            let mut aff = st.affinity.lock();
            for e in aff.entries.values_mut() {
                if e.instance != a {
                    e.last_seen = t1;
                }
            }
        }
        // 活跃负载 a=0、b=2:接下来两个新 session 都必须落到 a。
        // (旧实现累计计数不回落,第二个会被错误分到 b。)
        assert_eq!(pick_worker_at(&st, Some("n1"), t1).unwrap().instance, a);
        assert_eq!(pick_worker_at(&st, Some("n2"), t1).unwrap().instance, a);
    }

    #[test]
    fn stale_affinity_to_removed_worker_repicks() {
        let st = mk_state(2);
        st.affinity.lock().entries.insert(
            "ghost".into(),
            AffinityEntry { instance: 99, last_seen: Instant::now() },
        );
        // 亲和指向已不存在的 worker(拓扑变更):应重选一个真实 worker 并修正亲和。
        let picked = pick_worker(&st, Some("ghost")).expect("应能重选真实 worker");
        assert!(st.workers.iter().any(|w| w.instance == picked.instance));
        assert_eq!(st.affinity.lock().entries.get("ghost").unwrap().instance, picked.instance);
    }

    #[test]
    fn expired_pin_not_honored_between_cleanups() {
        let st = mk_state(2);
        let t0 = Instant::now();
        let t1 = t0 + AFFINITY_TTL + Duration::from_secs(1);
        {
            let mut aff = st.affinity.lock();
            // s1 钉在 instance 1 上但早已过期;全表清理"刚跑过"(节流未到期)。
            aff.entries
                .insert("s1".into(), AffinityEntry { instance: 1, last_seen: t0 });
            aff.last_cleanup = t1;
        }
        // 命中路径必须 O(1) 精确判过期,不依赖节流的全表清理兜底:
        // 两 worker 均无活跃负载,重选 tie 取 workers[0]=instance 0。
        let picked = pick_worker_at(&st, Some("s1"), t1).unwrap();
        assert_eq!(picked.instance, 0, "过期 pin 不得被复用");
    }

    #[test]
    fn failover_moves_session_off_dead_worker_and_repins() {
        let st = mk_state(2);
        let first = pick_worker(&st, Some("s1")).unwrap();
        let next =
            failover_target(&st, Some("s1"), first.instance).expect("双 worker 应有备选");
        assert_ne!(next.instance, first.instance, "备选必须避开故障实例");
        assert_eq!(
            st.affinity.lock().entries.get("s1").unwrap().instance,
            next.instance,
            "亲和应重钉到备选 worker"
        );
    }

    #[test]
    fn failover_none_when_no_alternative() {
        let st = mk_state(1);
        let only = pick_worker(&st, Some("x")).unwrap();
        assert!(
            failover_target(&st, Some("x"), only.instance).is_none(),
            "单 worker 无备选,只能 502"
        );
    }
}
