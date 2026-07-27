//! admin 上游账号管理端点(切片④)。
//!
//! - `GET    /accounts`          配置态列表(extra 中含 token 的字段已脱敏)
//! - `GET    /accounts/runtime`  运行态聚合:逐 worker 拉 /health 的调度器快照
//! - `POST   /accounts`          新增(凭据 JSON 进 extra;重复 409)
//! - `PATCH  /accounts/{id}`     部分更新 group/并发/禁用/extra(404 = 不存在)
//! - `DELETE /accounts/{id}`     删除(usage 历史归属保留)
//!
//! 改动经 worker 的 30s 周期 sync 生效,无需重启。DB 是账号事实源;
//! 运行态(冷却/封禁/在途并发)只存在于 worker 内存,经 /health 暴露。

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use gw_core::config::SystemSettings;
use gw_core::store::{AccountPatch, AccountRow};
use serde::Deserialize;

use super::{internal_error, redact_proxy_url, validate_proxy_url, AdminState};

/// 从设置 overlay 读 `egress_pool`(trim、去空);解析失败/未配置 → 空 Vec。
fn read_egress_pool(st: &AdminState) -> Vec<String> {
    let overlay: SystemSettings = match st.store.get_settings() {
        Ok(Some(j)) => serde_json::from_str(&j).unwrap_or_default(),
        _ => SystemSettings::default(),
    };
    overlay
        .egress_pool
        .unwrap_or_default()
        .into_iter()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// 取账号 extra JSON 里非空的 `proxy`;无/空 → None。
fn account_proxy(extra_json: &str) -> Option<String> {
    let extra: serde_json::Map<String, serde_json::Value> = serde_json::from_str(extra_json).ok()?;
    extra
        .get("proxy")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// 出口池「最少使用」分配器:把新号粘到当前分配最少的池 URL,使账号均衡铺满 N 个出口 IP
/// (每号固定一个,粘性)。计数初值 = 现有账号已分配到各池 URL 的数量;每分配一次本地计数 +1,
/// 保证同一批导入内也均匀(而非全堆到第一个)。
struct EgressAssigner {
    /// (池 URL, 当前已分配账号数)。
    counts: Vec<(String, usize)>,
}

impl EgressAssigner {
    /// 从设置 `egress_pool` + 现有账号分布构造;池为空/未配置 → None(不自动分配)。
    fn from_settings(st: &AdminState) -> Option<Self> {
        let pool = read_egress_pool(st);
        if pool.is_empty() {
            return None;
        }
        let mut counts: Vec<(String, usize)> = pool.into_iter().map(|u| (u, 0usize)).collect();
        if let Ok(rows) = st.store.list_accounts() {
            for row in &rows {
                if let Some(p) = account_proxy(&row.extra) {
                    if let Some(c) = counts.iter_mut().find(|(u, _)| *u == p) {
                        c.1 += 1;
                    }
                }
            }
        }
        Some(Self { counts })
    }

    /// 取当前分配最少的池 URL,并把其本地计数 +1(下一个号即感知)。
    fn next(&mut self) -> Option<String> {
        let idx = self
            .counts
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, c))| *c)
            .map(|(i, _)| i)?;
        self.counts[idx].1 += 1;
        Some(self.counts[idx].0.clone())
    }
}

/// 上号(导入/新建)时选择的出口网关解析。前端下拉回传 `egress`:
/// - `None`/`""`/`"direct"` → 直连(不设 proxy);
/// - `"auto"` → 最少使用自动分配(`EgressAssigner`,把号均衡铺满各网关);
/// - 数字索引 → `egress_pool[i]`(选定网关,本批所有新号都用它)。
enum EgressPicker {
    Direct,
    Fixed(String),
    Auto(EgressAssigner),
}

impl EgressPicker {
    fn build(st: &AdminState, sel: Option<&str>) -> Self {
        let pool = read_egress_pool(st);
        match sel.map(str::trim) {
            None | Some("") | Some("direct") => EgressPicker::Direct,
            Some("auto") => match EgressAssigner::from_settings(st) {
                Some(a) => EgressPicker::Auto(a),
                None => EgressPicker::Direct,
            },
            Some(s) => match s.parse::<usize>() {
                Ok(i) if i < pool.len() => EgressPicker::Fixed(pool[i].clone()),
                // 无效索引(网关被删/越界)→ 退回直连,绝不乱投到错误出口。
                _ => EgressPicker::Direct,
            },
        }
    }

    /// 取本账号应写入 `extra.proxy` 的 URL(直连=None;选定=固定;自动=最少使用)。
    fn next(&mut self) -> Option<String> {
        match self {
            EgressPicker::Direct => None,
            EgressPicker::Fixed(u) => Some(u.clone()),
            EgressPicker::Auto(a) => a.next(),
        }
    }
}

/// account_id 规则:1–64 个 URL-safe 字符(进路径段)。
fn validate_account_id(id: &str) -> Result<(), &'static str> {
    if id.is_empty() || id.len() > 64 {
        return Err("account_id 长度须在 1–64 字符之间");
    }
    if !id
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~'))
    {
        return Err("account_id 只能含字母、数字及 - _ . ~");
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct CreateAccountBody {
    account_id: String,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    max_concurrency: Option<i64>,
    /// 调度优先级:写入 extra.priority(数值越小越优先,缺省 100);缺省不写(scheduler 视作 100)。
    #[serde(default)]
    priority: Option<i64>,
    /// provider 专属凭据字段(refresh_token 等),原样存为 extra JSON。
    #[serde(default)]
    extra: Option<serde_json::Map<String, serde_json::Value>>,
    /// 上号选择的出口网关:""/缺省/"direct"=直连;"auto"=自动均衡;数字=egress_pool 索引。
    /// 仅在 extra 未显式带 proxy 时生效。
    #[serde(default)]
    egress: Option<String>,
    /// claude-dario 专用:粘贴 CC .credentials.json 全文。
    /// 后端调 `gw_dario::parse_cc_credentials` 解析后并入 extra(不覆盖显式传的同名字段)。
    #[serde(default)]
    credentials_json: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UpdateAccountBody {
    #[serde(default)]
    group_name: Option<String>,
    #[serde(default)]
    max_concurrency: Option<i64>,
    #[serde(default)]
    disabled: Option<bool>,
    /// 整体替换 extra(凭据轮换);缺省不动。
    #[serde(default)]
    extra: Option<serde_json::Map<String, serde_json::Value>>,
    /// 定点更新出口代理(走 merge_account_extra,**绝不**碰其它凭据字段)。
    /// 字段缺省=不动 proxy;`""`(空串)=清除;非空串=设代理 URL。
    /// (用 `Option<String>` 而非 `Option<Value>`:serde 会把 JSON `null` 折叠成 `None`,
    /// 无法区分"清除"与"不动";故清除约定为传空串。)
    #[serde(default)]
    proxy_url: Option<String>,
    /// 定点更新调度优先级(写 extra.priority,数值越小越优先,缺省 100)。
    /// 缺省=不动;走 merge_account_extra 绝不碰凭据(仿 proxy_url)。
    #[serde(default)]
    priority: Option<i64>,
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/accounts", get(list_accounts).post(create_account))
        .route("/accounts/oauth/start", post(oauth_start))
        .route("/accounts/oauth/complete", post(oauth_complete))
        .route("/accounts/import", post(import_accounts))
        .route("/accounts/import-apikeys", post(import_apikeys))
        .route("/accounts/rebalance-egress", post(rebalance_egress))
        .route("/accounts/runtime", get(runtime))
        .route("/accounts/{id}", patch(update_account).delete(delete_account))
        .route("/accounts/{id}/reset", post(reset_account))
        .route("/accounts/{id}/refresh", post(refresh_account))
        .route("/accounts/{id}/quota", post(quota_account))
}

fn api_error(status: StatusCode, msg: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({"type":"error","error":{"message": msg}})),
    )
        .into_response()
}

/// 把 AccountRow 转为对外视图:extra 解析成对象并把含 token/secret/password
/// 的字段脱敏(保尾 4 位)。凭据只进不出——admin 页展示概要即可,完整值
/// 留在库里供 worker 用。
fn redacted_view(row: AccountRow) -> serde_json::Value {
    let extra: serde_json::Value =
        serde_json::from_str(&row.extra).unwrap_or(serde_json::Value::Null);
    let extra = match extra {
        serde_json::Value::Object(map) => {
            let redacted: serde_json::Map<String, serde_json::Value> = map
                .into_iter()
                .map(|(k, v)| {
                    // proxy URL 可含 user:pass@(代理订阅密钥):掩码密码段,保留 host 供识别
                    // (审查 Architect#3)。真实值仍在库里,resolver 用真实值发包。
                    if k == "proxy" {
                        if let Some(s) = v.as_str() {
                            return (k, serde_json::json!(redact_proxy_url(s)));
                        }
                        return (k, v);
                    }
                    let lk = k.to_lowercase();
                    // 含 key 也算敏感:kiro_api_key / anthropic_api_key 等不含 token/secret/password
                    // 的凭据字段否则会经 GET 明文泄漏(本仓库凭据语境下 *_key 一律视为机密)。
                    let sensitive = lk.contains("token")
                        || lk.contains("secret")
                        || lk.contains("password")
                        || lk.contains("key");
                    let v = if sensitive {
                        // 按字符(非字节)取尾 4 位:非 ASCII 密钥按字节切会落在
                        // UTF-8 编码中间直接 panic(审查 Minimalist#5)。
                        match v.as_str() {
                            Some(s) if s.chars().count() > 6 => {
                                let tail: String =
                                    s.chars().skip(s.chars().count() - 4).collect();
                                serde_json::json!(format!("***{tail}"))
                            }
                            _ => serde_json::json!("***"),
                        }
                    } else {
                        v
                    };
                    (k, v)
                })
                .collect();
            serde_json::Value::Object(redacted)
        }
        other => other,
    };
    // 调度优先级顶层吐出(数值越小越优先,缺省 100;与 worker/scheduler.rs 分层 LRU 口径一致)。
    // priority 是普通数值、键名不含 token/secret/password/key,不被上面的脱敏改写,读脱敏后的
    // extra 与读原始值等价。
    let priority = extra.get("priority").and_then(|v| v.as_i64()).unwrap_or(100);
    serde_json::json!({
        "account_id": row.account_id,
        "group_name": row.group_name,
        "provider": row.provider,
        "max_concurrency": row.max_concurrency,
        "priority": priority,
        "disabled": row.disabled,
        "extra": extra,
        "created_at": row.created_at,
        // 累计成功/失败请求计数(监控用,非计费)。前端账号页展示"累计成功/失败"列。
        "success_count": row.success_count,
        "failure_count": row.failure_count,
    })
}

/// 账号的目标组必须**存在**且**不是影子组**。
///
/// - 存在:防"幽灵分组"——typo 的组名会让账号永远不被任何 worker 服务,groups 页也看不见。
/// - 非影子:影子组不绑 worker(它只是源组的可见性视图),分进去的账号同样永远不会被
///   任何 scheduler 加载,但 admin 列表还会显示 account_count > 0,更具迷惑性。
///
/// `Ok(())` = 放行(含空组名 = 未分组);`Err(resp)` = 已构造好的 400/500 响应。
fn require_real_group(st: &AdminState, group: &str) -> Result<(), axum::response::Response> {
    if group.is_empty() {
        return Ok(());
    }
    match st.store.list_groups() {
        Ok(rows) => match rows.iter().find(|g| g.name == group) {
            None => Err(api_error(StatusCode::BAD_REQUEST, "分组不存在")),
            Some(g) if !g.shadow_of.is_empty() => Err(api_error(
                StatusCode::BAD_REQUEST,
                "目标是影子组(低价档),它不持有账号;请分到其源组",
            )),
            Some(_) => Ok(()),
        },
        Err(e) => Err(internal_error(e)),
    }
}

