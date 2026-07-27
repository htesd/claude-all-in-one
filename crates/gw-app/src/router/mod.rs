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
use crate::{CLIENT_KEY_HEADER, GROUP_HEADER};

/// 一个 worker 的转发目标。
#[derive(Clone)]
struct WorkerTarget {
    instance: u32,
    base_url: String, // http://127.0.0.1:900N
    /// 该 worker 所属账号组(G0/kiro、DARIO/dario…)。router 据此把客户 key 的请求
    /// 派发到匹配组的 worker(数据面按组隔离,见 `pick_worker`)。
    account_group: String,
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
    /// **所有组**的 worker(跨 `instances*.yaml` 聚合):router 按客户 key 的分组在
    /// 同组子集内做亲和/负载,实现一套入口按组派发(G0→kiro / DARIO→dario)。
    workers: Vec<WorkerTarget>,
    /// 未分组 key('' group)回落到的组 = 本 router 自身 instances.yaml 的主组。
    /// 保持历史行为:未分组 key 仍打本 router 原本服务的那组(kiro 主通道为 G0)。
    default_group: String,
    /// session_id → worker 亲和(纯内存,P0)。
    affinity: Mutex<AffinityTable>,
    store: Option<Arc<SqliteStore>>,
    /// 组名 → 持有其成员的 owner 集合(带取回时刻的 TTL 快照,见 `owners_for`)。
    group_owners: Mutex<(HashMap<String, Vec<String>>, Option<Instant>)>,
    http: reqwest::Client,
}

/// 未分组 key 回落:`group_name` 为空(或鉴权降级无 group)时用 router 主组。
fn resolve_group<'a>(key_group: Option<&'a str>, default: &'a str) -> &'a str {
    match key_group {
        Some(g) if !g.is_empty() => g,
        _ => default,
    }
}

/// 组名 → **持有该组成员的 owner 集合**,带 TTL 的内存快照。
///
/// 与 worker 侧成员边同步同频:admin 改完最迟 [`GROUP_OWNERS_TTL`] 生效。读库失败时
/// **沿用上一份快照**而不是清空 —— 控制面抖动不该让数据面整体 503。
const GROUP_OWNERS_TTL: Duration = Duration::from_secs(15);

fn owners_for(st: &RouterState, group: &str) -> Vec<String> {
    // 无库(stub 模式):回落到"组名即 owner",与重构前的拓扑假设一致。
    let Some(store) = st.store.as_ref() else {
        return vec![group.to_string()];
    };
    // 先只在锁内判新鲜度并取快照,**绝不在锁内查 SQLite**:查询握着全局锁会让一次控制面
    // 抖动变成整个数据面的队头阻塞(对抗审查 Architect#4)。
    {
        let cache = st.group_owners.lock();
        if cache.1.is_some_and(|t| Instant::now().duration_since(t) < GROUP_OWNERS_TTL) {
            return cache.0.get(group).cloned().unwrap_or_default();
        }
    }
    match store.group_owners() {
        Ok(m) => {
            let owners = m.get(group).cloned().unwrap_or_default();
            let mut cache = st.group_owners.lock();
            cache.0 = m;
            cache.1 = Some(Instant::now());
            owners
        }
        Err(e) => {
            let cache = st.group_owners.lock();
            match cache.1 {
                // 已有快照:沿用旧的继续服务,别让控制面抖动打穿数据面。
                Some(_) => {
                    tracing::warn!("读取分组 owner 映射失败,沿用上轮快照: {e}");
                    cache.0.get(group).cloned().unwrap_or_default()
                }
                // **冷启动**首次就失败:空映射会让每一个组都 503。回落到"组名即 owner"
                // ——那正是本次重构之前的拓扑假设,对单 owner 部署(现状)完全正确。
                None => {
                    tracing::error!("冷启动读取 owner 映射失败,暂按'组名即 owner'服务: {e}");
                    vec![group.to_string()]
                }
            }
        }
    }
}

/// 亲和表键 = `(group, session_id)` 复合键。同一 session_id 在不同组下是**独立**的
/// pin(审查共识:仅用 session_id 时,上层网关给两个客户发同一 user_id/会话键会让
/// 两组互相驱逐对方的亲和、抖动负载与缓存)。组名与 session_id 都不含 NUL,用 NUL 分隔。
fn affinity_key(group: &str, session_id: &str) -> String {
    format!("{group}\u{0}{session_id}")
}