async fn list_accounts(State(st): State<AdminState>) -> axum::response::Response {
    match st.store.list_accounts() {
        Ok(rows) => Json(rows.into_iter().map(redacted_view).collect::<Vec<_>>()).into_response(),
        Err(e) => internal_error(e),
    }
}

async fn create_account(
    State(st): State<AdminState>,
    Json(body): Json<CreateAccountBody>,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&body.account_id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    // extra.proxy(若有,非空)写入边界校验 + 归一为 trim 后的值(fail-closed)。
    let mut extra_map = body.extra.clone().unwrap_or_default();
    if let Some(p) = extra_map.get("proxy").and_then(|v| v.as_str()).map(str::to_string) {
        let trimmed = p.trim();
        if trimmed.is_empty() {
            extra_map.remove("proxy");
        } else {
            match validate_proxy_url(trimmed) {
                Ok(valid) => {
                    extra_map.insert("proxy".into(), serde_json::json!(valid));
                }
                Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
            }
        }
    }
    // 无显式 proxy 且设置里配了 egress_pool → 自动分配最少使用的出口 IP(粘性)。
    let has_proxy = extra_map
        .get("proxy")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    if !has_proxy {
        if let Some(url) = EgressPicker::build(&st, body.egress.as_deref()).next() {
            extra_map.insert("proxy".into(), serde_json::json!(url));
        }
    }
    // provider 在 extra_json 序列化前确定,以便 dario 路径能并入凭据。
    let provider = body.provider.as_deref().filter(|p| !p.is_empty()).unwrap_or("kiro");
    // claude-dario:若带了 .credentials.json 全文,解析后并入 extra(不覆盖操作者已填的同名字段),
    // 并补生成稳定身份 device_id / account_uuid(凭证文件里没有)。
    if provider == "claude-dario" {
        if let Some(cred) = body.credentials_json.as_deref().filter(|s| !s.trim().is_empty()) {
            match gw_dario::parse_cc_credentials(cred) {
                Ok(parsed) => {
                    for (k, v) in parsed {
                        extra_map.entry(k).or_insert(v);
                    }
                }
                Err(e) => {
                    return api_error(
                        StatusCode::BAD_REQUEST,
                        &format!("解析 .credentials.json 失败: {e}"),
                    )
                }
            }
        }
        extra_map
            .entry("device_id".to_string())
            .or_insert_with(|| serde_json::json!(uuid::Uuid::new_v4().to_string()));
        extra_map
            .entry("account_uuid".to_string())
            .or_insert_with(|| serde_json::json!(uuid::Uuid::new_v4().to_string()));
    }
    // 调度优先级:显式传了才写 extra.priority(不传 → scheduler 缺省 100)。
    if let Some(pri) = body.priority {
        extra_map.insert("priority".into(), serde_json::json!(pri));
    }
    let extra_json = match serde_json::to_string(&extra_map) {
        Ok(s) => s,
        Err(e) => return internal_error(e),
    };
    let group = body.group.as_deref().unwrap_or("");
    // 非空组名必须真实存在,防"幽灵分组"(typo 的账号永远不被任何 worker 服务,
    // groups 页也看不见;审查 Minimalist#2)。
    if let Err(resp) = require_real_group(&st, group) {
        return resp;
    }
    let conc = body.max_concurrency.unwrap_or(2); // 缺省对齐 kiro.rs maxConcurrency=2
    match st
        .store
        .create_account(&body.account_id, group, provider, conc, &extra_json)
    {
        Ok(true) => match st.store.get_account(&body.account_id) {
            Ok(Some(row)) => (StatusCode::CREATED, Json(redacted_view(row))).into_response(),
            Ok(None) => internal_error("创建后读取不到账号"),
            Err(e) => internal_error(e),
        },
        Ok(false) => api_error(StatusCode::CONFLICT, "account_id 已存在"),
        Err(e) => internal_error(e),
    }
}

// ── claude-dario OAuth 上号(铸 token 走 per-account 出口,与 refresh/chat 同 IP)─────────
//
// 两步:start 生成 PKCE+authorize URL(纯本地,不发网络),操作员浏览器人肉登录同意拿 code →
// complete 把 code 扇给**目标组的 worker** 换码(走该组 egress=该号将来 refresh/chat 同出口),
// 成功后落库 + /sync。consent 浏览器那跳 IP 不纳入保证(code 数秒失效),铸/刷/发三步同 IP。

/// 待完成的上号会话(进程内存,**绝不落库、绝不进日志**;TTL 到期即弃)。
struct PendingOAuth {
    /// PKCE code_verifier(敏感:单次、短时、仅本会话可用)。
    verifier: String,
    /// 解析后的出口代理(None=组默认出口);换码与落库用同一值 → 铸=刷=发同 IP。
    proxy: Option<String>,
    account_id: String,
    group: String,
    max_concurrency: i64,
    created_at: std::time::Instant,
}

const OAUTH_PENDING_TTL_SECS: u64 = 600; // 10 分钟:够人肉登录,过期即弃(防内存累积 + 限攻击窗口)
const OAUTH_PENDING_MAX: usize = 256; // 硬上限:即便 TTL 内被狂发 start 也不无界增长(admin 已鉴权,够宽松)

// 进程内待完成会话。⚠️ 单 admin 进程局部:`/start` 与 `/complete` 必须命中同一 router 进程
// (admin UI 单一来源天然满足);router 重启会丢失 10 分钟窗口内的待完成会话(操作员重发即可)。
fn oauth_pending() -> &'static std::sync::Mutex<std::collections::HashMap<String, PendingOAuth>> {
    static S: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, PendingOAuth>>> =
        std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 清过期项(无后台清扫线程,读写时回收以绑定内存)。
fn sweep_expired(m: &mut std::collections::HashMap<String, PendingOAuth>) {
    m.retain(|_, v| v.created_at.elapsed().as_secs() < OAUTH_PENDING_TTL_SECS);
}

fn insert_pending(state: String, p: PendingOAuth) {
    let mut m = oauth_pending().lock().unwrap_or_else(|e| e.into_inner());
    sweep_expired(&mut m);
    // 清完仍达上限(极端突发)→ 丢最旧待完成会话,硬绑内存。
    while m.len() >= OAUTH_PENDING_MAX {
        let Some(oldest) = m.iter().min_by_key(|(_, v)| v.created_at).map(|(k, _)| k.clone()) else {
            break;
        };
        m.remove(&oldest);
    }
    m.insert(state, p);
}

/// 取出并**移除**(单次);取用时也清过期(不滞留到下次 insert)。
fn take_pending(state: &str) -> Option<PendingOAuth> {
    let mut m = oauth_pending().lock().unwrap_or_else(|e| e.into_inner());
    sweep_expired(&mut m);
    m.remove(state)
}

#[derive(Debug, Deserialize)]
pub struct StartOAuthBody {
    account_id: String,
    #[serde(default)]
    group: Option<String>,
    /// 出口网关选择:""/缺省/"direct"=直连;"auto"=自动均衡;数字=egress_pool 索引。
    /// (上号统一走 egress 选择,不单独收 raw proxy——审查 Minimalist#2:与 egress 重复。)
    #[serde(default)]
    egress: Option<String>,
    #[serde(default)]
    max_concurrency: Option<i64>,
}

/// `POST /accounts/oauth/start` —— 生成 PKCE + authorize URL,登记待完成会话。
/// 仅探一次目标组 worker 的 /health(loopback 只读)做 dario 预检,不发任何外网请求。
async fn oauth_start(
    State(st): State<AdminState>,
    Json(body): Json<StartOAuthBody>,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&body.account_id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    // 提前挡掉已存在的 account_id,别让操作员登录半天才在 complete 撞 409。
    match st.store.get_account(&body.account_id) {
        Ok(Some(_)) => return api_error(StatusCode::CONFLICT, "account_id 已存在"),
        Ok(None) => {}
        Err(e) => return internal_error(e),
    }
    let group = body.group.as_deref().unwrap_or("").to_string();
    if let Err(resp) = require_real_group(&st, &group) {
        return resp;
    }
    // 目标组必须有 worker,且其 provider 必须是 claude-dario。否则 complete 会在操作员**登录之后**
    // 才失败(白费一次 consent)。这里探一次该组 worker 的 /health(loopback 只读)提前挡掉。
    let group_workers: Vec<&gw_core::config::WorkerConfig> =
        st.workers.iter().filter(|w| w.account_group == group).collect();
    if group_workers.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "该组没有 worker(无法保证铸 token 出口 IP);请先为该组配置 worker",
        );
    }
    let mut is_dario = false;
    for w in &group_workers {
        let url = format!("http://{}/health", w.listen);
        if let Ok(resp) = st.http.get(&url).send().await {
            if let Ok(j) = resp.json::<serde_json::Value>().await {
                if j.get("provider").and_then(|v| v.as_str()) == Some("claude-dario") {
                    is_dario = true;
                    break;
                }
            }
        }
    }
    if !is_dario {
        return api_error(
            StatusCode::BAD_REQUEST,
            "该组不是 claude-dario 组(OAuth 上号仅支持 dario);请选 dario 组,或确认其 worker 在线",
        );
    }
    // 出口解析:统一按 egress 选择(显式网关/自动均衡/直连)。direct=本机出口 IP——caio 各 worker
    // 均 network_mode:host 同机,故 direct 对该组所有 worker 是同一 IP;非 direct(proxy)则各 worker
    // 都用同一 proxy 串 → 同 IP。换码与落库用同一 proxy 值,铸=刷=发同 IP(见 client_for_proxy)。
    let proxy: Option<String> = EgressPicker::build(&st, body.egress.as_deref()).next();
    let (verifier, challenge) = gw_dario::oauth::gen_pkce();
    let state = gw_dario::oauth::gen_state();
    let authorize_url = gw_dario::oauth::build_authorize_url(&challenge, &state);
    insert_pending(
        state.clone(),
        PendingOAuth {
            verifier,
            proxy,
            account_id: body.account_id,
            group,
            max_concurrency: body.max_concurrency.unwrap_or(2),
            created_at: std::time::Instant::now(),
        },
    );
    Json(serde_json::json!({
        "authorize_url": authorize_url,
        "state": state,
        "expires_in_sec": OAUTH_PENDING_TTL_SECS,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct CompleteOAuthBody {
    /// start 返回的 state(会话键 + CSRF 绑定)。
    state: String,
    /// 回调页显示的串(可能是 `code` 或 `code#state`)。
    code: String,
}

/// `POST /accounts/oauth/complete` —— 用 code 换 token(扇给目标组 worker,走该组 egress),落库 + /sync。
async fn oauth_complete(
    State(st): State<AdminState>,
    Json(body): Json<CompleteOAuthBody>,
) -> axum::response::Response {
    let (code, pasted_state) = gw_dario::oauth::parse_manual_code(&body.code);
    if code.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "code 为空");
    }
    // 回调串若自带 #state,必须与会话 state 一致(CSRF/串号防护)。
    if let Some(ps) = &pasted_state {
        if ps != &body.state {
            return api_error(StatusCode::BAD_REQUEST, "state 不匹配(请勿混用不同上号会话的 code)");
        }
    }
    let Some(pending) = take_pending(&body.state) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "上号会话已过期或无效(超过 10 分钟或已用过),请重新发起",
        );
    };
    // 换码前再查一次重名:缩小「成功铸出 refresh_token 却因重名被丢弃」的窗口(审查 Skeptic#4)。
    // code 此刻**尚未**递交上游、仍有效:重名/查询失败都属可恢复,保留会话供重试(改 account_id 后再 complete),
    // 绝不在这里 destructive 销毁(否则操作员被迫整轮重新 consent;审查 Skeptic A1)。
    match st.store.get_account(&pending.account_id) {
        Ok(Some(_)) => {
            insert_pending(body.state.clone(), pending);
            return api_error(StatusCode::CONFLICT, "account_id 已存在(上号会话已保留,请改用其他 ID 重试)");
        }
        Ok(None) => {}
        Err(e) => {
            insert_pending(body.state.clone(), pending);
            return internal_error(e);
        }
    }
    // 扇给**目标组**的 worker(account_group 匹配)——该 worker 的 egress = 该号将来 refresh/chat
    // 同一出口 IP。绝不扇给其它组(出口会错)。
    let group_workers: Vec<&gw_core::config::WorkerConfig> =
        st.workers.iter().filter(|w| w.account_group == pending.group).collect();
    if group_workers.is_empty() {
        // 组在 start 之后丢了 worker(罕见)→ code 未递交上游,保留会话供重试(审查 Skeptic#3)。
        insert_pending(body.state.clone(), pending);
        return api_error(
            StatusCode::BAD_GATEWAY,
            "目标组暂无 worker,上号会话已保留,请稍后重试",
        );
    }
    // 换码可能经代理 + 上游往返,2s(st.http)太短;用 30s 专用 client。
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(e) => return internal_error(e),
    };
    let payload = serde_json::json!({
        "proxy": pending.proxy,
        "code": code,
        "verifier": pending.verifier,
    });
    let mut tokens: Option<serde_json::Value> = None;
    let mut first_error: Option<(u16, String)> = None;
    let mut any_responded = false; // 任一 worker 给了 HTTP 应答 = code 已递交上游(无论成败)
    for w in &group_workers {
        let url = format!("http://{}/oauth/exchange", w.listen);
        let resp = match client.post(&url).json(&payload).send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(listen = %w.listen, "oauth/exchange 扇出失败(worker 离线?): {e}");
                continue;
            }
        };
        any_responded = true;
        let status = resp.status().as_u16();
        let rbody = resp.json::<serde_json::Value>().await.ok();
        if (200..300).contains(&status) {
            tokens = rbody;
            break;
        }
        if first_error.is_none() {
            let msg = rbody
                .as_ref()
                .and_then(|b| b.pointer("/error/message"))
                .and_then(|m| m.as_str())
                .unwrap_or("换码失败")
                .to_string();
            first_error = Some((status, msg));
        }
    }
    if tokens.is_none() && !any_responded {
        // 没有任何 worker 应答(纯连接失败)→ code 未递交上游、仍有效 → 保留会话供重试(审查 Skeptic#3)。
        insert_pending(body.state.clone(), pending);
        return api_error(
            StatusCode::BAD_GATEWAY,
            "目标组 worker 当前不可达,上号会话已保留,请稍后重试",
        );
    }
    let Some(tokens) = tokens else {
        // worker 应答但换码失败(code 已被上游消费,重试无益)→ 不保留会话,透出错误。
        if let Some((status, msg)) = first_error {
            let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            return api_error(code, &msg);
        }
        return api_error(StatusCode::BAD_GATEWAY, "换码失败");
    };
    // 组装 extra:换码所得 token 字段 + 出口 proxy(与换码同值)+ 稳定身份(凭证里没有,生成)。
    let mut extra_map = match tokens {
        serde_json::Value::Object(m) => m,
        _ => return internal_error("换码返回非对象"),
    };
    // 边界再校验:换码必须带来非空 refresh_token,否则号无法长期持有(审查 Architect#5 防御纵深;
    // worker 侧 parse_token_set 已校验,此处再挡一道防 provider 改动后失守)。
    if extra_map
        .get("refresh_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .is_none()
    {
        return internal_error("换码结果缺 refresh_token,拒绝落库");
    }
    if let Some(p) = &pending.proxy {
        extra_map.insert("proxy".into(), serde_json::json!(p));
    }
    extra_map
        .entry("device_id".to_string())
        .or_insert_with(|| serde_json::json!(uuid::Uuid::new_v4().to_string()));
    extra_map
        .entry("account_uuid".to_string())
        .or_insert_with(|| serde_json::json!(uuid::Uuid::new_v4().to_string()));
    let extra_json = match serde_json::to_string(&extra_map) {
        Ok(s) => s,
        Err(e) => return internal_error(e),
    };
    match st.store.create_account(
        &pending.account_id,
        &pending.group,
        "claude-dario",
        pending.max_concurrency,
        &extra_json,
    ) {
        Ok(true) => {
            // 主动 /sync 目标组 worker,消除"上号后 30s 内按号操作报无人持有"窗口(best-effort)。
            for w in &group_workers {
                let url = format!("http://{}/sync", w.listen);
                let _ = st.http.post(&url).send().await;
            }
            match st.store.get_account(&pending.account_id) {
                Ok(Some(row)) => (StatusCode::CREATED, Json(redacted_view(row))).into_response(),
                Ok(None) => internal_error("上号后读取不到账号"),
                Err(e) => internal_error(e),
            }
        }
        Ok(false) => api_error(StatusCode::CONFLICT, "account_id 已存在"),
        Err(e) => internal_error(e),
    }
}

#[derive(Debug, Deserialize)]
pub struct ImportBody {
    /// 目标组(账号挂到哪个 worker 组);非空时必须真实存在。
    #[serde(default)]
    group_name: Option<String>,
    /// KiroManager 导出原文(字符串)。前端就是粘贴文本,单一形态,服务端解析一次。
    json: String,
    /// 批量出口代理(可选):给本批所有导入账号写 extra.proxy。空/缺省=不设。
    #[serde(default)]
    batch_proxy: Option<String>,
    /// 上号选择的出口网关:""/缺省/"direct"=直连;"auto"=自动均衡;数字=egress_pool 索引。
    /// 显式 batch_proxy 优先于此。
    #[serde(default)]
    egress: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ImportApiKeysBody {
    /// 目标组;非空时必须真实存在。
    #[serde(default)]
    group_name: Option<String>,
    /// 粘贴的官方 API Key 文本(每行一个 `ksk_...`,支持空格/逗号分隔)。
    keys: String,
    /// 出口网关选择:""/缺省/"direct"=直连;"auto"=自动均衡;数字=egress_pool 索引。
    /// **不收 raw batch_proxy**:上号统一走 egress 选择(与主导入口径一致,避免绕过
    /// egress_pool 的后门——审查 r2 Architect#3/Minimalist#2)。
    #[serde(default)]
    egress: Option<String>,
}

/// `POST /accounts/import-apikeys` —— 批量粘贴官方 Kiro API Key(`ksk_`)导入。
///
/// 切出 key 列表 → 转成字符串数组 → **复用** [`import_accounts`] 的建号/去重/合并/捅同步
/// 全流程(bare `ksk_` 字符串由 `parse_accounts_export` 映射为 apikey 账号)。订阅档位与
/// 用量由 worker 后台预热(首次同步后回填),这里不做同步探测。响应结构同 `/accounts/import`。
async fn import_apikeys(
    State(st): State<AdminState>,
    Json(body): Json<ImportApiKeysBody>,
) -> axum::response::Response {
    let keys = gw_kiro::import::split_api_keys(&body.keys);
    if keys.is_empty() {
        return api_error(
            StatusCode::BAD_REQUEST,
            "未识别到任何 API Key(每行一个 ksk_...)",
        );
    }
    let json = match serde_json::to_string(&keys) {
        Ok(s) => s,
        Err(e) => return internal_error(e),
    };
    import_accounts(
        State(st),
        Json(ImportBody {
            group_name: body.group_name,
            json,
            batch_proxy: None, // apikey 导入不走 raw proxy,统一 egress
            egress: body.egress,
        }),
    )
    .await
}

/// 账号的稳定身份(user_id 优先,否则 email),用于导入碰撞核对。
fn identity_of(extra: &serde_json::Map<String, serde_json::Value>) -> Option<&str> {
    extra
        .get("user_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            extra
                .get("email")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        })
}