/// 是否是合法的 instances 配置文件名:`instances.yaml` 或 `instances-<seg>.yaml`
/// (单段、无额外点号)。排除备份/样例:`instances.old.yaml`、`instances-x.bak.yaml`、
/// `instances.example.yaml` 等(审查 Skeptic#8/Architect#6:避免聚合到陈旧/他人配置)。
fn is_instances_config_file(name: &str) -> bool {
    if name == "instances.yaml" {
        return true;
    }
    match name.strip_prefix("instances-").and_then(|s| s.strip_suffix(".yaml")) {
        Some(seg) => !seg.is_empty() && !seg.contains('.'),
        None => false,
    }
}

/// 跨 router 运行态可见性:admin 的 worker 扇出应覆盖**所有** worker,而不仅本 router
/// 路由的那批——多 router 部署时(kiro 主 router + 独立 dario router),主面板否则看不到
/// dario worker(账号显示"离线/未服务"),也无法对 dario 账号做 reset/refresh/quota。
/// 扫描 instances 同目录下所有 `instances-*.yaml`,并出 worker(按 listen 去重,own 永在内)。
///
/// **此并集只进 `AdminState`(admin 控制面扇出:/health 聚合 + reset/refresh/quota/sync——
/// 后者本就是"全 worker 幂等扇出,非持有者回 404",故覆盖 dario worker 是正确且有益的)。
/// 数据面路由用 `RouterState.workers`(=本 router 自己的 workers),与此完全分离,路由隔离不变。**
/// 读/解析失败的文件跳过并 warn(可见性功能,静默跳过会让"面板少个 worker"难排查)。
fn aggregate_display_workers(
    instances_path: &Path,
    own: &[gw_core::config::WorkerConfig],
) -> Vec<gw_core::config::WorkerConfig> {
    let mut out: Vec<gw_core::config::WorkerConfig> = own.to_vec();
    let mut seen: std::collections::HashSet<String> =
        out.iter().map(|w| w.listen.clone()).collect();
    let dir = instances_path.parent().unwrap_or_else(|| Path::new("."));
    let Ok(entries) = std::fs::read_dir(dir) else { return out; };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !is_instances_config_file(&name) {
            continue;
        }
        let path = entry.path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(file = %path.display(), "聚合 worker:读取 instances 文件失败,跳过: {e}");
                continue;
            }
        };
        let cfg = match serde_yaml::from_str::<InstancesConfig>(&text) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(file = %path.display(), "聚合 worker:解析 instances 文件失败,跳过: {e}");
                continue;
            }
        };
        for w in cfg.workers {
            if seen.insert(w.listen.clone()) {
                out.push(w);
            }
        }
    }
    out
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

    // 数据面 worker 列表 = 跨所有 `instances*.yaml` 聚合(含各自 account_group),
    // 而非仅本 router 自己那批 —— 这样单一入口(38991)即可按 key 的分组派发到对应
    // worker 组(G0→kiro / DARIO→dario)。与 admin 展示扇出共用同一次目录扫描。
    let all_worker_cfgs = aggregate_display_workers(instances_path, &instances.workers);
    let workers: Vec<WorkerTarget> = all_worker_cfgs
        .iter()
        .map(|w| WorkerTarget {
            instance: w.instance,
            base_url: format!("http://{}", w.listen),
            account_group: w.account_group.clone(),
        })
        .collect();

    // 本 router 自身必须定义 ≥1 worker:default_group 由它的首个 worker 派生,空了
    // 会让回落组为 "",空组/降级流量全 503(审查 Skeptic#5)。聚合非空但 own 空时也拒。
    if instances.workers.is_empty() {
        anyhow::bail!("本 router 的 {} 未定义任何 worker", instances_path.display());
    }

    // 全局唯一性护栏:聚合跨文件后,以下重复都会破坏数据面正确性,提级为"启动即报错":
    //  ① account_group 重复 → 两 worker 绑同一组,各自加载同批账号 → 并发翻倍 + rolling
    //     refresh_token 互相覆盖(`config.rs` validate 在单文件内禁止此事,跨文件同理致命);
    //  ② instance 重复 → 亲和/负载按 `instance` 键,撞号会把别组 session 串到错 worker。
    // (listen 重复已被 `aggregate_display_workers` 按 listen 去重,这里不再涉及。)
    {
        let mut groups = std::collections::HashSet::new();
        let mut seen_inst = std::collections::HashSet::new();
        for w in &all_worker_cfgs {
            if !groups.insert(w.account_group.clone()) {
                anyhow::bail!(
                    "路由 worker 聚合非法:账号组 '{}' 被多个 worker 绑定(跨 instances 文件),\
                     并发与凭据刷新会互踩;请确保每组仅一个 worker",
                    w.account_group
                );
            }
            if !seen_inst.insert(w.instance) {
                anyhow::bail!(
                    "路由 worker 聚合非法:instance={} 重复(跨 instances 文件),\
                     亲和/负载按 instance 键会串号路由;请为各组 worker 使用全局唯一的 instance 号",
                    w.instance
                );
            }
        }
    }

    // 未分组 key 的回落组 = 本 router 自身 instances.yaml 的首个 worker 组(kiro 主
    // 通道即 G0)。保持历史语义:未显式分组的 key 仍打本 router 原本服务的那一组。
    let default_group = instances.workers[0].account_group.clone();

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
            // 跨 router 并集(与数据面同一份):主面板得以显示 dario worker 等其他
            // router 的运行态;数据面则在其上按组过滤路由。
            all_worker_cfgs.clone(),
            system.clone(),
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
        group_owners: Mutex::new((HashMap::new(), None)),
        workers,
        default_group,
        affinity: Mutex::new(AffinityTable::new()),
        store,
        http: reqwest::Client::new(),
    });

    let max_body = system.effective_max_request_body_bytes();
    let mut app = Router::new()
        .route("/v1/messages", post(forward))
        .route("/v1/messages/count_tokens", post(count_tokens))
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

    // 入站体积上限:客户端 base64 图片/PDF 常 >2MB,axum 默认 2MB 会在 handler 前 413 且不入库
    // (2026-06 线上实测)。提到 system.max_request_body_bytes(默认 16MB),让大请求进得来交给
    // 下游 worker 内容感知护栏(6.3MB 裁剪/压缩或清晰报错)。有界值(非 disable)防 DoS。
    // **挂在 nest 之后**:axum 的 .layer() 只包住调用时已存在的路由,放这里才能同时覆盖
    // /v1/messages 与 /admin/api(大 JSON 导入),否则 admin 仍受默认 2MB(Skeptic 审查)。
    let app = app.layer(axum::extract::DefaultBodyLimit::max(max_body));
    tracing::info!(
        max_request_body_bytes = max_body,
        "router 入站体积上限(全路由生效;需改请改 system.yaml 后重启)"
    );

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