/// `POST /accounts/import` —— 完整导入 KiroManager 导出 JSON(智能合并)。
///
/// 智能合并(已与用户确认):新账号全字段写入;已存在账号**只补缺失字段**(machineId/
/// clientId/secret/profileArn 等身份字段),**不覆盖**服务器已 roll 的 refresh_token/
/// access_token,已有 machineId 也不覆盖(若与导入值不同则在结果里标 conflict 提示)。
///
/// 安全:响应**只回** account_id + 动作,绝不回显任何 token/secret。
async fn import_accounts(
    State(st): State<AdminState>,
    Json(body): Json<ImportBody>,
) -> axum::response::Response {
    // 目标组校验(防幽灵分组:typo 的账号永不被任何 worker 服务)。
    let group = body.group_name.as_deref().unwrap_or("");
    if let Err(resp) = require_real_group(&st, group) {
        return resp;
    }

    // 宽松解析:容忍从富文本/网页复制粘贴带入的非标准空白(nbsp 等),否则 serde
    // 严格解析会直接报错(用户粘贴上号工具原文常踩此坑)。
    let root: serde_json::Value = match gw_kiro::import::parse_import_json(&body.json) {
        Ok(v) => v,
        Err(msg) => return api_error(StatusCode::BAD_REQUEST, &msg),
    };

    let imported = match gw_kiro::import::parse_accounts_export(&root) {
        Ok(v) => v,
        Err(msg) => return api_error(StatusCode::BAD_REQUEST, &msg),
    };

    // 批量代理写入边界校验一次(非空则必须合法,fail-closed),归一为 trim 后的值。
    let batch_proxy: Option<String> = match body.batch_proxy.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => match validate_proxy_url(s) {
            Ok(valid) => Some(valid),
            Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
        },
        _ => None,
    };

    // 上号选择的出口网关(直连/自动均衡/选定网关);显式 batch_proxy 仍优先,
    // 已存在号合并不动 proxy。
    let mut egress_picker = EgressPicker::build(&st, body.egress.as_deref());

    let mut created = 0u32;
    let mut merged = 0u32;
    let mut skipped = 0u32;
    let mut items = Vec::new();

    for imp in imported {
        // account_id 已由 import 清洗;再校验一次防御。
        if validate_account_id(&imp.account_id).is_err() {
            skipped += 1;
            items.push(serde_json::json!({
                "account_id": imp.account_id, "action": "skipped", "reason": "非法 account_id"
            }));
            continue;
        }
        let has_mid = imp.has_machine_id();
        let existing = match st.store.get_account(&imp.account_id) {
            Ok(v) => v,
            Err(e) => return internal_error(e),
        };
        let Some(row) = existing else {
            // 新账号:全字段写入(含 token)。create_account 是 INSERT OR IGNORE,
            // 返回 false = 并发下别人刚插了同 id(竞态)→ 当 skipped,不谎报 created。
            // 批量代理:操作员显式意图,写进新账号 extra.proxy(已校验归一)。
            let mut new_extra = imp.extra.clone();
            if let Some(bp) = &batch_proxy {
                new_extra.insert("proxy".into(), serde_json::json!(bp));
            } else if let Some(url) = egress_picker.next() {
                // 选定网关 / 自动均衡:写 extra.proxy(自动模式每号挑最少使用,粘性铺满)。
                new_extra.insert("proxy".into(), serde_json::json!(url));
            }
            let extra_json = match serde_json::to_string(&new_extra) {
                Ok(s) => s,
                Err(e) => return internal_error(e),
            };
            match st.store.create_account(&imp.account_id, group, "kiro", 2, &extra_json) {
                Ok(true) => {
                    created += 1;
                    items.push(serde_json::json!({
                        "account_id": imp.account_id, "action": "created", "has_machine_id": has_mid
                    }));
                }
                Ok(false) => {
                    skipped += 1;
                    items.push(serde_json::json!({
                        "account_id": imp.account_id, "action": "skipped", "reason": "并发已存在"
                    }));
                }
                Err(e) => return internal_error(e),
            }
            continue;
        };

        // 已存在账号:智能合并。
        let existing_extra: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&row.extra).unwrap_or_default();

        // 碰撞防护(审查 H1):邮箱清洗派生的 account_id 可能让两个不同真号撞同一 ID。
        // 用稳定身份(user_id 优先,否则 email)核对:双方都有且不同 → 是不同账号撞 ID,
        // 跳过(绝不把两个真号合并成一个)。
        if let (Some(a), Some(b)) = (identity_of(&imp.extra), identity_of(&existing_extra)) {
            if a != b {
                skipped += 1;
                items.push(serde_json::json!({
                    "account_id": imp.account_id, "action": "skipped",
                    "reason": "account_id 派生碰撞(不同账号映射到同一 ID),未合并"
                }));
                continue;
            }
        }

        // delta 只放"现账号缺失/空"的 key → 已有字段天然不覆盖。**token 字段(refresh_token/
        // access_token/expires_at)在合并时一律跳过**:服务器拥有并轮换它们,导出里的是旧值,
        // 既消除"用旧 token 覆盖刚 roll 的新 token"竞态(审查 H3),也兑现"不回退 server token"。
        const MERGE_SKIP_TOKEN_FIELDS: [&str; 3] = ["refresh_token", "access_token", "expires_at"];
        let mut delta = serde_json::Map::new();
        let mut machine_id_conflict = false;
        for (k, v) in &imp.extra {
            if MERGE_SKIP_TOKEN_FIELDS.contains(&k.as_str()) {
                continue; // token 仅创建时写,合并不碰。
            }
            let present_nonempty = matches!(
                existing_extra.get(k),
                Some(ev) if ev.as_str() != Some("")
            );
            if present_nonempty {
                if k == "machine_id" && existing_extra.get(k) != Some(v) {
                    machine_id_conflict = true; // 已有不同设备指纹,不覆盖,仅提示。
                }
            } else {
                delta.insert(k.clone(), v.clone());
            }
        }
        // 批量代理:操作员显式意图,合并时直接设(可覆盖旧 proxy;让"仅设代理"也算有效合并)。
        if let Some(bp) = &batch_proxy {
            delta.insert("proxy".into(), serde_json::json!(bp));
        }
        if delta.is_empty() {
            skipped += 1;
            items.push(serde_json::json!({
                "account_id": imp.account_id, "action": "skipped",
                "machine_id_conflict": machine_id_conflict
            }));
            continue;
        }
        let delta_json = match serde_json::to_string(&delta) {
            Ok(s) => s,
            Err(e) => return internal_error(e),
        };
        match st.store.merge_account_extra(&imp.account_id, &delta_json) {
            Ok(_) => {
                merged += 1;
                items.push(serde_json::json!({
                    "account_id": imp.account_id, "action": "merged",
                    "machine_id_conflict": machine_id_conflict
                }));
            }
            Err(e) => return internal_error(e),
        }
    }

    // 导入落库后 best-effort 捅所有 worker 立即同步账号集——消除"导入后 30s 内
    // 按号操作(验活/刷新)报『没有 worker 持有该账号』"的窗口。
    if created + merged > 0 {
        poke_workers_sync(&st).await;
    }

    Json(serde_json::json!({
        "created": created, "merged": merged, "skipped": skipped, "items": items
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct RebalanceEgressBody {
    /// true(默认):只给当前**无**出口代理的账号分配池 URL(不打扰已固定专属代理/已在池内的号);
    /// false:全量重铺所有账号到池(忽略当前 proxy)。
    #[serde(default = "default_only_unassigned")]
    only_unassigned: bool,
}
fn default_only_unassigned() -> bool {
    true
}

/// `POST /accounts/rebalance-egress` —— 把账号按「最少使用」均衡铺到出口池(`egress_pool`)。
///
/// 配好 egress_pool 后用它把现有账号回填到美国多 IP。只改 `extra.proxy`
/// (merge_account_extra,绝不碰凭据字段);改完捅 worker 立即同步。
async fn rebalance_egress(
    State(st): State<AdminState>,
    Json(body): Json<RebalanceEgressBody>,
) -> axum::response::Response {
    let pool = read_egress_pool(&st);
    if pool.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "未配置 egress_pool(出口池),无法均衡");
    }
    let rows = match st.store.list_accounts() {
        Ok(r) => r,
        Err(e) => return internal_error(e),
    };
    // 计数基线:已固定在池内的账号都计入(保持均衡),无论是否重铺它们。
    let mut counts: Vec<(String, usize)> = pool.iter().map(|u| (u.clone(), 0usize)).collect();
    let mut to_assign: Vec<String> = Vec::new();
    for row in &rows {
        let cur = account_proxy(&row.extra);
        let in_pool = cur.as_deref().map(|p| pool.iter().any(|u| u == p)).unwrap_or(false);
        if body.only_unassigned {
            if in_pool {
                // 已在池内:计数,不动。
                if let Some(p) = &cur {
                    if let Some(c) = counts.iter_mut().find(|(u, _)| u == p) {
                        c.1 += 1;
                    }
                }
            } else if cur.is_none() {
                // 无代理:待分配。
                to_assign.push(row.account_id.clone());
            }
            // 有非池专属代理:only_unassigned 下尊重不动。
        } else {
            // 全量重铺:所有账号都重新分配(忽略当前 proxy)。
            to_assign.push(row.account_id.clone());
        }
    }
    let mut assigned = 0u32;
    for aid in &to_assign {
        let Some(idx) = counts
            .iter()
            .enumerate()
            .min_by_key(|(_, (_, c))| *c)
            .map(|(i, _)| i)
        else {
            break;
        };
        counts[idx].1 += 1;
        let url = counts[idx].0.clone();
        let delta = serde_json::json!({ "proxy": url }).to_string();
        match st.store.merge_account_extra(aid, &delta) {
            Ok(_) => assigned += 1,
            Err(e) => return internal_error(e),
        }
    }
    if assigned > 0 {
        poke_workers_sync(&st).await;
    }
    // distribution 用 redact_proxy_url 掩码,避免经接口泄漏代理口令。
    let distribution: Vec<serde_json::Value> = counts
        .iter()
        .map(|(u, c)| serde_json::json!({ "proxy": redact_proxy_url(u), "count": c }))
        .collect();
    Json(serde_json::json!({
        "assigned": assigned,
        "pool_size": pool.len(),
        "distribution": distribution,
    }))
    .into_response()
}

#[cfg(test)]
mod egress_tests {
    use super::EgressAssigner;

    #[test]
    fn assigner_picks_unique_least() {
        let mut a = EgressAssigner {
            counts: vec![("A".into(), 5), ("B".into(), 1), ("C".into(), 3)],
        };
        // B 计数最少 → 先补 B。
        assert_eq!(a.next().as_deref(), Some("B"));
    }

    #[test]
    fn assigner_balances_toward_equal() {
        let mut a = EgressAssigner {
            counts: vec![("A".into(), 2), ("B".into(), 0), ("C".into(), 0)],
        };
        // 第一个补到最少的(B/C 之一)。
        let first = a.next().unwrap();
        assert!(first == "B" || first == "C", "应先补最少的, got {first}");
        // 再连续分配若干次,三者计数差 ≤ 1(趋于均衡铺满)。
        for _ in 0..4 {
            a.next();
        }
        let counts: Vec<usize> = a.counts.iter().map(|(_, c)| *c).collect();
        let max = *counts.iter().max().unwrap();
        let min = *counts.iter().min().unwrap();
        assert!(max - min <= 1, "应趋于均衡, got {counts:?}");
    }
}

async fn update_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateAccountBody>,
) -> axum::response::Response {
    if let Some(g) = body.group_name.as_deref().filter(|g| !g.is_empty()) {
        if let Err(resp) = require_real_group(&st, g) {
            return resp;
        }
    }
    let extra = match &body.extra {
        // `***` 开头的字符串值是脱敏哨兵 = "保留 DB 原值":GET 返回的就是脱敏形态,
        // 前端整块回传时不需要(也不可能)还原真实凭据;没有这一层,带多个敏感字段
        // 的账号在轮换单个 token 时会丢掉其余凭据(审查 Minimalist#6)。
        Some(map) => {
            let current = match st.store.get_account(&id) {
                Ok(Some(row)) => row.extra,
                Ok(None) => return api_error(StatusCode::NOT_FOUND, "账号不存在"),
                Err(e) => return internal_error(e),
            };
            let current: serde_json::Map<String, serde_json::Value> =
                serde_json::from_str(&current).unwrap_or_default();
            let mut resolved = serde_json::Map::new();
            for (k, v) in map {
                match v.as_str() {
                    Some(s) if s.starts_with("***") => {
                        if let Some(orig) = current.get(k) {
                            resolved.insert(k.clone(), orig.clone());
                        }
                        // DB 已无该字段:脱敏占位无可保留,丢弃。
                    }
                    _ => {
                        resolved.insert(k.clone(), v.clone());
                    }
                }
            }
            match serde_json::to_string(&resolved) {
                Ok(s) => Some(s),
                Err(e) => return internal_error(e),
            }
        }
        None => None,
    };
    // 主体更新:仅当存在可改字段时才打 update_account(避免 all-None 空 patch 语义歧义)。
    let has_patch = body.group_name.is_some()
        || body.max_concurrency.is_some()
        || body.disabled.is_some()
        || extra.is_some();
    if has_patch {
        let patch = AccountPatch {
            group_name: body.group_name.clone(),
            max_concurrency: body.max_concurrency,
            disabled: body.disabled,
            extra,
        };
        match st.store.update_account(&id, &patch) {
            Ok(true) => {}
            Ok(false) => return api_error(StatusCode::NOT_FOUND, "账号不存在"),
            Err(e) => return internal_error(e),
        }
    }
    // 定点代理合并:**在整块替换之后**,绝不被 extra 整块替换覆盖(规避 PATCH extra 坑)。
    // 空/纯空白 = 清除该号专属代理(写 proxy:null → resolver 回落默认/源 IP);
    // 非空 = 写入边界校验(非法直接 400,fail-closed 不静默回退裸 IP,审查 Skeptic#2)。
    if let Some(proxy) = &body.proxy_url {
        let trimmed = proxy.trim();
        let proxy_val = if trimmed.is_empty() {
            serde_json::Value::Null
        } else {
            match validate_proxy_url(trimmed) {
                Ok(valid) => serde_json::Value::String(valid),
                Err(msg) => return api_error(StatusCode::BAD_REQUEST, msg),
            }
        };
        let delta = serde_json::json!({ "proxy": proxy_val }).to_string();
        match st.store.merge_account_extra(&id, &delta) {
            Ok(true) => {}
            Ok(false) => return api_error(StatusCode::NOT_FOUND, "账号不存在"),
            Err(e) => return internal_error(e),
        }
    }
    // 定点优先级合并(同 proxy_url:在整块替换之后走增量 merge,绝不碰凭据)。
    // 数值越小越优先,缺省 100(见 worker/scheduler.rs 分层 LRU);缺省=不动 priority。
    if let Some(pri) = body.priority {
        let delta = serde_json::json!({ "priority": pri }).to_string();
        match st.store.merge_account_extra(&id, &delta) {
            Ok(true) => {}
            Ok(false) => return api_error(StatusCode::NOT_FOUND, "账号不存在"),
            Err(e) => return internal_error(e),
        }
    }
    // 落库后 best-effort 捅所有 worker 立即同步(同 delete_account/import 的理由):
    // 否则启用/禁用/换组等改动要等 worker 自己最多 30s 的周期 sync 才生效,期间按号操作
    // (如导入对话框"编辑后立即验活")会误报"没有 worker 持有该账号"。
    if has_patch || body.proxy_url.is_some() || body.priority.is_some() {
        poke_workers_sync(&st).await;
    }
    match st.store.get_account(&id) {
        Ok(Some(row)) => Json(redacted_view(row)).into_response(),
        Ok(None) => api_error(StatusCode::NOT_FOUND, "账号不存在"),
        Err(e) => internal_error(e),
    }
}

async fn delete_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    match st.store.delete_account(&id) {
        Ok(true) => {
            // 立即让 worker 从内存移除(审查 Skeptic#4/Minimalist#5):否则删除后
            // 至多 30s 内该账号仍可能被 chat 选中,与"已删除"语义冲突。
            poke_workers_sync(&st).await;
            StatusCode::NO_CONTENT.into_response()
        }
        Ok(false) => api_error(StatusCode::NOT_FOUND, "账号不存在"),
        Err(e) => internal_error(e),
    }
}

/// best-effort 捅所有 worker 立即从 DB 同步账号集(导入/删除后消除 30s 生效窗口)。
/// 失败仅 debug log:worker 自己的 30s 周期 sync 兜底。
async fn poke_workers_sync(st: &AdminState) {
    let fanout = st.workers.iter().map(|w| {
        let http = st.http.clone();
        let url = format!("http://{}/sync", w.listen);
        let instance = w.instance;
        async move {
            if let Err(e) = http.post(&url).send().await {
                tracing::debug!(instance, "sync 扇出失败(worker 离线?): {e}");
            }
        }
    });
    futures::future::join_all(fanout).await;
}

/// 运行态聚合:并发拉各 worker 的 `/health`(单个 2s 超时),离线 worker 标 `online:false`。
/// 并发使最坏耗时 ≈ 单个超时,而非随离线 worker 数串行累加;join_all 保持 worker 顺序。
async fn runtime(State(st): State<AdminState>) -> axum::response::Response {
    let fetches = st.workers.iter().map(|w| {
        let http = st.http.clone();
        async move {
            let url = format!("http://{}/health", w.listen);
            match http.get(&url).send().await {
                Ok(resp) => match resp.json::<serde_json::Value>().await {
                    Ok(v) => serde_json::json!({
                        "instance": w.instance,
                        "group": w.account_group,
                        "online": true,
                        "accounts_status": v.get("accounts_status").cloned()
                            .unwrap_or(serde_json::Value::Array(vec![])),
                    }),
                    Err(e) => {
                        tracing::warn!(instance = w.instance, "worker /health 响应解析失败: {e}");
                        serde_json::json!({
                            "instance": w.instance, "group": w.account_group, "online": false,
                        })
                    }
                },
                Err(e) => {
                    tracing::debug!(instance = w.instance, "worker 不在线: {e}");
                    serde_json::json!({
                        "instance": w.instance, "group": w.account_group, "online": false,
                    })
                }
            }
        }
    });
    let out = futures::future::join_all(fetches).await;
    Json(out).into_response()
}

/// `POST /accounts/{id}/reset` —— 人工救号:清该账号在 worker 内存里的运行时
/// 禁用/冷却/失败计数(quota_exhausted、invalid_refresh_token、too_many_failures
/// 等均立即解除;配置层 disabled 不动)。
///
/// 运行态在 worker 进程内存,且 DB 不记录账号↔worker 的实际归属(组配置可能刚改),
/// 故对**所有** worker 扇出(并发,单个 2s 超时);各 worker 对不在本组的账号回 404,
/// 幂等无副作用。任一 worker 命中即视为成功。
async fn reset_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    let fanout = st.workers.iter().map(|w| {
        let http = st.http.clone();
        let id = id.clone();
        async move {
            let url = format!("http://{}/accounts/{}/reset", w.listen, id);
            match http.post(&url).send().await {
                Ok(resp) if resp.status().is_success() => Some(w.instance),
                Ok(_) => None,  // 404 = 账号不在该 worker 组
                Err(e) => {
                    tracing::debug!(instance = w.instance, "reset 扇出失败(worker 离线?): {e}");
                    None
                }
            }
        }
    });
    let hits: Vec<u32> = futures::future::join_all(fanout)
        .await
        .into_iter()
        .flatten()
        .collect();
    if hits.is_empty() {
        return api_error(
            StatusCode::NOT_FOUND,
            "没有 worker 持有该账号(账号不存在、worker 离线或组未被任何 worker 绑定)",
        );
    }
    Json(serde_json::json!({"reset": true, "account_id": id, "workers": hits})).into_response()
}

/// `POST /accounts/{id}/refresh` —— 人工**强制刷新该账号 token**(rt→at 换一次,仅刷新,
/// **不**发 chat → 不触发风控,见 no-chat-test-on-real-accounts 记忆)。
///
/// 与 reset 不同,refresh 有副作用(滚动 refresh_token),故**顺序**询问各 worker。
/// 各 worker 三种回应:
/// - 2xx:持有方刷新成功 → **立即**返回(成功才滚 token,故绝不会刷到第二个 worker);
/// - 404:不持有此账号 → 问下一个 worker;
/// - 其他(502/400):持有方尝试了但上游刷新失败(失败不滚 token,安全)→ 记下首个错误,
///   继续问后面的 worker,看重复持有窗口里是否有别的 worker 能成功(审查 Skeptic#3/Architect#4)。
///
/// 扫完仍无成功:有错误则透出首个错误(而非误报“无人持有”),否则全 404 → 404。
async fn refresh_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    let mut first_error: Option<(u16, String)> = None;
    for w in st.workers.iter() {
        let url = format!("http://{}/accounts/{}/refresh", w.listen, id);
        let resp = match st.http.post(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!(instance = w.instance, "refresh 扇出失败(worker 离线?): {e}");
                continue;
            }
        };
        let status = resp.status().as_u16();
        if status == 404 {
            continue; // 该 worker 不持有此账号。
        }
        let body = resp.json::<serde_json::Value>().await.ok();
        if (200..300).contains(&status) {
            return Json(
                body.unwrap_or_else(|| serde_json::json!({"refreshed": true, "account_id": id})),
            )
            .into_response();
        }
        // 持有方尝试了但失败(未滚 token):记下首个错误,继续看后面是否有 worker 能成功。
        if first_error.is_none() {
            let msg = body
                .as_ref()
                .and_then(|b| b.pointer("/error/message"))
                .and_then(|m| m.as_str())
                .unwrap_or("刷新失败")
                .to_string();
            first_error = Some((status, msg));
        }
    }
    if let Some((status, msg)) = first_error {
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
        return api_error(code, &msg);
    }
    api_error(
        StatusCode::NOT_FOUND,
        "没有 worker 持有该账号(账号不存在、worker 离线或组未被任何 worker 绑定)",
    )
}