/// `POST /v1/messages/count_tokens` —— 本地估算,零上游调用(对齐 kiro.rs 默认路径)。
///
/// NewAPI 等上层网关与部分客户端会探测/调用此端点,缺失会 404 影响兼容。
/// 估算口径 = system + 消息 text 块 + 工具定义;计费**不**依赖它(计费走上游 usage),
/// 因此在 router 直接处理,不占用 worker/账号。
async fn count_tokens(
    State(st): State<Arc<RouterState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    let authed = match authorize(&st, &headers).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // 一致性契约:与 /v1/messages、/v1/models 一样,该 key 的分组若无对应 worker 就
    // 503,而非给一个会误导探测的 200(审查 Skeptic#4/Architect#5)。本端点仍是纯本地
    // 估算,不选具体 worker、不触账号。
    // 影子组必须先映射成源组再判有没有 worker —— 影子组自己永远没有 worker。
    let group = resolve_group(authed.group.as_deref(), &st.default_group);
    let owners = owners_for(&st, group);
    if !st.workers.iter().any(|w| owners.iter().any(|o| o == &w.account_group)) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("分组 '{group}' 无可用 worker"),
        )
            .into_response();
    }
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"type":"error","error":{
                    "type":"invalid_request_error","message": format!("无效 JSON: {e}")}})),
            )
                .into_response();
        }
    };
    let tokens = gw_kiro::text_tokens::count_request_tokens(&parsed);
    Json(serde_json::json!({"input_tokens": tokens})).into_response()
}