/// `POST /accounts/{id}/quota` —— 按需验活:让持有方 worker 确保 token 有效(必要时
/// 刷新,只读)并查一次配额(getUsageLimits)。导入对话框逐账号验活用,**绝不发 chat**。
///
/// 与 refresh 同款**顺序**扇出(虽只读,但保持同一语义最简单):
/// - 2xx:持有方查到结果 → 立即返回;
/// - 404:不持有 → 问下一个;
/// - 其他:持有方查询失败(死号/网络)→ 记首个错误继续,扫完透出(而非误报"无人持有")。
async fn quota_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    let mut first_error: Option<(u16, String)> = None;
    for w in st.workers.iter() {
        let url = format!("http://{}/accounts/{}/quota", w.listen, id);
        let resp = match st.http.post(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!(instance = w.instance, "quota 扇出失败(worker 离线?): {e}");
                continue;
            }
        };
        let status = resp.status().as_u16();
        if status == 404 {
            continue; // 该 worker 不持有此账号。
        }
        let body = resp.json::<serde_json::Value>().await.ok();
        if (200..300).contains(&status) {
            return Json(
                body.unwrap_or_else(|| serde_json::json!({"verified": false, "account_id": id})),
            )
            .into_response();
        }
        if first_error.is_none() {
            let msg = body
                .as_ref()
                .and_then(|b| b.pointer("/error/message"))
                .and_then(|m| m.as_str())
                .unwrap_or("配额查询失败")
                .to_string();
            first_error = Some((status, msg));
        }
    }
    if let Some((status, msg)) = first_error {
        let code = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
        return api_error(code, &msg);
    }
    api_error(
        StatusCode::NOT_FOUND,
        "没有 worker 持有该账号(账号不存在、worker 离线或组未被任何 worker 绑定)",
    )
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use crate::admin::tests_support::{app, app_with_workers, req};

    async fn json_body(resp: axum::response::Response) -> serde_json::Value {
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn accounts_require_admin_token() {
        let (app, _) = app();
        let r = Request::builder()
            .method("GET")
            .uri("/accounts")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(r).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn account_crud_roundtrip_with_redaction() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        store.create_group("G1", "", "", "", None, None).unwrap();
        // 创建(带敏感 extra)。
        let body = r#"{"account_id":"kiro-01","group":"G0","max_concurrency":2,
            "extra":{"refresh_token":"rt-secret-12345678","region":"us-east-1"}}"#;
        let resp = app.clone().oneshot(req("POST", "/accounts", Some(body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let v = json_body(resp).await;
        assert_eq!(v["account_id"], "kiro-01");
        assert_eq!(v["group_name"], "G0");
        assert_eq!(v["provider"], "kiro", "缺省 provider = kiro");
        let rt = v["extra"]["refresh_token"].as_str().unwrap();
        assert!(!rt.contains("rt-secret"), "refresh_token 必须脱敏,实际 {rt}");
        assert!(rt.ends_with("5678"), "保尾 4 位便于识别");
        assert_eq!(v["extra"]["region"], "us-east-1", "非敏感字段原样");
        // 库里存的是完整值(worker 要用)。
        let raw = store.get_account("kiro-01").unwrap().unwrap();
        assert!(raw.extra.contains("rt-secret-12345678"));

        // 重复 409;非法 id 400。
        let resp = app.clone().oneshot(req("POST", "/accounts", Some(body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CONFLICT);
        let resp = app
            .clone()
            .oneshot(req("POST", "/accounts", Some(r#"{"account_id":"bad id/x"}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        // 列表也脱敏。
        let resp = app.clone().oneshot(req("GET", "/accounts", None)).await.unwrap();
        let v = json_body(resp).await;
        assert!(!v[0]["extra"]["refresh_token"].as_str().unwrap().contains("rt-secret"));

        // PATCH:禁用 + 换组,extra 不动。
        let resp = app
            .clone()
            .oneshot(req(
                "PATCH",
                "/accounts/kiro-01",
                Some(r#"{"disabled":true,"group_name":"G1"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["disabled"], true);
        assert_eq!(v["group_name"], "G1");
        let raw = store.get_account("kiro-01").unwrap().unwrap();
        assert!(raw.extra.contains("rt-secret-12345678"), "PATCH 未传 extra 不得改凭据");

        // PATCH 不存在 404;删除;二次删除 404。
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/accounts/ghost", Some(r#"{"disabled":true}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        let resp = app.clone().oneshot(req("DELETE", "/accounts/kiro-01", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let resp = app.oneshot(req("DELETE", "/accounts/kiro-01", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn rejects_nonexistent_group() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        // 创建账号挂不存在的组 → 400(防幽灵分组)。
        let resp = app
            .clone()
            .oneshot(req(
                "POST",
                "/accounts",
                Some(r#"{"account_id":"kiro-g","group":"G0-typo"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // PATCH 到不存在的组同样 400。
        store.create_account("kiro-g", "G0", "kiro", 1, "{}").unwrap();
        let resp = app
            .oneshot(req(
                "PATCH",
                "/accounts/kiro-g",
                Some(r#"{"group_name":"GO"}"#),
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn patch_extra_masked_sentinel_preserves_original() {
        let (app, store) = app();
        store
            .create_account(
                "kiro-rot",
                "",
                "kiro",
                1,
                r#"{"refresh_token":"rt-original-9999","client_secret":"cs-keep-1234","region":"eu"}"#,
            )
            .unwrap();
        // 模拟前端整块回传:轮换 refresh_token,其余敏感字段还是脱敏形态。
        let body = r#"{"extra":{"refresh_token":"rt-rotated-8888","client_secret":"***1234","region":"eu"}}"#;
        let resp = app
            .oneshot(req("PATCH", "/accounts/kiro-rot", Some(body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let raw = store.get_account("kiro-rot").unwrap().unwrap();
        assert!(raw.extra.contains("rt-rotated-8888"), "新 token 应写入");
        assert!(raw.extra.contains("cs-keep-1234"), "脱敏哨兵字段必须保留原值");
        assert!(!raw.extra.contains("***"), "哨兵本身不得落库");
    }

    #[tokio::test]
    async fn redaction_handles_non_ascii_secret() {
        let (app, store) = app();
        store
            .create_account("kiro-cn", "", "kiro", 1, r#"{"password":"秘密口令一二三四"}"#)
            .unwrap();
        // 字节切片会 panic;按字符脱敏必须正常返回(审查 Minimalist#5)。
        let resp = app.oneshot(req("GET", "/accounts", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        let masked = v[0]["extra"]["password"].as_str().unwrap();
        assert!(masked.starts_with("***"));
        assert!(masked.contains("一二三四"), "保尾 4 个字符,实际 {masked}");
    }

    #[tokio::test]
    async fn redaction_covers_api_key_fields() {
        let (app, store) = app();
        store
            .create_account("kiro-ak", "", "kiro", 1, r#"{"kiro_api_key":"ksk-super-secret-9999"}"#)
            .unwrap();
        let resp = app.oneshot(req("GET", "/accounts", None)).await.unwrap();
        let v = json_body(resp).await;
        let masked = v[0]["extra"]["kiro_api_key"].as_str().unwrap();
        // kiro_api_key 不含 token/secret/password,必须靠 "key" 规则脱敏,否则明文泄漏。
        assert!(masked.starts_with("***"), "kiro_api_key 必须脱敏,实际 {masked}");
        assert!(!masked.contains("ksk-super-secret"), "明文不得出现:{masked}");
    }

    #[tokio::test]
    async fn import_creates_account_with_machine_id() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        // KiroManager 导出(单 Enterprise 号),json 以字符串形式传入。
        let export = serde_json::json!({
            "version": "1.7.5",
            "accounts": [{
                "email": "newco@example.com",
                "machineId": "a".repeat(64),
                "credentials": {
                    "refreshToken": "rt-export-aaa",
                    "accessToken": "at-export",
                    "clientId": "cid", "clientSecret": "csecret",
                    "region": "us-east-1", "provider": "Enterprise",
                    "profileArn": "arn:aws:codewhisperer:us-east-1:1:profile/X"
                },
                "subscription": {"title": "KIRO POWER"}
            }]
        });
        let body = serde_json::json!({"group_name": "G0", "json": export.to_string()}).to_string();
        let resp = app.clone().oneshot(req("POST", "/accounts/import", Some(&body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["created"], 1);
        assert_eq!(v["items"][0]["account_id"], "newco-example.com");
        assert_eq!(v["items"][0]["has_machine_id"], true);
        // 库里:machineId 等关键字段落库,client_secret/refresh_token 是完整值。
        let row = store.get_account("newco-example.com").unwrap().unwrap();
        assert!(row.extra.contains(&"a".repeat(64)), "machineId 必须落库");
        assert!(row.extra.contains("rt-export-aaa"));
        assert!(row.extra.contains("\"client_secret\":\"csecret\""));
        assert!(row.extra.contains("\"kiro_provider\":\"enterprise\""));
        // 不得映射 auth_method(避免误触发 TokenType 头)。
        assert!(!row.extra.contains("auth_method"));
    }

    #[tokio::test]
    async fn import_with_batch_proxy_sets_proxy_on_new_account() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        let export = serde_json::json!({
            "accounts": [{
                "email": "proxied@example.com",
                "machineId": "b".repeat(64),
                "credentials": {"refreshToken": "rt-x", "provider": "BuilderId"}
            }]
        });
        let body = serde_json::json!({
            "group_name": "G0",
            "json": export.to_string(),
            "batch_proxy": "socks5://user:pass@h:1080"
        })
        .to_string();
        let resp = app.oneshot(req("POST", "/accounts/import", Some(&body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let row = store.get_account("proxied-example.com").unwrap().unwrap();
        assert!(
            row.extra.contains("\"proxy\":\"socks5://user:pass@h:1080\""),
            "批量代理应写进 extra.proxy: {}",
            row.extra
        );
    }

    #[tokio::test]
    async fn update_proxy_url_merges_without_touching_credentials() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        store
            .create_account(
                "acc1",
                "G0",
                "kiro",
                2,
                r#"{"refresh_token":"rt-secret","client_secret":"cs"}"#,
            )
            .unwrap();
        // 仅设 proxy_url:不带 extra,凭据必须原样保留。
        let body = serde_json::json!({"proxy_url": "http://p:8888"}).to_string();
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/accounts/acc1", Some(&body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let row = store.get_account("acc1").unwrap().unwrap();
        assert!(row.extra.contains("\"proxy\":\"http://p:8888\""), "proxy 应写入");
        assert!(row.extra.contains("rt-secret"), "凭据不得被定点合并冲掉");
        assert!(row.extra.contains("\"client_secret\":\"cs\""));
        // proxy_url="" 清除(写入 JSON null,resolver 视作无代理)。
        let body2 = serde_json::json!({"proxy_url": ""}).to_string();
        let resp2 = app
            .oneshot(req("PATCH", "/accounts/acc1", Some(&body2)))
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::OK);
        let row2 = store.get_account("acc1").unwrap().unwrap();
        assert!(row2.extra.contains("\"proxy\":null"), "清除应写 proxy:null: {}", row2.extra);
        assert!(row2.extra.contains("rt-secret"), "清除代理不得动凭据");
    }

    #[tokio::test]
    async fn update_priority_merges_without_touching_credentials() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        store
            .create_account(
                "acc1",
                "G0",
                "kiro",
                2,
                r#"{"refresh_token":"rt-secret","client_secret":"cs"}"#,
            )
            .unwrap();
        // 仅设 priority:不带 extra,凭据必须原样保留;顶层视图应回显新值。
        let body = serde_json::json!({"priority": 30}).to_string();
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/accounts/acc1", Some(&body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let view: serde_json::Value = json_body(resp).await;
        assert_eq!(view["priority"].as_i64(), Some(30), "顶层 priority 应回显: {view}");
        let row = store.get_account("acc1").unwrap().unwrap();
        assert!(row.extra.contains("\"priority\":30"), "priority 应写入 extra: {}", row.extra);
        assert!(row.extra.contains("rt-secret"), "凭据不得被定点合并冲掉");
        assert!(row.extra.contains("\"client_secret\":\"cs\""));
    }

    #[tokio::test]
    async fn view_defaults_priority_to_100_when_absent() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        store
            .create_account("acc1", "G0", "kiro", 2, r#"{"refresh_token":"rt"}"#)
            .unwrap();
        let resp = app
            .oneshot(req("GET", "/accounts", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let list: serde_json::Value = json_body(resp).await;
        assert_eq!(
            list[0]["priority"].as_i64(),
            Some(100),
            "无 extra.priority 时顶层应回落 100: {list}"
        );
    }

    #[tokio::test]
    async fn update_extra_rotation_and_priority_together_both_apply() {
        // 同一 PATCH 同时轮换 token(整块 extra 替换)+ 改 priority(定点合并)。priority 合并
        // 硬编码在整块替换【之后】,即便 extra 里带的是打开弹窗时的旧 priority 快照,最终也应是
        // 新值 —— 锁定这个顺序不变量(审查 Low#3),防未来重排三段顺序静默退化。
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        store
            .create_account("acc1", "G0", "kiro", 2, r#"{"refresh_token":"rt-old","priority":100}"#)
            .unwrap();
        let body = serde_json::json!({
            "extra": {"refresh_token": "rt-new", "priority": 100},
            "priority": 30
        })
        .to_string();
        let resp = app
            .oneshot(req("PATCH", "/accounts/acc1", Some(&body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let row = store.get_account("acc1").unwrap().unwrap();
        assert!(row.extra.contains("rt-new"), "token 应被轮换: {}", row.extra);
        assert!(
            row.extra.contains("\"priority\":30"),
            "priority 定点合并应覆盖旧快照为 30: {}",
            row.extra
        );
    }

    /// PATCH 落库后应立即 poke worker `/sync`,消除"改配置(启用/禁用/换组)后 30s 内
    /// 按号操作(如立即验活)报『没有 worker 持有该账号』"的窗口——与 delete_account 同理。
    #[tokio::test]
    async fn update_account_pokes_workers_for_immediate_sync() {
        use axum::routing::post;
        use axum::Json;
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;

        let sync_calls = Arc::new(AtomicUsize::new(0));
        let counter = sync_calls.clone();
        let router = axum::Router::new().route(
            "/sync",
            post(move || {
                let counter = counter.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    Json(serde_json::json!({ "ok": true }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let worker = gw_core::config::WorkerConfig {
            instance: 0,
            listen: addr.to_string(),
            egress: gw_core::config::EgressConfig::Direct,
            account_group: "".to_string(),
        };

        let (app, store) = app_with_workers(vec![worker]);
        store.create_account("kiro-poke", "", "kiro", 1, "{}").unwrap();

        let resp = app
            .oneshot(req("PATCH", "/accounts/kiro-poke", Some(r#"{"disabled":false}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            sync_calls.load(Ordering::SeqCst),
            1,
            "PATCH 应立即 poke worker /sync,不等 30s 周期同步"
        );
    }

    #[tokio::test]
    async fn import_smart_merge_backfills_machine_id_keeps_server_token() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        // 已存在账号:有服务器已 roll 的 rt,但无 machineId(正是待修复的老号)。
        store
            .create_account(
                "dave-example.com",
                "G0",
                "kiro",
                1,
                r#"{"refresh_token":"rt-server-rolled","region":"us-east-1"}"#,
            )
            .unwrap();
        // 导入同一号(email 派生同 account_id),带真机 machineId + 旧 rt。
        let export = serde_json::json!({
            "accounts": [{
                "email": "dave@example.com",
                "machineId": "b".repeat(64),
                "credentials": {"refreshToken": "rt-export-STALE", "region": "us-east-1", "provider": "BuilderId"}
            }]
        });
        let body = serde_json::json!({"group_name": "G0", "json": export.to_string()}).to_string();
        let resp = app.oneshot(req("POST", "/accounts/import", Some(&body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v["merged"], 1, "已存在 → merged");
        let row = store.get_account("dave-example.com").unwrap().unwrap();
        // machineId 被补上(关键修复)。
        assert!(row.extra.contains(&"b".repeat(64)), "machineId 应补齐");
        // 服务器已 roll 的 rt **不被**导出里的旧 rt 覆盖。
        assert!(row.extra.contains("rt-server-rolled"), "服务器 token 必须保留");
        assert!(!row.extra.contains("rt-export-STALE"), "导出里的旧 rt 不得覆盖");
        // kiro_provider 是新补的字段。
        assert!(row.extra.contains("\"kiro_provider\":\"builderid\""));
    }

    #[tokio::test]
    async fn import_collision_does_not_merge_two_real_accounts() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        // 两个不同真号(不同 userId),email 清洗后撞同一 account_id "a-b-x.com"。
        let export = serde_json::json!({
            "accounts": [
                {"email": "a+b@x.com", "userId": "u-FIRST", "machineId": "a".repeat(64),
                 "credentials": {"refreshToken": "rt-first", "provider": "BuilderId"}},
                {"email": "a-b@x.com", "userId": "u-SECOND", "machineId": "b".repeat(64),
                 "credentials": {"refreshToken": "rt-second", "provider": "BuilderId"}}
            ]
        });
        let body = serde_json::json!({"group_name": "G0", "json": export.to_string()}).to_string();
        let resp = app.oneshot(req("POST", "/accounts/import", Some(&body))).await.unwrap();
        let v = json_body(resp).await;
        // 第一个创建,第二个因碰撞被跳过——绝不合并进第一个。
        assert_eq!(v["created"], 1);
        assert_eq!(v["skipped"], 1);
        let row = store.get_account("a-b-x.com").unwrap().unwrap();
        assert!(row.extra.contains("u-FIRST"), "应是第一个真号");
        assert!(!row.extra.contains("u-SECOND"), "第二个真号绝不能并进来");
        assert!(!row.extra.contains("rt-second"), "第二个号的凭据不得污染第一个");
    }

    #[tokio::test]
    async fn import_merge_never_overwrites_server_token_even_if_missing() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        // 已存在号:有 machineId,但 access_token 为空(待刷新)。
        store
            .create_account(
                "joe-x.com",
                "G0",
                "kiro",
                1,
                &serde_json::json!({"refresh_token":"rt-srv","machine_id":"c".repeat(64),"email":"joe@x.com"}).to_string(),
            )
            .unwrap();
        // 导入带 access_token —— 但 token 是"仅创建"字段,合并不得写入(消除覆盖竞态)。
        let export = serde_json::json!({"accounts": [{
            "email": "joe@x.com",
            "credentials": {"refreshToken": "rt-export", "accessToken": "at-export-STALE",
                            "region": "us-east-1", "provider": "BuilderId"}
        }]});
        let body = serde_json::json!({"group_name": "G0", "json": export.to_string()}).to_string();
        let resp = app.oneshot(req("POST", "/accounts/import", Some(&body))).await.unwrap();
        let v = json_body(resp).await;
        let row = store.get_account("joe-x.com").unwrap().unwrap();
        assert!(!row.extra.contains("at-export-STALE"), "access_token 合并时不得写入");
        assert!(!row.extra.contains("rt-export"), "refresh_token 合并时不得覆盖");
        assert!(row.extra.contains("rt-srv"), "服务器 token 保留");
        // 但非 token 身份字段(region/kiro_provider)正常补齐。
        assert!(row.extra.contains("\"kiro_provider\":\"builderid\""));
        assert_eq!(v["merged"], 1);
    }

    #[tokio::test]
    async fn import_rejects_nonexistent_group_and_bad_json() {
        let (app, store) = app();
        store.create_group("G0", "", "", "", None, None).unwrap();
        // 不存在的组 → 400。
        let body = serde_json::json!({"group_name": "GHOST", "json": "{}"}).to_string();
        let resp = app.clone().oneshot(req("POST", "/accounts/import", Some(&body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // 非 KiroManager 格式(无 accounts)→ 400。
        let body = serde_json::json!({"group_name": "G0", "json": "{\"version\":\"x\"}"}).to_string();
        let resp = app.oneshot(req("POST", "/accounts/import", Some(&body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn reset_validates_id_and_404s_without_workers() {
        let (app, _) = app();
        // 非法 id → 400(不发起任何扇出)。
        let resp = app
            .clone()
            .oneshot(req("POST", "/accounts/bad%2Fid/reset", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // 无 worker 在线 → 404(运行态只存在于 worker 内存,无人持有即无可重置)。
        let resp = app
            .oneshot(req("POST", "/accounts/k1/reset", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn refresh_validates_id_and_404s_without_workers() {
        let (app, _) = app();
        // 非法 id → 400(不发起任何扇出)。
        let resp = app
            .clone()
            .oneshot(req("POST", "/accounts/bad%2Fid/refresh", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // 无 worker 在线 → 404(token 刷新发生在 worker 内存的选号池,无人持有即无可刷新)。
        let resp = app
            .oneshot(req("POST", "/accounts/k1/refresh", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn refresh_requires_admin_token() {
        let (app, _) = app();
        let r = Request::builder()
            .method("POST")
            .uri("/accounts/k1/refresh")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(r).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn quota_validates_id_and_404s_without_workers() {
        let (app, _) = app();
        // 非法 id → 400(不发起任何扇出)。
        let resp = app
            .clone()
            .oneshot(req("POST", "/accounts/bad%2Fid/quota", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // 无 worker 在线 → 404(配额查询发生在持有方 worker,无人持有即无可验)。
        let resp = app
            .oneshot(req("POST", "/accounts/k1/quota", None))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn quota_requires_admin_token() {
        let (app, _) = app();
        let r = Request::builder()
            .method("POST")
            .uri("/accounts/k1/quota")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(r).await.unwrap().status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn runtime_with_no_workers_returns_empty() {
        let (app, _) = app();
        let resp = app.oneshot(req("GET", "/accounts/runtime", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let v = json_body(resp).await;
        assert_eq!(v.as_array().unwrap().len(), 0);
    }

    // ── claude-dario 创建账号 ────────────────────────────────────────────────

    /// 提供合法 .credentials.json → 账号 extra 含 access_token,provider=claude-dario,
    /// device_id / account_uuid 为 36 字符 UUID。
    #[tokio::test]
    async fn create_dario_account_parses_credentials_json() {
        let (app, store) = app();
        let creds_json =
            r#"{"claudeAiOauth":{"accessToken":"at-test-tok","refreshToken":"rt-test-tok","expiresAt":1780531200000}}"#;
        let body = serde_json::json!({
            "account_id": "dario-01",
            "provider": "claude-dario",
            "credentials_json": creds_json,
            "max_concurrency": 1
        })
        .to_string();
        let resp = app.clone().oneshot(req("POST", "/accounts", Some(&body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED, "应 201 Created");
        let v = json_body(resp).await;
        assert_eq!(v["provider"], "claude-dario", "provider 必须保留");

        // 库里存的是完整 extra(worker 要用)。
        let raw = store.get_account("dario-01").unwrap().unwrap();
        let extra: serde_json::Value = serde_json::from_str(&raw.extra).unwrap();
        assert_eq!(extra["access_token"], "at-test-tok", "access_token 须并入");
        assert_eq!(extra["refresh_token"], "rt-test-tok", "refresh_token 须并入");

        // device_id / account_uuid 自动生成,必须是 36 字符的 UUID。
        let dev = extra["device_id"].as_str().expect("device_id 存在");
        let uid = extra["account_uuid"].as_str().expect("account_uuid 存在");
        assert_eq!(dev.len(), 36, "device_id 应为 UUID(36 字符),实际: {dev}");
        assert_eq!(uid.len(), 36, "account_uuid 应为 UUID(36 字符),实际: {uid}");
        assert_eq!(dev.chars().filter(|&c| c == '-').count(), 4, "UUID 应含 4 个连字符");
    }

    /// provider=claude-dario 但 credentials_json 为空 → 账号仍创建成功,
    /// device_id / account_uuid 依旧自动生成(操作者可稍后通过 PATCH 补凭据)。
    #[tokio::test]
    async fn create_dario_account_without_credentials_json_still_creates() {
        let (app, store) = app();
        let body = serde_json::json!({
            "account_id": "dario-02",
            "provider": "claude-dario"
        })
        .to_string();
        let resp = app.clone().oneshot(req("POST", "/accounts", Some(&body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let raw = store.get_account("dario-02").unwrap().unwrap();
        let extra: serde_json::Value = serde_json::from_str(&raw.extra).unwrap();
        let dev = extra["device_id"].as_str().expect("device_id 存在");
        assert_eq!(dev.len(), 36, "无凭证时也应生成 device_id");
    }

    /// credentials_json 格式非法 → 400,账号不落库。
    #[tokio::test]
    async fn create_dario_account_bad_credentials_json_returns_400() {
        let (app, store) = app();
        let body = serde_json::json!({
            "account_id": "dario-bad",
            "provider": "claude-dario",
            "credentials_json": r#"{"not_valid": true}"#
        })
        .to_string();
        let resp = app.clone().oneshot(req("POST", "/accounts", Some(&body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "应 400");
        let resp_json = json_body(resp).await;
        let msg = resp_json["error"]["message"].as_str().unwrap_or("");
        assert!(msg.contains("credentials.json"), "错误信息应提及 credentials.json,实际: {msg}");
        assert!(store.get_account("dario-bad").unwrap().is_none(), "账号不得落库");
    }

    /// 操作者在 extra 里显式填了 device_id → 不覆盖(entry.or_insert_with 语义)。
    #[tokio::test]
    async fn create_dario_account_does_not_overwrite_explicit_device_id() {
        let (app, store) = app();
        let creds_json = r#"{"claudeAiOauth":{"accessToken":"at2","refreshToken":"rt2","expiresAt":1780531200000}}"#;
        let body = serde_json::json!({
            "account_id": "dario-03",
            "provider": "claude-dario",
            "credentials_json": creds_json,
            "extra": { "device_id": "my-explicit-device-id-0000000000000" }
        })
        .to_string();
        let resp = app.clone().oneshot(req("POST", "/accounts", Some(&body))).await.unwrap();
        assert_eq!(resp.status(), StatusCode::CREATED);
        let raw = store.get_account("dario-03").unwrap().unwrap();
        let extra: serde_json::Value = serde_json::from_str(&raw.extra).unwrap();
        assert_eq!(
            extra["device_id"].as_str().unwrap(),
            "my-explicit-device-id-0000000000000",
            "显式 device_id 不得被覆盖"
        );
    }

    // === OAuth 上号生命周期(B3 审查补测)===
    // 假 worker(真 TCP 端口):/health 报指定 provider;/oauth/exchange 按指定 HTTP 码 + body 应答;
    // /sync 恒 200(complete 成功后 best-effort 调用)。覆盖 start 预检 + complete 扇出/落库/会话保留语义。
    async fn spawn_fake_worker(
        group: &str,
        provider: &str,
        exchange_status: u16,
        exchange_body: serde_json::Value,
    ) -> gw_core::config::WorkerConfig {
        use axum::routing::{get, post};
        use axum::Json;
        let provider = provider.to_string();
        let body = exchange_body;
        let router = axum::Router::new()
            .route(
                "/health",
                get(move || {
                    let p = provider.clone();
                    async move { Json(serde_json::json!({ "provider": p })) }
                }),
            )
            .route(
                "/oauth/exchange",
                post(move |_b: Json<serde_json::Value>| {
                    let body = body.clone();
                    async move {
                        (axum::http::StatusCode::from_u16(exchange_status).unwrap(), Json(body))
                    }
                }),
            )
            .route("/sync", post(|| async { Json(serde_json::json!({ "ok": true })) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        gw_core::config::WorkerConfig {
            instance: 0,
            listen: addr.to_string(),
            egress: gw_core::config::EgressConfig::Direct,
            account_group: group.to_string(),
        }
    }
    // CHUNK_ANCHOR_OAUTH_TESTS

    /// start 后从响应取 (status, authorize_url, state)。
    async fn start(app: &axum::Router, account_id: &str, group: &str) -> (StatusCode, serde_json::Value) {
        let body = serde_json::json!({ "account_id": account_id, "group": group }).to_string();
        let resp = app
            .clone()
            .oneshot(req("POST", "/accounts/oauth/start", Some(&body)))
            .await
            .unwrap();
        let status = resp.status();
        (status, json_body(resp).await)
    }

    /// complete 调用(code 可含 `#state`)。
    async fn complete(app: &axum::Router, state: &str, code: &str) -> (StatusCode, serde_json::Value) {
        let body = serde_json::json!({ "state": state, "code": code }).to_string();
        let resp = app
            .clone()
            .oneshot(req("POST", "/accounts/oauth/complete", Some(&body)))
            .await
            .unwrap();
        let status = resp.status();
        (status, json_body(resp).await)
    }

    /// 空组(无 worker)→ start 阶段即 400,**在浏览器 consent 之前**失败(审查 Architect B2:
    /// 不让操作员登录半天才在 complete 撞死路)。
    #[tokio::test]
    async fn oauth_start_rejects_group_without_worker_before_consent() {
        let (app, _store) = app_with_workers(vec![]);
        let (status, _v) = start(&app, "dario-x", "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "无 worker 的组应在 start 即拒");
    }
    // CHUNK_ANCHOR_OAUTH_TESTS2

    /// start 命中真实 claude-dario 组 → 返回 authorize_url + state。
    #[tokio::test]
    async fn oauth_start_returns_authorize_url_for_dario_group() {
        let w = spawn_fake_worker("G0", "claude-dario", 200, serde_json::json!({})).await;
        let (app, store) = app_with_workers(vec![w]);
        store.create_group("G0", "", "", "", None, None).unwrap();
        let (status, v) = start(&app, "dario-ok", "G0").await;
        assert_eq!(status, StatusCode::OK);
        let url = v["authorize_url"].as_str().unwrap();
        assert!(url.starts_with("https://claude.ai/oauth/authorize?"), "authorize_url: {url}");
        assert!(url.contains("code_challenge_method=S256"));
        assert!(!v["state"].as_str().unwrap_or("").is_empty(), "必须返回 state");
    }

    /// 非 claude-dario 组(如 kiro)→ start 即 400(OAuth 上号仅支持 dario)。
    #[tokio::test]
    async fn oauth_start_rejects_non_dario_group() {
        let w = spawn_fake_worker("G0", "kiro", 200, serde_json::json!({})).await;
        let (app, store) = app_with_workers(vec![w]);
        store.create_group("G0", "", "", "", None, None).unwrap();
        let (status, _v) = start(&app, "k-1", "G0").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "非 dario 组应拒");
    }

    /// 贴回串自带的 `#state` 与会话 state 不符 → 400,且**不消费**会话(可用正确 code 重试)。
    #[tokio::test]
    async fn oauth_complete_state_mismatch_does_not_consume_session() {
        let tokens = serde_json::json!({
            "access_token": "at-1", "refresh_token": "rt-1", "expires_at": "2026-06-04T01:00:00Z"
        });
        let w = spawn_fake_worker("G0", "claude-dario", 200, tokens).await;
        let (app, store) = app_with_workers(vec![w]);
        store.create_group("G0", "", "", "", None, None).unwrap();
        let (_s, v) = start(&app, "dario-mm", "G0").await;
        let state = v["state"].as_str().unwrap().to_string();
        // 贴回 code 带错误的 #state → 400,会话保留。
        let (status, _v) = complete(&app, &state, "thecode#totally-wrong-state").await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "state 不匹配应拒");
        // 用正确 code(无 #state)重试 → 成功,证明上一步没消费会话。
        let (status2, _v2) = complete(&app, &state, "thecode").await;
        assert_eq!(status2, StatusCode::CREATED, "会话应仍在,重试成功");
        assert!(store.get_account("dario-mm").unwrap().is_some());
    }

    /// 成功换码 → 账号以 claude-dario 入库,自动补稳定身份 device_id/account_uuid(36 字符 UUID)。
    #[tokio::test]
    async fn oauth_complete_success_creates_dario_account_with_identity() {
        let tokens = serde_json::json!({
            "access_token": "at-onboard-123",
            "refresh_token": "rt-onboard-456",
            "expires_at": "2026-06-04T01:00:00Z"
        });
        let w = spawn_fake_worker("G0", "claude-dario", 200, tokens).await;
        let (app, store) = app_with_workers(vec![w]);
        store.create_group("G0", "", "", "", None, None).unwrap();
        let (_s, v) = start(&app, "dario-new", "G0").await;
        let state = v["state"].as_str().unwrap().to_string();
        let (status, _v) = complete(&app, &state, "validcode").await;
        assert_eq!(status, StatusCode::CREATED);
        let row = store.get_account("dario-new").unwrap().unwrap();
        assert_eq!(row.provider, "claude-dario");
        assert_eq!(row.group_name, "G0");
        let extra: serde_json::Value = serde_json::from_str(&row.extra).unwrap();
        assert_eq!(extra["access_token"], "at-onboard-123", "库存完整 token");
        assert_eq!(extra["refresh_token"], "rt-onboard-456");
        assert_eq!(extra["device_id"].as_str().unwrap().len(), 36, "自动生成 device_id UUID");
        assert_eq!(extra["account_uuid"].as_str().unwrap().len(), 36);
    }
    // CHUNK_ANCHOR_OAUTH_TESTS3

    /// 成功换码后 state 单次消费:同一 state 二次 complete → 会话已被取走 → 400。
    #[tokio::test]
    async fn oauth_complete_consumes_state_once_on_success() {
        let tokens = serde_json::json!({
            "access_token": "at-1", "refresh_token": "rt-1", "expires_at": "2026-06-04T01:00:00Z"
        });
        let w = spawn_fake_worker("G0", "claude-dario", 200, tokens).await;
        let (app, store) = app_with_workers(vec![w]);
        store.create_group("G0", "", "", "", None, None).unwrap();
        let (_s, v) = start(&app, "dario-once", "G0").await;
        let state = v["state"].as_str().unwrap().to_string();
        let (s1, _) = complete(&app, &state, "code-1").await;
        assert_eq!(s1, StatusCode::CREATED);
        // 二次用同 state(且账号已存在)→ 会话已消费 → 400(过期/无效),而非 CONFLICT。
        let (s2, _) = complete(&app, &state, "code-2").await;
        assert_eq!(s2, StatusCode::BAD_REQUEST, "state 应单次消费");
    }

    /// **A1 修复**:换码前撞重名 409,code 尚未递交上游、仍有效 → 会话**保留**,
    /// 改用其他 account_id 可继续完成(绝不 destructive 销毁逼操作员重新 consent)。
    #[tokio::test]
    async fn oauth_complete_duplicate_id_preserves_session_for_retry() {
        let tokens = serde_json::json!({
            "access_token": "at-9", "refresh_token": "rt-9", "expires_at": "2026-06-04T01:00:00Z"
        });
        let w = spawn_fake_worker("G0", "claude-dario", 200, tokens).await;
        let (app, store) = app_with_workers(vec![w]);
        store.create_group("G0", "", "", "", None, None).unwrap();
        let (_s, v) = start(&app, "dario-dup", "G0").await;
        let state = v["state"].as_str().unwrap().to_string();
        // start 之后、complete 之前,另一路创建了同名账号(模拟并发)。
        store.create_account("dario-dup", "G0", "claude-dario", 2, "{}").unwrap();
        // complete → 重名 409,但会话保留。
        let (s1, _) = complete(&app, &state, "thecode").await;
        assert_eq!(s1, StatusCode::CONFLICT, "重名应 409");
        // 删掉冲突账号后,同一会话用同一 state 重试 → 成功(证明会话未被销毁)。
        let resp = app.clone().oneshot(req("DELETE", "/accounts/dario-dup", None)).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NO_CONTENT);
        let (s2, _) = complete(&app, &state, "thecode").await;
        assert_eq!(s2, StatusCode::CREATED, "会话应被保留,重试成功");
    }

    /// 上游拒换码(worker 应答非 2xx,如 code 已失效)→ code 已被上游消费,重试无益 →
    /// 会话**消费**不再保留(防留可重放凭据),错误透出。
    #[tokio::test]
    async fn oauth_complete_upstream_reject_consumes_session() {
        let err = serde_json::json!({"error": {"message": "invalid_grant"}});
        let w = spawn_fake_worker("G0", "claude-dario", 400, err).await;
        let (app, store) = app_with_workers(vec![w]);
        store.create_group("G0", "", "", "", None, None).unwrap();
        let (_s, v) = start(&app, "dario-rej", "G0").await;
        let state = v["state"].as_str().unwrap().to_string();
        let (s1, _) = complete(&app, &state, "expiredcode").await;
        assert!(s1.is_client_error() || s1.is_server_error(), "上游拒应透出错误,实际 {s1}");
        assert!(store.get_account("dario-rej").unwrap().is_none(), "失败不得落库");
        // 同一 state 重试 → 会话已消费 → 400(不可重放)。
        let (s2, _) = complete(&app, &state, "expiredcode").await;
        assert_eq!(s2, StatusCode::BAD_REQUEST, "上游拒后会话应已消费");
    }
}