async fn forward(
    State(st): State<Arc<RouterState>>,
    headers: HeaderMap,
    body: Bytes,
) -> axum::response::Response {
    // ① 鉴权(拿到客户 key 用于归属 + 分组用于派发)。
    let authed = match authorize(&st, &headers).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    let client_key = authed.key_id;
    // 请求所属分组:worker 据此取成员视图。组名同时用于选 worker(谁持有本组成员)。
    let group = resolve_group(authed.group.as_deref(), &st.default_group);

    // ② 在该分组的 worker 子集内选号(会话亲和)。子集为空 = 该组无对应 worker:
    // 明确 503,而非静默把请求错喂给别组 worker(此前数据面无组逻辑的根因)。
    let session_id = parse_session_id(&body);
    let target = pick_worker(&st, session_id.as_deref(), group);
    let Some(target) = target else {
        tracing::warn!(group, "请求命中无可用 worker 的分组,拒绝");
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("分组 '{group}' 无可用 worker"),
        )
            .into_response();
    };

    // ③ 转发(流式透传)。send() 失败 = worker 进程可能已挂但仍在配置里(连接拒绝):
    // 对这种**未送达**的请求做一次故障转移——丢弃指向故障实例的亲和、换 worker 重发,
    // 否则该 session 会钉死故障 worker 502 长达 AFFINITY_TTL(审查 Architect#1)。
    // client 未设总超时,Err 基本是 connect 级错误,重复送达上游的风险可忽略。
    let mut target = target;
    let mut failed_over = false;
    loop {
        match send_messages_to_worker(&st, &target, &headers, &body, client_key.as_deref(), Some(group))
            .await
        {
            Ok(resp) => return proxy_response(resp),
            Err(e) => {
                tracing::error!(instance = target.instance, "转发到 worker 失败: {e}");
                if failed_over {
                    return (StatusCode::BAD_GATEWAY, "worker 不可达").into_response();
                }
                match failover_target(&st, session_id.as_deref(), group, target.instance) {
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
    group: Option<&str>,
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
    // 请求所属分组:worker 据此取成员视图。这是**白名单转发**——客户端自己带的
    // x-gw-group 到不了 worker,伪造不出本分组以外的账号。
    if let Some(g) = group {
        req = req.header(GROUP_HEADER, g);
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

/// 转发失败后的故障转移:丢弃指向故障实例的会话亲和,在**同组的其余** worker 里按
/// 活跃负载重选并重钉亲和。故障转移**不得跨组**(否则把 DARIO 请求转到 G0 worker)。
/// 同组无其他 worker 可选时返回 None(调用方 502)。
fn failover_target(
    st: &RouterState,
    session_id: Option<&str>,
    group: &str,
    failed_instance: u32,
) -> Option<WorkerTarget> {
    let owners = owners_for(st, group);
    let candidates: Vec<WorkerTarget> = st
        .workers
        .iter()
        .filter(|w| {
            owners.iter().any(|o| o == &w.account_group) && w.instance != failed_instance
        })
        .cloned()
        .collect();
    let mut aff = st.affinity.lock();
    if let Some(sid) = session_id {
        let key = affinity_key(group, sid);
        if aff.entries.get(&key).map(|e| e.instance) == Some(failed_instance) {
            aff.entries.remove(&key);
        }
    }
    let chosen = least_loaded_locked(&candidates, &aff.entries)?;
    if let Some(sid) = session_id {
        aff.entries.insert(
            affinity_key(group, sid),
            AffinityEntry {
                instance: chosen.instance,
                last_seen: Instant::now(),
            },
        );
    }
    Some(chosen)
}

/// 鉴权结果(通过时)。`key_id`/`group` 为 `None` 表示 store=None 的 P0 降级放行
/// (无归属、无分组,路由回落到 router 主组)。
struct Authed {
    /// 客户 key(用量归属;X-Gw-Client-Key 透传给 worker)。
    key_id: Option<String>,
    /// 客户 key 所属分组(路由派发依据;'' = 未分组 → 回落主组)。
    /// worker 据此取成员视图 —— 每请求现读 DB,admin 改成员边下一个请求即生效。
    group: Option<String>,
}

/// 鉴权(forward / forward_models / count_tokens 共用)。
/// `Ok(Authed{..})`=通过(降级放行时字段为 None);`Err(resp)`=拒绝(401/429/500)。
async fn authorize(
    st: &RouterState,
    headers: &HeaderMap,
) -> Result<Authed, axum::response::Response> {
    let Some(store) = st.store.as_ref() else {
        // P0:无控制面库,放行且无归属/无分组(路由用 default_group)。
        return Ok(Authed { key_id: None, group: None });
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
            Ok(Some(auth)) => Ok(Authed {
                key_id: Some(auth.key_id),
                group: Some(auth.group_name),
            }),
            Ok(None) => Err(unauthorized("无效 API key")),
            Err(e) => {
                tracing::error!("鉴权查询失败: {e}");
                Err((StatusCode::INTERNAL_SERVER_ERROR, "鉴权失败").into_response())
            }
        },
        None => Err(unauthorized("缺少 API key(x-api-key 或 Authorization: Bearer)")),
    }
}

/// `GET /v1/models` —— 模型目录与会话无关,鉴权后转发到**本 key 分组**内最空的 worker。
///
/// 按组路由(B)后:不同分组对应不同 provider 家族(G0/kiro、DARIO/dario),目录因
/// provider 而异。这里按 key 的分组选该组代表 worker,避免 dario 客户拿到 kiro 目录
/// (反之亦然)。组内多 worker 时取最空者(同组同 provider,目录一致)。
async fn forward_models(
    State(st): State<Arc<RouterState>>,
    headers: HeaderMap,
) -> axum::response::Response {
    let authed = match authorize(&st, &headers).await {
        Ok(a) => a,
        Err(resp) => return resp,
    };
    // 这里同样要按成员归属选 worker:漏了就会让某个分组的 key 探测 /v1/models 拿 503,
    // NewAPI / Claude Code 会直接把整条渠道判为不可用。
    let group = resolve_group(authed.group.as_deref(), &st.default_group);
    // 无 session:在该组子集内取最空 worker(无亲和记忆)。
    let Some(target) = pick_worker(&st, None, group) else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            format!("分组 '{group}' 无可用 worker"),
        )
            .into_response();
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

/// 选 worker:在 `group` 对应的 worker 子集内,命中亲和则复用,否则选活跃负载最低者
/// 并记亲和。子集为空(该组无 worker)返回 None,调用方据此 503(不跨组错路由)。
fn pick_worker(st: &RouterState, session_id: Option<&str>, group: &str) -> Option<WorkerTarget> {
    pick_worker_at(st, session_id, group, Instant::now())
}

/// `now` 注入便于测试过期语义(Instant 无法伪造,只能向前偏移)。
fn pick_worker_at(
    st: &RouterState,
    session_id: Option<&str>,
    group: &str,
    now: Instant,
) -> Option<WorkerTarget> {
    // 先把候选缩到本组子集:亲和复用与负载均衡都只在组内进行。
    let owners = owners_for(st, group);
    let subset: Vec<WorkerTarget> = st
        .workers
        .iter()
        .filter(|w| owners.iter().any(|o| o == &w.account_group))
        .cloned()
        .collect();
    if subset.is_empty() {
        return None; // 该分组无对应 worker。
    }

    let mut aff = st.affinity.lock();
    aff.cleanup_if_due(now);

    if let Some(sid) = session_id {
        let key = affinity_key(group, sid);
        if let Some(entry) = aff.entries.get_mut(&key) {
            // 命中路径 O(1) 精确判过期:全表清理是节流的,不能依赖它兜底。
            if now.duration_since(entry.last_seen) < AFFINITY_TTL {
                let instance = entry.instance;
                // 键已含组,pin 天然属本组;仍校验 pin 落在子集内,兜底拓扑变更
                // (worker 被移除)指向已不存在的 instance → 丢弃重选。
                if let Some(w) = subset.iter().find(|w| w.instance == instance) {
                    entry.last_seen = now;
                    return Some(w.clone());
                }
            }
            // 已过期、或 pin 指向已不存在的 worker:丢弃,走组内重选。
            aff.entries.remove(&key);
        }
        // 未命中:组内选最空 worker,记亲和(插入即计入活跃负载)。
        let chosen = least_loaded_locked(&subset, &aff.entries)?;
        aff.entries.insert(
            key,
            AffinityEntry {
                instance: chosen.instance,
                last_seen: now,
            },
        );
        return Some(chosen);
    }

    // 无 session_id:仍在组内选最空 worker(无亲和记忆,不计负载)。
    least_loaded_locked(&subset, &aff.entries)
}

/// 活跃负载 = 亲和表里钉在该 worker 上的未过期 session 数。
/// (此前用只增不减的累计计数:会话过期后负载不回落,空闲 worker 拿不到新会话。)
/// `workers` 已是按组过滤后的子集;`aff` 含全组 session,但只对子集 instance 取计数,
/// 别组 session 的 instance 不在子集中,天然被忽略(组内负载相互独立)。
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

/// 提取客户端 API key。**两种鉴权头都认**:
/// 1. `x-api-key: <key>` —— **Anthropic 标准头**(真 Anthropic API 用它)。NewAPI 的
///    Claude/Anthropic 渠道、Anthropic SDK、Claude Code 指到本网关都发这个;旧版只认
///    `Authorization` → 这些客户端一律 401,上游(如 NewAPI)再包装成 500。
/// 2. `Authorization: Bearer <key>`(或直接把 key 放进 Authorization)—— OpenAI 风格。
///
/// 两者都在时优先 `x-api-key`(本网关对外是 Anthropic 线缆)。空白值跳过、回退下一种。
fn extract_bearer(headers: &HeaderMap) -> Option<String> {
    if let Some(k) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        let k = k.trim();
        if !k.is_empty() {
            return Some(k.to_string());
        }
    }
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .trim();
    let key = raw
        .strip_prefix("Bearer ")
        .or_else(|| raw.strip_prefix("bearer "))
        .unwrap_or(raw) // 兼容把 key 直接放进 Authorization(无 Bearer 前缀)。
        .trim();
    (!key.is_empty()).then(|| key.to_string())
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

    fn hdr(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    #[test]
    fn extract_bearer_accepts_x_api_key_and_authorization() {
        // Anthropic 标准头(NewAPI Claude 渠道 / Anthropic SDK / Claude Code 都发这个)。
        assert_eq!(
            extract_bearer(&hdr(&[("x-api-key", "sk-abc")])).as_deref(),
            Some("sk-abc")
        );
        // OpenAI 风格 Bearer。
        assert_eq!(
            extract_bearer(&hdr(&[("authorization", "Bearer sk-def")])).as_deref(),
            Some("sk-def")
        );
        // 直接把 key 放进 Authorization(无 Bearer 前缀)。
        assert_eq!(
            extract_bearer(&hdr(&[("authorization", "sk-ghi")])).as_deref(),
            Some("sk-ghi")
        );
        // 两者都在 → 优先 x-api-key(本网关对外是 Anthropic 线缆)。
        assert_eq!(
            extract_bearer(&hdr(&[("x-api-key", "sk-xak"), ("authorization", "Bearer sk-auth")]))
                .as_deref(),
            Some("sk-xak")
        );
        // 空 x-api-key 回退到 Authorization。
        assert_eq!(
            extract_bearer(&hdr(&[("x-api-key", "  "), ("authorization", "Bearer sk-fallback")]))
                .as_deref(),
            Some("sk-fallback")
        );
        // 都没有 → None。
        assert_eq!(extract_bearer(&hdr(&[("accept", "application/json")])), None);
    }

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

    /// 用与生产 router/worker 同形的 layer(读 SystemConfig 的有效上限)验证 DefaultBodyLimit
    /// 机制:<=上限放行、>上限回 413。锁住"客户端大图片/PDF 不再在入口被框架闷死"的契约,
    /// 并防 axum 升级改变默认行为时悄悄回归。覆盖 `Bytes`(router 用)与 `Json`(worker 用)
    /// 两种全量缓冲提取器,以及精确边界(N 放行 / N+1 拒)。
    #[tokio::test]
    async fn default_body_limit_layer_allows_under_and_rejects_over() {
        use axum::body::Body;
        use tower::ServiceExt; // oneshot

        async fn echo_bytes(body: Bytes) -> String {
            body.len().to_string()
        }
        async fn echo_json(Json(v): Json<serde_json::Value>) -> String {
            v.to_string()
        }

        // 取小上限(64B)构造同形 layer(经 effective_max_request_body_bytes,显式值不回落)。
        let mut cfg = gw_core::config::SystemConfig::default();
        cfg.max_request_body_bytes = 64;
        let limit = cfg.effective_max_request_body_bytes();
        assert_eq!(limit, 64); // 显式值不被 0 回落覆盖。

        // Bytes(router /v1/messages 用)+ Json(worker messages 用)两条都挂同一 layer。
        let app: Router = Router::new()
            .route("/b", post(echo_bytes))
            .route("/j", post(echo_json))
            .layer(axum::extract::DefaultBodyLimit::max(limit));

        let post = |uri: &str, n: usize| {
            // /j 需合法 JSON;用一个长度可控的 JSON 字符串。
            let body = if uri == "/j" {
                let pad = "x".repeat(n.saturating_sub(9).max(1)); // {"k":"…"} 约 9 字节壳
                format!("{{\"k\":\"{pad}\"}}")
            } else {
                "x".repeat(n)
            };
            axum::http::Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body))
                .unwrap()
        };
        let status = |app: Router, uri: &'static str, n: usize| async move {
            app.oneshot(post(uri, n)).await.unwrap().status()
        };

        // Bytes:32B 放行;精确边界 64B 放行、65B 拒;200B 拒。
        assert_eq!(status(app.clone(), "/b", 32).await, StatusCode::OK);
        assert_eq!(status(app.clone(), "/b", 64).await, StatusCode::OK);
        assert_eq!(
            status(app.clone(), "/b", 65).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
        assert_eq!(
            status(app.clone(), "/b", 200).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );

        // Json:小 body 放行;超限 body 在提取阶段 413(handler 不执行)。
        assert_eq!(status(app.clone(), "/j", 20).await, StatusCode::OK);
        assert_eq!(
            status(app.clone(), "/j", 400).await,
            StatusCode::PAYLOAD_TOO_LARGE
        );
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

    /// n 个 worker,全部同一组 "G0"(覆盖原有的组内亲和/负载语义)。
    fn mk_state(n: u32) -> RouterState {
        mk_state_grouped((0..n).map(|i| (i, "G0".to_string())).collect())
    }

    /// 按 (instance, group) 列表构造,用于按组路由测试。
    fn mk_state_grouped(specs: Vec<(u32, String)>) -> RouterState {
        let default_group = specs.first().map(|(_, g)| g.clone()).unwrap_or_default();
        RouterState {
            group_owners: Mutex::new((HashMap::new(), None)),
            workers: specs
                .into_iter()
                .map(|(i, g)| WorkerTarget {
                    instance: i,
                    base_url: format!("http://127.0.0.1:{}", 9000 + i),
                    account_group: g,
                })
                .collect(),
            default_group,
            affinity: Mutex::new(AffinityTable::new()),
            store: None,
            http: reqwest::Client::new(),
        }
    }

    #[test]
    fn affinity_sticks_same_session_to_same_worker() {
        let st = mk_state(3);
        let first = pick_worker(&st, Some("s1"), "G0").unwrap();
        for _ in 0..5 {
            let again = pick_worker(&st, Some("s1"), "G0").unwrap();
            assert_eq!(again.instance, first.instance, "同 session 必须钉同 worker");
        }
    }

    #[test]
    fn new_sessions_spread_across_workers() {
        let st = mk_state(2);
        let a = pick_worker(&st, Some("sa"), "G0").unwrap();
        let b = pick_worker(&st, Some("sb"), "G0").unwrap();
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
            .map(|sid| pick_worker_at(&st, Some(sid), "G0", t0).unwrap().instance)
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
        assert_eq!(pick_worker_at(&st, Some("n1"), "G0", t1).unwrap().instance, a);
        assert_eq!(pick_worker_at(&st, Some("n2"), "G0", t1).unwrap().instance, a);
    }

    #[test]
    fn stale_affinity_to_removed_worker_repicks() {
        let st = mk_state(2);
        let key = affinity_key("G0", "ghost");
        st.affinity.lock().entries.insert(
            key.clone(),
            AffinityEntry { instance: 99, last_seen: Instant::now() },
        );
        // 亲和指向已不存在的 worker(拓扑变更):应重选一个真实 worker 并修正亲和。
        let picked = pick_worker(&st, Some("ghost"), "G0").expect("应能重选真实 worker");
        assert!(st.workers.iter().any(|w| w.instance == picked.instance));
        assert_eq!(st.affinity.lock().entries.get(&key).unwrap().instance, picked.instance);
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
                .insert(affinity_key("G0", "s1"), AffinityEntry { instance: 1, last_seen: t0 });
            aff.last_cleanup = t1;
        }
        // 命中路径必须 O(1) 精确判过期,不依赖节流的全表清理兜底:
        // 两 worker 均无活跃负载,重选 tie 取 workers[0]=instance 0。
        let picked = pick_worker_at(&st, Some("s1"), "G0", t1).unwrap();
        assert_eq!(picked.instance, 0, "过期 pin 不得被复用");
    }

    #[test]
    fn failover_moves_session_off_dead_worker_and_repins() {
        let st = mk_state(2);
        let first = pick_worker(&st, Some("s1"), "G0").unwrap();
        let next =
            failover_target(&st, Some("s1"), "G0", first.instance).expect("双 worker 应有备选");
        assert_ne!(next.instance, first.instance, "备选必须避开故障实例");
        assert_eq!(
            st.affinity.lock().entries.get(&affinity_key("G0", "s1")).unwrap().instance,
            next.instance,
            "亲和应重钉到备选 worker"
        );
    }

    #[test]
    fn failover_none_when_no_alternative() {
        let st = mk_state(1);
        let only = pick_worker(&st, Some("x"), "G0").unwrap();
        assert!(
            failover_target(&st, Some("x"), "G0", only.instance).is_none(),
            "单 worker 无备选,只能 502"
        );
    }

    #[test]
    fn empty_group_resolves_to_default() {
        // 显式分组按原样;空/缺失回落到 router 主组(向后兼容:未分组 key 仍打 kiro)。
        assert_eq!(resolve_group(Some("DARIO"), "G0"), "DARIO");
        assert_eq!(resolve_group(Some(""), "G0"), "G0");
        assert_eq!(resolve_group(None, "G0"), "G0");
    }

    #[test]
    fn routes_to_matching_group_only() {
        // 一套入口、两组 worker:G0 key 永远落 G0 worker,DARIO key 永远落 DARIO worker。
        // (B 的核心契约:不再把 DARIO 请求静默错喂给 G0/kiro。)
        let st = mk_state_grouped(vec![(0, "G0".into()), (1, "DARIO".into())]);
        for _ in 0..5 {
            assert_eq!(pick_worker(&st, Some("g0-sess"), "G0").unwrap().instance, 0);
            assert_eq!(
                pick_worker(&st, Some("dario-sess"), "DARIO").unwrap().instance,
                1
            );
        }
    }

    #[test]
    fn unknown_group_yields_no_worker() {
        // 配了分组但无对应 worker(如 G1)→ None,调用方 503;绝不回落到别组。
        let st = mk_state_grouped(vec![(0, "G0".into()), (1, "DARIO".into())]);
        assert!(pick_worker(&st, Some("s"), "G1").is_none());
        assert!(pick_worker(&st, None, "G1").is_none(), "/v1/models 路径同样不跨组");
    }

    #[test]
    fn same_session_id_across_groups_keeps_independent_pins() {
        // 同一 session 键在两组下是独立 pin:G0 钉 instance 0、DARIO 钉 instance 1,
        // 且彼此**不驱逐**(审查共识修复:复合键 (group,sid) 杜绝跨组亲和抖动)。
        let st = mk_state_grouped(vec![(0, "G0".into()), (1, "DARIO".into())]);
        assert_eq!(pick_worker(&st, Some("shared"), "G0").unwrap().instance, 0);
        assert_eq!(pick_worker(&st, Some("shared"), "DARIO").unwrap().instance, 1);
        // 关键:DARIO 那次不得抹掉 G0 的 pin —— 再查 G0 仍稳定在 0。
        assert_eq!(pick_worker(&st, Some("shared"), "G0").unwrap().instance, 0);
        assert_eq!(pick_worker(&st, Some("shared"), "DARIO").unwrap().instance, 1);
        // 两组各自一条亲和记录,共存。
        assert_eq!(st.affinity.lock().entries.len(), 2);
    }

    #[test]
    fn failover_stays_within_group() {
        // G0 两台 + DARIO 一台:G0 故障转移只在 G0 内换,绝不跳到 DARIO。
        let st = mk_state_grouped(vec![(0, "G0".into()), (1, "G0".into()), (2, "DARIO".into())]);
        let first = pick_worker(&st, Some("s1"), "G0").unwrap();
        assert_eq!(first.account_group, "G0");
        let next = failover_target(&st, Some("s1"), "G0", first.instance).unwrap();
        assert_eq!(next.account_group, "G0", "故障转移不得跨组");
        assert_ne!(next.instance, first.instance);
    }

    #[test]
    fn instances_config_file_glob_excludes_backups() {
        assert!(is_instances_config_file("instances.yaml"));
        assert!(is_instances_config_file("instances-dario.yaml"));
        assert!(is_instances_config_file("instances-us_pool.yaml"));
        // 备份/样例/陈旧文件必须排除,否则会聚合到他人/过期 worker。
        assert!(!is_instances_config_file("instances.old.yaml"));
        assert!(!is_instances_config_file("instances-dario.bak.yaml"));
        assert!(!is_instances_config_file("instances.example.yaml"));
        assert!(!is_instances_config_file("instances.yaml.bak"));
        assert!(!is_instances_config_file("docker-compose.yml"));
        assert!(!is_instances_config_file("instances-.yaml"));
    }

    // ───────── 影子组(低价档)路由 ─────────
}
