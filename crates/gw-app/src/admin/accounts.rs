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
use gw_store::UpdateAccountOutcome;
use serde::Deserialize;

use super::{internal_error, redact_proxy_url, validate_proxy_url, AdminState};

/// 从设置 overlay 读 `egress_pool`(trim、去空);解析失败/未配置 → 空 Vec。
pub(crate) fn read_egress_pool(store: &gw_store::SqliteStore) -> Vec<String> {
    let overlay: SystemSettings = match store.get_settings() {
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
pub(crate) fn account_proxy(extra_json: &str) -> Option<String> {
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
pub(crate) struct EgressAssigner {
    /// (池 URL, 当前已分配账号数)。
    counts: Vec<(String, usize)>,
}

impl EgressAssigner {
    /// 从设置 `egress_pool` + 现有账号分布构造;池为空/未配置 → None(不自动分配)。
    fn from_settings(store: &gw_store::SqliteStore) -> Option<Self> {
        let pool = read_egress_pool(store);
        if pool.is_empty() {
            return None;
        }
        let mut counts: Vec<(String, usize)> = pool.into_iter().map(|u| (u, 0usize)).collect();
        if let Ok(rows) = store.list_accounts() {
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
pub(crate) enum EgressPicker {
    Direct,
    Fixed(String),
    Auto(EgressAssigner),
}

impl EgressPicker {
    /// ⚠️ 退回直连**必须报错**,不能像 2026-08-06 之前那样静默发生。
    ///
    /// 那次的代价:`egress:"auto"` 遇到空池时无声变直连,42 个自动补货的号全部
    /// 从服务器主 IP 出去,被 AWS 按出口 IP 关联,**59.5% 被 TEMPORARILY_SUSPENDED**
    /// (同期走代理池的 11 个号一个没封)。面板上一切正常,只有逐个翻账号的
    /// `extra.proxy` 才看得出来。请求方明确要了一个出口却拿到直连,是配置事故,
    /// 不是可以默默降级的默认值。
    pub(crate) fn build(store: &gw_store::SqliteStore, sel: Option<&str>) -> Self {
        let pool = read_egress_pool(store);
        match sel.map(str::trim) {
            None | Some("") | Some("direct") => EgressPicker::Direct,
            Some("auto") => match EgressAssigner::from_settings(store) {
                Some(a) => EgressPicker::Auto(a),
                None => {
                    tracing::error!(
                        "出口分配失败:请求 egress=auto 但 egress_pool 为空/不可读 —— \
                         本批账号将全部走直连(服务器主 IP)。同 IP 上的号会被上游关联封禁, \
                         请在设置里配置 egress_pool。"
                    );
                    EgressPicker::Direct
                }
            },
            Some(s) => match s.parse::<usize>() {
                Ok(i) if i < pool.len() => EgressPicker::Fixed(pool[i].clone()),
                // 无效索引(网关被删/越界)→ 退回直连,绝不乱投到错误出口。
                _ => {
                    tracing::error!(
                        egress = s,
                        pool_len = pool.len(),
                        "出口分配失败:egress 索引越界或非法(网关被删?)—— 本批账号将全部走直连"
                    );
                    EgressPicker::Direct
                }
            },
        }
    }

    /// 取本账号应写入 `extra.proxy` 的 URL(直连=None;选定=固定;自动=最少使用)。
    pub(crate) fn next(&mut self) -> Option<String> {
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
    /// 定点开关「排队等冷却」(写 `extra.queue_enabled`)。缺省=不动。
    ///
    /// 逐账号而非全局:企业号的上游并发跨租户共享,429 是跟别人抢、等一下就有;
    /// 社交号的 429 常伴随额度见底,等待只会把客户多挂几秒后照样报错。
    /// 见 `worker/scheduler.rs::queue_enabled`。
    #[serde(default)]
    queue_enabled: Option<bool>,
    /// 定点更新模型白名单(写 `extra.model_allowlist`,走 merge_account_extra
    /// 绝不碰凭据,仿 proxy_url)。缺省=不动;`""`(空串/纯分隔符)=清除(写 null,
    /// 读侧把 null 与缺失同义为「不限」);非空=逗号/分号/空白分隔的条目列表,
    /// 写侧校验后规范化成 **JSON 字符串数组**落库(交接规格第 3 条:CSV 只当
    /// UI 输入形态)。非法条目(裸 `*`、通配符不在末尾、非法字符)直接 400 ——
    /// fail-closed,绝不静默收下一个语义存疑的白名单(规格第 4/5 条)。
    #[serde(default)]
    model_allowlist: Option<String>,
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/accounts", get(list_accounts).post(create_account))
        .route("/accounts/oauth/start", post(oauth_start))
        .route("/accounts/oauth/complete", post(oauth_complete))
        // Cursor 官方登录流(PKCE + 轮询):start 拿链接,poll 由前端按秒级重复调用。
        .route("/accounts/cursor/login/start", post(cursor_login_start))
        .route("/accounts/cursor/login/poll", post(cursor_login_poll))
        .route("/accounts/import", post(import_accounts))
        .route("/accounts/import-apikeys", post(import_apikeys))
        .route("/accounts/rebalance-egress", post(rebalance_egress))
        .route("/accounts/runtime", get(runtime))
        .route("/accounts/{id}", patch(update_account).delete(delete_account))
        .route("/accounts/{id}/reset", post(reset_account))
        .route("/accounts/{id}/refresh", post(refresh_account))
        .route("/accounts/{id}/quota", post(quota_account))
        .route("/accounts/{id}/on-demand", post(on_demand_account))
        .route("/accounts/{id}/models", post(models_account))
        .route("/accounts/{id}/models/local", get(models_local_account))
        .route("/accounts/{id}/probe", post(probe_account))
        .route("/models/catalog", get(model_catalog))
}

fn api_error(status: StatusCode, msg: &str) -> axum::response::Response {
    (
        status,
        Json(serde_json::json!({"type":"error","error":{"message": msg}})),
    )
        .into_response()
}

/// `model_allowlist` 的写侧校验与归一(交接规格第 3/4/5 条)。
///
/// 输入是逗号/分号/空白分隔的条目串(UI 的自然输入形态);
/// - 空 / 纯分隔符 → `Ok(null)` = 清除(读侧把 null 与缺失同义为「不限」);
/// - 非空 → 逐条校验后规范化为**小写 JSON 字符串数组**(规范存储形态);
/// - 非法条目 → `Err(400 文案)`,整单拒绝不做部分应用 —— fail-closed,
///   绝不静默收下一个语义存疑的白名单。
///
/// 逐条规则:
/// - 裸 `*` 拒绝:「不限」用清空表达,「全禁」用账号 disabled 表达(规格第 4 条表尾);
/// - `*` 只许出现在末尾(`grok*` 合法、`gr*k`/`*grok` 拒绝,规格第 5 条);
/// - 字符集只允许字母/数字/`-`/`.`/`_`(Run 侧目录名的实际字符集)。
fn normalize_model_allowlist(raw: &str) -> Result<serde_json::Value, String> {
    let mut items: Vec<serde_json::Value> = Vec::new();
    for tok in raw.split([',', ';']).flat_map(str::split_whitespace) {
        let t = tok.trim().to_ascii_lowercase();
        if t.is_empty() {
            continue;
        }
        if t == "*" {
            return Err(
                "model_allowlist 不接受裸 `*`:不限就把该字段清空,全禁请直接停用账号".into(),
            );
        }
        let body = t.strip_suffix('*').unwrap_or(&t);
        if body.contains('*') {
            return Err(format!(
                "model_allowlist 条目 {t:?} 非法:通配符 `*` 只允许出现在末尾(如 grok*)"
            ));
        }
        if body.is_empty()
            || !body
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_'))
        {
            return Err(format!(
                "model_allowlist 条目 {t:?} 非法:只允许字母/数字/`-`/`.`/`_`(可选末尾 `*`)"
            ));
        }
        items.push(serde_json::Value::String(t));
    }
    if items.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    Ok(serde_json::Value::Array(items))
}

/// 控制面扇出专用 http client(30s 超时)。
///
/// ⚠️ **不能用 `st.http`**:那是 2s 超时的管理面聚合客户端,为的是 worker 离线时快速
/// 跳过。而配额/模型目录这类扇出,worker 侧要真打一次上游控制面,>2s 是常态 —— 2s
/// 超时在扇出循环里等价于「该 worker 离线」,所有 worker 轮一遍后误报 404「没有
/// worker 持有该账号」,直连同一 worker 却是 200(2026-08-10 线上确诊)。
/// probe 端点早就踩过同款(见其注释,用 120s)。quota/models 两个扇出共用 30s:
/// 控制面 GET 正常秒级,30s 足以兜住上游抖动,又不至于把管理页面挂死。
/// 纯本地的 models/local 扇出不在此列 —— 它用 st.http(见 models_local_account 注释)。
fn control_plane_http() -> Result<reqwest::Client, reqwest::Error> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
}

/// 把 AccountRow 转为对外视图:extra 解析成对象并把含 token/secret/password
/// 的字段脱敏(保尾 4 位)。凭据只进不出——admin 页展示概要即可,完整值
/// 留在库里供 worker 用。
///
/// `memberships` = 该账号的成员边 `[(组名, 组内优先级)]`,`None` 则不吐 `groups` 字段
/// (单条增删改的响应不带,前端那边这些 mutation 只做 invalidate、不回写缓存)。
fn redacted_view(row: AccountRow, memberships: Option<&[(String, i64)]>) -> serde_json::Value {
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
    let mut view = serde_json::json!({
        "account_id": row.account_id,
        "group_name": row.group_name,
        "provider": row.provider,
        "max_concurrency": row.max_concurrency,
        "priority": priority,
        // 排队开关:extra 里没有即视作关闭(与 scheduler::queue_enabled 同口径)。
        "queue_enabled": extra.get("queue_enabled").and_then(|v| v.as_bool()).unwrap_or(false),
        // 模型白名单顶层回显(前端编辑框要能读到现值)。缺失/null = 不限,统一吐 null;
        // 键名不含 token/secret/password/key,不会被上面的脱敏改写。
        "model_allowlist": extra.get("model_allowlist").cloned().unwrap_or(serde_json::Value::Null),
        "disabled": row.disabled,
        "extra": extra,
        "created_at": row.created_at,
        // 累计成功/失败请求计数(监控用,非计费)。前端账号页展示"累计成功/失败"列。
        "success_count": row.success_count,
        "failure_count": row.failure_count,
    });
    // 成员边:决定"谁能用这个号 + 在那个组里排第几"。顶层 `priority`(=extra.priority)
    // 重构后只是导入种子,**调度不读**,别拿它当排序依据。
    if let Some(memberships) = memberships {
        let groups: Vec<serde_json::Value> = memberships
            .iter()
            .map(|(name, priority)| serde_json::json!({ "name": name, "priority": priority }))
            .collect();
        view["groups"] = serde_json::Value::Array(groups);
    }
    view
}

/// 账号的**归属**组必须存在(防"幽灵分组":typo 的组名会让账号不被任何 worker 加载,
/// groups 页也看不见它)。
///
/// 注意这里校验的是归属(`accounts.group_name` = 哪个 worker 独占管它的运行态),
/// 不是可见性 —— 后者由成员边决定,一个号可以同时是多个组的成员。
///
/// `Ok(())` = 放行(含空组名 = 未分组);`Err(resp)` = 已构造好的 400/500 响应。
fn require_real_group(st: &AdminState, group: &str) -> Result<(), axum::response::Response> {
    if group.is_empty() {
        return Ok(());
    }
    match st.store.list_groups() {
        Ok(rows) if rows.iter().any(|g| g.name == group) => Ok(()),
        Ok(_) => Err(api_error(StatusCode::BAD_REQUEST, "分组不存在")),
        Err(e) => Err(internal_error(e)),
    }
}

async fn list_accounts(State(st): State<AdminState>) -> axum::response::Response {
    // 账号与成员边必须同一快照:分两次查会返回"账号已建、边还没有"这种从未存在过的组合
    // (create_account 是原子建号+建边),而前端拿它当"无分组"告警和差集基线。
    match st.store.list_accounts_with_memberships() {
        Ok(rows) => Json(
            rows.into_iter()
                .map(|(row, edges)| redacted_view(row, Some(&edges)))
                .collect::<Vec<_>>(),
        )
        .into_response(),
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
        if let Some(url) = EgressPicker::build(&st.store, body.egress.as_deref()).next() {
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
            Ok(Some(row)) => (StatusCode::CREATED, Json(redacted_view(row, None))).into_response(),
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
    let proxy: Option<String> = EgressPicker::build(&st.store, body.egress.as_deref()).next();
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
                Ok(Some(row)) => (StatusCode::CREATED, Json(redacted_view(row, None))).into_response(),
                Ok(None) => internal_error("上号后读取不到账号"),
                Err(e) => internal_error(e),
            }
        }
        Ok(false) => api_error(StatusCode::CONFLICT, "account_id 已存在"),
        Err(e) => internal_error(e),
    }
}

// ── Cursor 官方登录流(PKCE + 轮询)────────────────────────────────────────────
//
// 与 dario 的 OAuth 上号是同一个形状(start 给链接 → 人肉授权 → 换凭据落库),
// 但有两处本质差异,决定了不能复用那套代码:
//
// **① 轮询是多次的。** dario 的 `complete` 拿 code 一次换完,`take_pending` 单次
// 取出即删。Cursor 是操作员点授权前上游一直回 404,前端要反复 poll —— 所以这里
// 用 `peek`(读而不删)+ 成功落库后才 `remove`。
//
// **② 不扇给 worker。** dario 必须由目标 worker 换码(它的 token 与铸造出口 IP 绑定);
// Cursor 的 `/auth/poll` 是普通 HTTPS,router 直接调即可。但**出口纪律照旧**:
// 轮询走的 proxy 必须与该号将来 refresh/chat 的 proxy 是同一个值,否则铸/发 IP 不一致
// (见 §7 防关联)。所以 start 就把 `EgressPicker` 选定的 proxy 冻在会话里,
// poll 与落库都用它。

/// 待完成的 Cursor 登录会话(进程内存,**绝不落库、绝不进日志**)。
struct PendingCursorLogin {
    /// PKCE verifier —— 秘密,只发给上游轮询端点,不回给前端。
    flow: gw_cursor::login::LoginFlow,
    /// 冻结的出口(None=直连)。轮询与落库同一个值 → 铸=刷=发同 IP。
    proxy: Option<String>,
    account_id: String,
    group: String,
    max_concurrency: i64,
    priority: i64,
    created_at: std::time::Instant,
}

/// 登录窗口。比 dario 的 10 分钟略长:Cursor 登录可能要过邮箱验证码/2FA。
const CURSOR_LOGIN_TTL_SECS: u64 = 900;
const CURSOR_LOGIN_MAX: usize = 64;

fn cursor_pending() -> &'static std::sync::Mutex<
    std::collections::HashMap<String, PendingCursorLogin>,
> {
    static S: std::sync::OnceLock<
        std::sync::Mutex<std::collections::HashMap<String, PendingCursorLogin>>,
    > = std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

fn cursor_sweep(m: &mut std::collections::HashMap<String, PendingCursorLogin>) {
    m.retain(|_, v| v.created_at.elapsed().as_secs() < CURSOR_LOGIN_TTL_SECS);
}

/// 腾出一个位置:先清过期,仍达上限则淘汰最旧。
///
/// 抽成对**任意 map** 生效的函数(而不是直接操作全局表),是为了能在单测里
/// 用自己的 map 验上限行为 —— 全局表是进程级 `OnceLock`,在里面灌 64 个会话
/// 会把并发跑的其它测试的会话挤掉,那种测试互相打架排查起来极其费时(踩过)。
fn cursor_make_room(m: &mut std::collections::HashMap<String, PendingCursorLogin>, cap: usize) {
    cursor_sweep(m);
    while m.len() >= cap {
        let Some(oldest) = m.iter().min_by_key(|(_, v)| v.created_at).map(|(k, _)| k.clone()) else {
            break;
        };
        m.remove(&oldest);
    }
}

#[derive(Debug, Deserialize)]
pub struct CursorLoginStartBody {
    account_id: String,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    max_concurrency: Option<i64>,
    /// 缺省 100(低优先)。cursor 是订阅号,没有 Kiro 那种档位概念。
    #[serde(default)]
    priority: Option<i64>,
    /// ""/缺省/"direct"=直连;"auto"=自动均衡;数字=egress_pool 索引。
    #[serde(default)]
    egress: Option<String>,
}

/// `POST /accounts/cursor/login/start` —— 生成登录链接。**不发任何上游请求。**
async fn cursor_login_start(
    State(st): State<AdminState>,
    Json(body): Json<CursorLoginStartBody>,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&body.account_id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    // 提前挡重名:别让操作员在浏览器登完了才撞 409(照 dario 的教训)。
    match st.store.get_account(&body.account_id) {
        Ok(Some(_)) => return api_error(StatusCode::CONFLICT, "account_id 已存在"),
        Ok(None) => {}
        Err(e) => return internal_error(e),
    }
    let group = body.group.as_deref().unwrap_or("").to_string();
    if let Err(resp) = require_real_group(&st, &group) {
        return resp;
    }

    let proxy: Option<String> = EgressPicker::build(&st.store, body.egress.as_deref()).next();
    let flow = gw_cursor::login::start();
    let login_url = flow.login_url.clone();
    let flow_id = flow.uuid.clone();
    {
        let mut m = cursor_pending().lock().unwrap_or_else(|e| e.into_inner());
        cursor_make_room(&mut m, CURSOR_LOGIN_MAX);
        m.insert(
            flow_id.clone(),
            PendingCursorLogin {
                flow,
                proxy,
                account_id: body.account_id,
                group,
                max_concurrency: body.max_concurrency.unwrap_or(2),
                priority: body.priority.unwrap_or(100),
                created_at: std::time::Instant::now(),
            },
        );
    }
    // ⚠️ 只回 login_url 与 flow_id,**绝不回 verifier**(那是 PKCE 的秘密)。
    Json(serde_json::json!({
        "login_url": login_url,
        "flow_id": flow_id,
        "expires_in_sec": CURSOR_LOGIN_TTL_SECS,
        "poll_interval_sec": gw_cursor::login::POLL_INTERVAL_SECS,
    }))
    .into_response()
}

#[derive(Debug, Deserialize)]
pub struct CursorLoginPollBody {
    flow_id: String,
}

/// `POST /accounts/cursor/login/poll` —— 问一次「授权好了吗」。
///
/// 前端按 `poll_interval_sec` 反复调。三种回应:
///   `{"status":"pending"}`  还没授权,继续问
///   `{"status":"done", ...账号行}`  已落库(201)
///   4xx/5xx  终态失败(会话已清)
async fn cursor_login_poll(
    State(st): State<AdminState>,
    Json(body): Json<CursorLoginPollBody>,
) -> axum::response::Response {
    // 读而不删:未授权时会话必须留着给下一次 poll。
    let (flow, proxy) = {
        let mut m = cursor_pending().lock().unwrap_or_else(|e| e.into_inner());
        cursor_sweep(&mut m);
        match m.get(&body.flow_id) {
            Some(p) => (p.flow.clone(), p.proxy.clone()),
            None => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "登录会话已过期或无效(超过 15 分钟/已完成/router 重启过),请重新发起",
                )
            }
        }
    };

    // 轮询必须走该号冻结的出口 —— 与将来 refresh/chat 同 IP。
    let client = match build_login_client(proxy.as_deref()) {
        Ok(c) => c,
        Err(e) => return internal_error(e),
    };
    let outcome = match gw_cursor::login::poll_once(&client, &flow).await {
        Ok(o) => o,
        Err(e) => {
            // 上游明确拒绝(如企业策略)是终态,清掉会话免得前端一直问。
            if e.kind == gw_core::error::UpstreamErrorKind::TokenInvalid {
                let mut m = cursor_pending().lock().unwrap_or_else(|x| x.into_inner());
                m.remove(&body.flow_id);
                return api_error(StatusCode::BAD_REQUEST, &e.message);
            }
            // 网络抖动等瞬时错误:保留会话,让前端继续轮询。
            return api_error(StatusCode::BAD_GATEWAY, &e.message);
        }
    };

    let (access_token, refresh_token) = match outcome {
        gw_cursor::login::PollOutcome::Pending => {
            return Json(serde_json::json!({"status": "pending"})).into_response()
        }
        gw_cursor::login::PollOutcome::Done {
            access_token,
            refresh_token,
            ..
        } => (access_token, refresh_token),
    };

    // 拿到凭据了 —— 从这里起会话用完即弃(无论落库成败,凭据已在手,重试 poll 也拿不到第二次)。
    let Some(pending) = ({
        let mut m = cursor_pending().lock().unwrap_or_else(|e| e.into_inner());
        m.remove(&body.flow_id)
    }) else {
        return api_error(StatusCode::BAD_REQUEST, "登录会话已被并发完成");
    };

    let mut extra = serde_json::Map::new();
    extra.insert("access_token".into(), serde_json::json!(access_token));
    extra.insert("refresh_token".into(), serde_json::json!(refresh_token));
    extra.insert("priority".into(), serde_json::json!(pending.priority));
    if let Some(p) = &pending.proxy {
        extra.insert("proxy".into(), serde_json::json!(p));
    }
    // 记下 token 过期时刻,让 has_fresh_token 能主动续期而不是等 401(见 gw_cursor::auth)。
    if let Some(exp) = gw_cursor::auth::token_expires_at(&access_token) {
        extra.insert(
            "expires_at".into(),
            serde_json::json!(gw_cursor::auth::format_unix_utc(exp)),
        );
    }
    let extra_json = match serde_json::to_string(&extra) {
        Ok(s) => s,
        Err(e) => return internal_error(e),
    };
    match st.store.create_account(
        &pending.account_id,
        &pending.group,
        "cursor",
        pending.max_concurrency,
        &extra_json,
    ) {
        Ok(true) => {
            // 主动 /sync 目标组,消除「上号后 30s 内按号操作报无人持有」的窗口。
            for w in st.workers.iter().filter(|w| w.account_group == pending.group) {
                let url = format!("http://{}/sync", w.listen);
                let _ = st.http.post(&url).send().await;
            }
            match st.store.get_account(&pending.account_id) {
                Ok(Some(row)) => {
                    let mut v = redacted_view(row, None);
                    if let Some(o) = v.as_object_mut() {
                        o.insert("status".into(), serde_json::json!("done"));
                    }
                    (StatusCode::CREATED, Json(v)).into_response()
                }
                Ok(None) => internal_error("上号后读取不到账号"),
                Err(e) => internal_error(e),
            }
        }
        Ok(false) => api_error(StatusCode::CONFLICT, "account_id 已存在"),
        Err(e) => internal_error(e),
    }
}

/// 轮询用的 HTTP client:绑定该号冻结的出口。
///
/// 不用 `st.http` —— 那是 2 秒超时的管理面客户端,而轮询要走代理打境外站点
/// (`quota_account` 就是踩了这个坑,超时被误判成「worker 离线」)。
fn build_login_client(proxy: Option<&str>) -> anyhow::Result<reqwest::Client> {
    let mut b = reqwest::Client::builder().timeout(std::time::Duration::from_secs(20));
    if let Some(url) = proxy.map(str::trim).filter(|s| !s.is_empty()) {
        b = b.proxy(reqwest::Proxy::all(url)?);
    }
    Ok(b.build()?)
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
    let mut egress_picker = EgressPicker::build(&st.store, body.egress.as_deref());

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
    let pool = read_egress_pool(&st.store);
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
    // ⚠️ `disabled=false`(启用)必须**剔除出通用 patch**:启用与「清 suspend 生命周期 +
    // epoch 递增」是 restore_account 的单事务,拆两次写会留下"已启用但旧退避还在"
    // 的可持久化半套状态(对抗审查二轮阻断#2)。disabled=true(停用)无此耦合,照走 patch。
    let restore_requested = body.disabled == Some(false);
    let patch_disabled = body.disabled.filter(|d| *d);
    let has_patch = body.group_name.is_some()
        || body.max_concurrency.is_some()
        || patch_disabled.is_some()
        || extra.is_some();
    if has_patch {
        let patch = AccountPatch {
            group_name: body.group_name.clone(),
            max_concurrency: body.max_concurrency,
            disabled: patch_disabled,
            extra,
        };
        match st.store.update_account(&id, &patch) {
            Ok(UpdateAccountOutcome::Ok) => {}
            Ok(UpdateAccountOutcome::NotFound) => {
                return api_error(StatusCode::NOT_FOUND, "账号不存在")
            }
            // 改归属会让某个组同时有两个 owner —— 与建边时的 CrossOwner 是同一条不变量,
            // 只是从边的另一头被破坏。整单拒绝,不做部分应用。
            Ok(UpdateAccountOutcome::CrossOwner { group, existing, incoming }) => {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    &format!(
                        "改归属会让分组 {group} 同时属于两个 owner({existing} 与 {incoming})\
                         ——组内优先级将不再是全局排序。请先把该号移出 {group},或连同该组其余成员一起迁移。"
                    ),
                )
            }
            Err(e) => return internal_error(e),
        }
    }
    // 人工启用 = 原子恢复:disabled=0 + 清 suspend 生命周期 + epoch 递增,单事务
    // (restore_account 是唯一执行启用写入的地方,见上)。对没有生命周期行的普通账号
    // 这是无害 no-op(仅多写一行清零行,worker 对账后自然收敛)。
    if restore_requested {
        match st.store.restore_account(&id) {
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
    // 定点开关排队(同上:增量 merge,绝不碰凭据)。
    if let Some(q) = body.queue_enabled {
        let delta = serde_json::json!({ "queue_enabled": q }).to_string();
        match st.store.merge_account_extra(&id, &delta) {
            Ok(true) => {}
            Ok(false) => return api_error(StatusCode::NOT_FOUND, "账号不存在"),
            Err(e) => return internal_error(e),
        }
    }
    // 定点模型白名单合并(同 proxy_url:增量 merge,绝不碰凭据)。
    // 空串/纯分隔符 = 清除(写 null;merge_account_extra 是读-合-写,写 null 不删键,
    // 读侧 gw_core::account::model_allowlist_allows 把 null 与缺失同义为「不限」)。
    // 非空 = 写侧校验后规范化成 JSON 字符串数组;非法条目直接 400(fail-closed,
    // 绝不静默收下语义存疑的白名单 —— 交接规格第 4/5 条)。
    if let Some(raw) = &body.model_allowlist {
        let allow_val = match normalize_model_allowlist(raw) {
            Ok(v) => v,
            Err(msg) => return api_error(StatusCode::BAD_REQUEST, &msg),
        };
        let delta = serde_json::json!({ "model_allowlist": allow_val }).to_string();
        match st.store.merge_account_extra(&id, &delta) {
            Ok(true) => {}
            Ok(false) => return api_error(StatusCode::NOT_FOUND, "账号不存在"),
            Err(e) => return internal_error(e),
        }
    }
    // 落库后 best-effort 捅所有 worker 立即同步(同 delete_account/import 的理由):
    // 否则启用/禁用/换组等改动要等 worker 自己最多 30s 的周期 sync 才生效,期间按号操作
    // (如导入对话框"编辑后立即验活")会误报"没有 worker 持有该账号"。
    if has_patch
        || restore_requested
        || body.proxy_url.is_some()
        || body.priority.is_some()
        || body.queue_enabled.is_some()
        || body.model_allowlist.is_some()
    {
        poke_workers_sync(&st).await;
    }
    match st.store.get_account(&id) {
        Ok(Some(row)) => Json(redacted_view(row, None)).into_response(),
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
pub(super) async fn poke_workers_sync(st: &AdminState) {
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
                        // 排队实况:面板要显示"当前排队 N / 容量 M"。
                        "queue": v.get("queue").cloned().unwrap_or(serde_json::Value::Null),
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
    // 长超时 client:worker 侧要真打上游 getUsageLimits,2s 的 st.http 会把慢响应
    // 误判成 worker 离线(见 control_plane_http 注释)。
    let http = match control_plane_http() {
        Ok(c) => c,
        Err(e) => return internal_error(anyhow::anyhow!("扇出 http 客户端构造失败: {e}")),
    };
    let mut first_error: Option<(u16, String)> = None;
    for w in st.workers.iter() {
        let url = format!("http://{}/accounts/{}/quota", w.listen, id);
        let resp = match http.post(&url).send().await {
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

/// `POST /accounts/{id}/on-demand` —— 设置该号的超额(on-demand)额度上限。
///
/// body: `{"limit_usd": 50}`(美元整数);`0`/`null` = 关闭超额。目前只有 Cursor 支持。
///
/// ⚠️ **写**操作:改的是上游账号的计费设置,开启后套餐用尽会产生真实费用。
/// 与 quota 同款顺序扇出:2xx 立即返回,404 问下一个,其余记首个错误后继续
/// (错误原文要能透出,比如未绑支付方式时上游的「Payment method required」)。
async fn on_demand_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    body: Option<Json<serde_json::Value>>,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    // 长超时 client:worker 侧要真打上游 SetHardLimit + 回读,2s 会误判成 worker 离线。
    let http = match control_plane_http() {
        Ok(c) => c,
        Err(e) => return internal_error(anyhow::anyhow!("扇出 http 客户端构造失败: {e}")),
    };
    let payload = body.map(|Json(v)| v).unwrap_or(serde_json::Value::Null);
    let mut first_error: Option<(u16, String)> = None;
    for w in st.workers.iter() {
        let url = format!("http://{}/accounts/{}/on-demand", w.listen, id);
        let resp = match http.post(&url).json(&payload).send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!(instance = w.instance, "on-demand 扇出失败(worker 离线?): {e}");
                continue;
            }
        };
        let status = resp.status().as_u16();
        if status == 404 {
            continue; // 该 worker 不持有此账号。
        }
        let rbody = resp.json::<serde_json::Value>().await.ok();
        if (200..300).contains(&status) {
            return Json(
                rbody.unwrap_or_else(|| serde_json::json!({"ok": true, "account_id": id})),
            )
            .into_response();
        }
        if first_error.is_none() {
            let msg = rbody
                .as_ref()
                .and_then(|b| b.pointer("/error/message"))
                .and_then(|m| m.as_str())
                .unwrap_or("超额额度设置失败")
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

/// `POST /accounts/{id}/models` —— 用该账号拉一次上游模型目录并落库。
///
/// 拿 `rateMultiplier` 定价、拿逐模型的 thinking 档位表。**只读**,不发 chat。
/// 与 quota 同款顺序扇出:2xx 立即返回,404 问下一个,其余记首个错误后继续。
/// **人工探针**扇出:钉住指定账号真发一次最小 chat,看上游到底出不出词。
///
/// `POST /accounts/{id}/probe?model=claude-opus-5`(model 缺省 `claude-haiku-4.5`)。
///
/// 为什么必须有它:`/quota` 只证明控制面凭据活着,`/models` 只证明目录里有这个模型 ——
/// **两者都不代表数据面能出词**。实测存在「有额度、目录有 opus,一发 chat 恒
/// `ModelNotAvailable`」的号。判定一个停用号能不能复活,只有真收到 delta 才算数。
///
/// ⚠️ 调用方**必须串行 + 限速**:短时高频 chat 验号历史上直接导致 `TEMPORARILY_SUSPENDED`
/// (见 memory caio-kiro-key-suspend-lesson)。本端点单次 `max_tokens=16`、收到首个文本
/// delta 即断流,但**批量调用的节奏由调用方负责**,服务端不替你兜底。
async fn probe_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    let qs = query.filter(|q| !q.is_empty()).map(|q| format!("?{q}")).unwrap_or_default();
    // ⚠️ **不能用 `st.http`**:它是 2s 超时的管理面客户端(为的是 worker 离线时快速跳过)。
    // 探针要真发一次 chat,opus 首字节就可能十几秒 —— 用 2s 客户端必然超时,而超时在扇出
    // 循环里等价于"该 worker 离线",最终误报成「没有 worker 持有该账号」。踩过一次,别改回去。
    let probe_http = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(120))
        .build()
    {
        Ok(c) => c,
        Err(e) => return internal_error(anyhow::anyhow!("探针 http 客户端构造失败: {e}")),
    };
    let mut first_error: Option<(u16, String)> = None;
    for w in st.workers.iter() {
        let url = format!("http://{}/accounts/{}/probe{}", w.listen, id, qs);
        let resp = match probe_http.post(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!(instance = w.instance, "probe 扇出失败(worker 离线?): {e}");
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
                body.unwrap_or_else(|| serde_json::json!({"replied": false, "account_id": id})),
            )
            .into_response();
        }
        if first_error.is_none() {
            first_error = Some((status, "探针失败".to_string()));
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

async fn models_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    // 长超时 client:worker 侧要真打上游拉目录,2s 的 st.http 会把慢响应误判成
    // worker 离线(见 control_plane_http 注释)。
    let http = match control_plane_http() {
        Ok(c) => c,
        Err(e) => return internal_error(anyhow::anyhow!("扇出 http 客户端构造失败: {e}")),
    };
    let mut first_error: Option<(u16, String)> = None;
    for w in st.workers.iter() {
        let url = format!("http://{}/accounts/{}/models", w.listen, id);
        let resp = match http.post(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!(instance = w.instance, "models 扇出失败(worker 离线?): {e}");
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
                body.unwrap_or_else(|| serde_json::json!({"fetched": false, "account_id": id})),
            )
            .into_response();
        }
        if first_error.is_none() {
            let msg = body
                .as_ref()
                .and_then(|b| b.pointer("/error/message"))
                .and_then(|m| m.as_str())
                .unwrap_or("模型目录查询失败")
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

/// `GET /accounts/{id}/models/local` —— 账号可用模型清单(**纯本地**,worker 侧零上游)。
///
/// 与 `models_account` 同款顺序扇出(2xx 立即返回,404 问下一个,其余记首个错误后继续),
/// 区别在 worker 侧只读内存(静态目录 + 档位支持判断 + 已学模型标记),毫秒级返回,
/// 所以面板「查看模型」按钮可以随便点。
///
/// ⚠️ 这里**故意用 2s 的 `st.http`**,不用 control_plane_http:worker 侧纯本地、毫秒级,
/// 2s 不响应就说明它真的挂了/事件循环卡死 —— 串行扇出若给 30s,一个挂住的 worker 会把
/// 整个面板请求拖 30s。30s 长超时只留给真打上游的 quota/models 扇出(审查 gpt-5.6-sol)。
async fn models_local_account(
    State(st): State<AdminState>,
    Path(id): Path<String>,
) -> axum::response::Response {
    if let Err(msg) = validate_account_id(&id) {
        return api_error(StatusCode::BAD_REQUEST, msg);
    }
    let mut first_error: Option<(u16, String)> = None;
    for w in st.workers.iter() {
        let url = format!("http://{}/accounts/{}/models/local", w.listen, id);
        let resp = match st.http.get(&url).send().await {
            Ok(resp) => resp,
            Err(e) => {
                tracing::debug!(instance = w.instance, "models/local 扇出失败(worker 离线?): {e}");
                continue;
            }
        };
        let status = resp.status().as_u16();
        if status == 404 {
            continue; // 该 worker 不持有此账号。
        }
        let body = resp.json::<serde_json::Value>().await.ok();
        if (200..300).contains(&status) {
            return Json(body.unwrap_or_else(|| serde_json::json!({"account_id": id})))
                .into_response();
        }
        if first_error.is_none() {
            let msg = body
                .as_ref()
                .and_then(|b| b.pointer("/error/message"))
                .and_then(|m| m.as_str())
                .unwrap_or("模型清单查询失败")
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

/// `GET /models/catalog` —— 读最近一次落库的模型目录(不打上游),并报出**档位漂移**。
///
/// 从未抓过 → `{"catalog": null}` 而非 404:调用方靠这个字段判断"还没拉过",
/// 不必把"没数据"和"路由不存在"混在同一个状态码里。
///
/// `effort_drift` 是关键增值:热路径的档位表是**编译期常量**,而上游随时会增删档位。
/// 目录本身只是快照,不参与发包;这个字段让"我们会发出上游不认的档位"这件事在**发生前**
/// 就能看见,而不是等线上报 400 才发现。非空 = 该改代码里的静态表了。
async fn model_catalog(State(st): State<AdminState>) -> axum::response::Response {
    let json = match st.store.get_kv(gw_store::SqliteStore::KEY_MODEL_CATALOG) {
        Ok(Some(j)) => j,
        Ok(None) => {
            return Json(serde_json::json!({"catalog": serde_json::Value::Null})).into_response()
        }
        Err(e) => return api_error(StatusCode::INTERNAL_SERVER_ERROR, &format!("读取失败: {e}")),
    };
    let value = match serde_json::from_str::<serde_json::Value>(&json) {
        Ok(v) => v,
        // 库里存着但解析不了(手改过 / 旧格式):当没有,别 500 掉整个面板。
        Err(e) => {
            tracing::warn!("模型目录解析失败,按未抓取处理: {e}");
            return Json(serde_json::json!({"catalog": serde_json::Value::Null})).into_response();
        }
    };
    let drift = gw_kiro::converter::effort_drift(&extract_effort_triples(&value));
    Json(serde_json::json!({"catalog": value, "effort_drift": drift})).into_response()
}

/// 从落库的目录 JSON 里抽出 `(modelId, effortLevels, defaultEffortLevel)` 三元组。
/// 形状对不上的条目直接跳过 —— 漂移检测是辅助信号,不该因为一条脏数据就报错。
fn extract_effort_triples(catalog: &serde_json::Value) -> Vec<(String, Vec<String>, Option<String>)> {
    catalog
        .get("models")
        .and_then(|m| m.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let id = m.get("model_id")?.as_str()?.to_string();
                    let levels = m
                        .get("effort_levels")
                        .and_then(|l| l.as_array())
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                        .unwrap_or_default();
                    let default = m
                        .get("default_effort_level")
                        .and_then(|d| d.as_str())
                        .map(str::to_string);
                    Some((id, levels, default))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::normalize_model_allowlist;
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

    /// 账号列表必须带上**成员边** —— 前端账号页靠它显示"这个号在哪几个组、各排第几",
    /// 也是编辑弹窗做差集的基线。少了它就回到 2026-07-29 那种局面:号看着可用,
    /// 实际只挂在一个没有流量的组里,白放一天没人发现。
    #[tokio::test]
    async fn list_accounts_carries_memberships_in_stable_order() {
        let (app, store) = app();
        for g in ["G0", "ZED", "GLOW"] {
            store.create_group(g, "", "").unwrap();
        }
        store.create_account("kiro-01", "G0", "kiro", 2, "{}").unwrap();
        // 反字典序写入,证明返回顺序来自排序而非插入顺序。
        store.upsert_membership("kiro-01", "ZED", 100).unwrap();
        store.upsert_membership("kiro-01", "GLOW", 0).unwrap();

        let v = json_body(app.clone().oneshot(req("GET", "/accounts", None)).await.unwrap()).await;
        let row = &v.as_array().unwrap()[0];
        assert_eq!(
            row["groups"],
            serde_json::json!([
                { "name": "G0", "priority": 100 },
                { "name": "GLOW", "priority": 0 },
                { "name": "ZED", "priority": 100 },
            ]),
            "组名升序 + 组内优先级逐条对上"
        );

        // 反向断言:删掉一条边,那个组不得再出现。
        assert!(store.remove_membership("kiro-01", "GLOW").unwrap());
        let v = json_body(app.clone().oneshot(req("GET", "/accounts", None)).await.unwrap()).await;
        let groups = v[0]["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        assert!(
            !groups.iter().any(|g| g["name"] == "GLOW"),
            "已删的成员边不得继续出现在列表里"
        );

        // 顶层 priority(=extra.priority)仍在,但只是导入种子;它与成员边的优先级
        // **不是一回事**,前端不得拿它当排序依据。
        assert_eq!(v[0]["priority"], 100);
    }

    #[tokio::test]
    async fn account_crud_roundtrip_with_redaction() {
        let (app, store) = app();
        store.create_group("G0", "", "").unwrap();
        store.create_group("G1", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
    async fn update_model_allowlist_normalizes_clears_and_rejects() {
        let (app, store) = app();
        store.create_group("G0", "", "").unwrap();
        store
            .create_account(
                "acc1",
                "G0",
                "cursor",
                2,
                r#"{"access_token":"tok-secret"}"#,
            )
            .unwrap();
        // 设置:CSV 输入 → 规范化成小写 JSON 字符串数组落库;凭据原样保留;顶层回显。
        let body =
            serde_json::json!({"model_allowlist": "Default, composer* ;GROK*"}).to_string();
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/accounts/acc1", Some(&body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let view: serde_json::Value = json_body(resp).await;
        assert_eq!(
            view["model_allowlist"],
            serde_json::json!(["default", "composer*", "grok*"]),
            "顶层应回显规范化后的数组: {view}"
        );
        let row = store.get_account("acc1").unwrap().unwrap();
        assert!(
            row.extra
                .contains(r#""model_allowlist":["default","composer*","grok*"]"#),
            "规范存储必须是小写 JSON 数组: {}",
            row.extra
        );
        assert!(row.extra.contains("tok-secret"), "凭据不得被定点合并冲掉");

        // 非法条目整单 400,库内值不动(fail-closed,不做部分应用)。
        for bad in ["*", "gr*k", "*grok", "grok 4.5!"] {
            let body = serde_json::json!({ "model_allowlist": bad }).to_string();
            let resp = app
                .clone()
                .oneshot(req("PATCH", "/accounts/acc1", Some(&body)))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "{bad:?} 应被拒绝");
        }
        let row = store.get_account("acc1").unwrap().unwrap();
        assert!(
            row.extra.contains(r#""model_allowlist":["default","composer*","grok*"]"#),
            "非法输入被拒后库内值必须不动: {}",
            row.extra
        );

        // 清除:空串 → 写 null(读侧与缺失同义 = 不限),凭据仍在。
        let body = serde_json::json!({"model_allowlist": ""}).to_string();
        let resp = app
            .clone()
            .oneshot(req("PATCH", "/accounts/acc1", Some(&body)))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let row = store.get_account("acc1").unwrap().unwrap();
        assert!(
            row.extra.contains(r#""model_allowlist":null"#),
            "清除应写 model_allowlist:null: {}",
            row.extra
        );
        assert!(row.extra.contains("tok-secret"), "清除白名单不得动凭据");
    }

    /// 写侧校验函数的单元口径(与 PATCH 集成测试互补:这里穷举纯函数分支)。
    #[test]
    fn normalize_model_allowlist_rules() {
        // 空 / 纯分隔符 → null(清除)。
        assert_eq!(normalize_model_allowlist("").unwrap(), serde_json::Value::Null);
        assert_eq!(normalize_model_allowlist("  , ; ").unwrap(), serde_json::Value::Null);
        // 规范化:小写、去空白、逗号/分号/空白混合分隔都收。
        assert_eq!(
            normalize_model_allowlist("Default composer*,GROK-4.5;claude-*").unwrap(),
            serde_json::json!(["default", "composer*", "grok-4.5", "claude-*"])
        );
        // 裸 `*`、通配符不在末尾、非法字符 → Err。
        assert!(normalize_model_allowlist("*").is_err());
        assert!(normalize_model_allowlist("default,*").is_err());
        assert!(normalize_model_allowlist("gr*k").is_err());
        assert!(normalize_model_allowlist("*grok").is_err());
        assert!(normalize_model_allowlist("grok!").is_err());
        // 非法条目不许部分应用:一个坏条目拖垮整单。
        assert!(normalize_model_allowlist("default,gr*k,grok*").is_err());
    }

    #[tokio::test]
    async fn update_priority_merges_without_touching_credentials() {
        let (app, store) = app();
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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

    // === GET /accounts/{id}/models/local 扇出(账号可用模型清单,纯本地)===

    /// 假 worker(真 TCP 端口):GET /accounts/{id}/models/local 按指定状态码 + body 应答,
    /// `delay` 可模拟慢响应。
    async fn spawn_models_local_worker(
        status: u16,
        body: serde_json::Value,
        delay: std::time::Duration,
    ) -> gw_core::config::WorkerConfig {
        use axum::routing::get;
        use axum::Json;
        let router = axum::Router::new().route(
            "/accounts/{id}/models/local",
            get(move || {
                let body = body.clone();
                async move {
                    if !delay.is_zero() {
                        tokio::time::sleep(delay).await;
                    }
                    (axum::http::StatusCode::from_u16(status).unwrap(), Json(body))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        gw_core::config::WorkerConfig {
            instance: 0,
            listen: addr.to_string(),
            egress: gw_core::config::EgressConfig::Direct,
            account_group: "".to_string(),
        }
    }

    async fn get_models_local(app: &axum::Router, id: &str) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(req("GET", &format!("/accounts/{id}/models/local"), None))
            .await
            .unwrap();
        let status = resp.status();
        (status, json_body(resp).await)
    }

    /// 持有方 200 → 原样透出(账号的模型清单 + 目录外标记)。
    #[tokio::test]
    async fn models_local_fanout_returns_holder_response() {
        let payload = serde_json::json!({
            "account_id": "acc-1",
            "models": [
                {"id": "claude-opus-5", "display_name": "Opus 5", "supported": true,
                 "mark_remaining_secs": null, "available": true},
                {"id": "claude-haiku-4.5", "display_name": "Haiku 4.5", "supported": true,
                 "mark_remaining_secs": 1200, "available": false},
            ],
            "off_catalog_marks": [{"model": "claude-weird-9", "remaining_secs": 300}],
        });
        let w = spawn_models_local_worker(200, payload, std::time::Duration::ZERO).await;
        let (app, _store) = app_with_workers(vec![w]);
        let (status, v) = get_models_local(&app, "acc-1").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(v["models"].as_array().unwrap().len(), 2, "清单应原样透出");
        assert_eq!(v["off_catalog_marks"][0]["model"], "claude-weird-9");
    }

    /// 404 = 不持有 → 问下一个 worker,命中后透出其应答。
    #[tokio::test]
    async fn models_local_fanout_404_falls_through_to_next_worker() {
        let w1 = spawn_models_local_worker(
            404,
            serde_json::json!({"account_id": "acc-2"}),
            std::time::Duration::ZERO,
        )
        .await;
        let w2 = spawn_models_local_worker(
            200,
            serde_json::json!({"account_id": "acc-2", "models": [], "off_catalog_marks": []}),
            std::time::Duration::ZERO,
        )
        .await;
        let (app, _store) = app_with_workers(vec![w1, w2]);
        let (status, v) = get_models_local(&app, "acc-2").await;
        assert_eq!(status, StatusCode::OK, "第一个 404 应续问第二个");
        assert_eq!(v["account_id"], "acc-2");
    }

    /// 所有 worker 都不持有(或没有 worker)→ 404「没有 worker 持有该账号」。
    #[tokio::test]
    async fn models_local_fanout_no_holder_is_404() {
        let w = spawn_models_local_worker(
            404,
            serde_json::json!({"account_id": "acc-3"}),
            std::time::Duration::ZERO,
        )
        .await;
        let (app, _store) = app_with_workers(vec![w]);
        let (status, v) = get_models_local(&app, "acc-3").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(
            v.to_string().contains("没有 worker 持有该账号"),
            "应报无人持有: {v}"
        );
    }

    /// 持有方报错(如 500)→ 透出首个错误,而不是误报「无人持有」。
    #[tokio::test]
    async fn models_local_fanout_surfaces_first_error() {
        let w = spawn_models_local_worker(
            500,
            serde_json::json!({"error": {"message": "目录读取炸了"}}),
            std::time::Duration::ZERO,
        )
        .await;
        let (app, _store) = app_with_workers(vec![w]);
        let (status, v) = get_models_local(&app, "acc-4").await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(v.to_string().contains("目录读取炸了"), "错误应透出: {v}");
    }

    /// ⚠️ 回归:真打上游的扇出(quota/models)必须用长超时 client(30s,见
    /// control_plane_http)。worker 应答 > 2s 时,旧的 st.http(2s 管理面 client)会把
    /// 传输超时当成「worker 离线」,扫完误报 404「没有 worker 持有该账号」——直连同一
    /// worker 却是 200。这条用 3s 慢应答的 quota 假 worker 锁死:> 旧超时、< 新超时,
    /// 必须拿到 200。(纯本地的 models/local 扇出相反:它就该用 2s 的 st.http,
    /// 见 models_local_account 注释,故慢应答回归不打在它上面。)
    #[tokio::test]
    async fn quota_fanout_tolerates_slow_worker_beyond_2s() {
        use axum::routing::post;
        use axum::Json;
        let router = axum::Router::new().route(
            "/accounts/{id}/quota",
            post(|| async {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                Json(serde_json::json!({"verified": true, "account_id": "acc-5"}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        let w = gw_core::config::WorkerConfig {
            instance: 0,
            listen: addr.to_string(),
            egress: gw_core::config::EgressConfig::Direct,
            account_group: "".to_string(),
        };
        let (app, _store) = app_with_workers(vec![w]);
        let resp = app
            .oneshot(req("POST", "/accounts/acc-5/quota", None))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::OK,
            "3s 慢应答不得被误判成 worker 离线(旧 2s client 会)"
        );
    }

    #[tokio::test]
    async fn import_smart_merge_backfills_machine_id_keeps_server_token() {
        let (app, store) = app();
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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

    // ── Cursor 官方登录流 ───────────────────────────────────────────────────

    async fn cursor_start(
        app: &axum::Router,
        account_id: &str,
        group: &str,
    ) -> (StatusCode, serde_json::Value) {
        let body = serde_json::json!({ "account_id": account_id, "group": group }).to_string();
        let resp = app
            .clone()
            .oneshot(req("POST", "/accounts/cursor/login/start", Some(&body)))
            .await
            .unwrap();
        let status = resp.status();
        (status, json_body(resp).await)
    }

    async fn cursor_poll(app: &axum::Router, flow_id: &str) -> (StatusCode, serde_json::Value) {
        let body = serde_json::json!({ "flow_id": flow_id }).to_string();
        let resp = app
            .clone()
            .oneshot(req("POST", "/accounts/cursor/login/poll", Some(&body)))
            .await
            .unwrap();
        let status = resp.status();
        (status, json_body(resp).await)
    }

    /// start 是纯本地的:不探 worker、不发上游请求,所以空组也该成功 ——
    /// 与 dario 不同(那个必须由目标 worker 铸 token,故 start 就要求组里有 worker)。
    #[tokio::test]
    async fn cursor_login_start_returns_url_without_touching_upstream() {
        let (app, _store) = app_with_workers(vec![]);
        let (status, v) = cursor_start(&app, "cursor-a", "").await;
        assert_eq!(status, StatusCode::OK);
        let url = v["login_url"].as_str().unwrap();
        assert!(url.starts_with("https://cursor.com/loginDeepControl"));
        assert!(url.contains("challenge="));
        assert!(url.contains("mode=login"));
        assert!(!v["flow_id"].as_str().unwrap().is_empty());
        assert!(v["poll_interval_sec"].as_u64().unwrap() >= 1);
    }

    /// ⚠️ 安全红线:PKCE verifier **绝不能出现在响应里**(它一旦泄漏,PKCE 就白做了)。
    #[tokio::test]
    async fn cursor_login_start_never_leaks_verifier() {
        let (app, _store) = app_with_workers(vec![]);
        let (_s, v) = cursor_start(&app, "cursor-b", "").await;
        let flow_id = v["flow_id"].as_str().unwrap().to_string();
        let verifier = {
            let m = crate::admin::accounts::cursor_pending().lock().unwrap();
            m.get(&flow_id).unwrap().flow.verifier.clone()
        };
        assert_eq!(verifier.len(), 43, "verifier 应是 base64url(32B)");
        let whole = serde_json::to_string(&v).unwrap();
        assert!(!whole.contains(&verifier), "响应体泄漏了 verifier");
        assert!(!whole.contains("verifier"), "响应里连字段名都不该有");
    }

    /// 重名要在**操作员打开浏览器之前**就挡掉(照 dario 的教训:别让人登完才撞 409)。
    #[tokio::test]
    async fn cursor_login_start_rejects_duplicate_before_browser() {
        let (app, store) = app_with_workers(vec![]);
        store
            .create_account("cursor-dup", "", "cursor", 2, "{}")
            .unwrap();
        let (status, _v) = cursor_start(&app, "cursor-dup", "").await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn cursor_login_start_validates_account_id() {
        let (app, _store) = app_with_workers(vec![]);
        let (status, _v) = cursor_start(&app, "bad id!", "").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    /// 未知/过期 flow_id → 400,且**不打上游**。
    #[tokio::test]
    async fn cursor_login_poll_rejects_unknown_flow() {
        let (app, _store) = app_with_workers(vec![]);
        let (status, v) = cursor_poll(&app, "no-such-flow").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(v.to_string().contains("过期") || v.to_string().contains("无效"));
    }

    /// 瞬时失败(网络抖动/上游不可达)**不能**清掉会话 —— 轮询天然要问很多次,
    /// 这是与 dario `take_pending`(单次取出即删)最关键的差异。
    ///
    /// 断言的是**行为契约**而非全局表内容:会话表是进程级 `OnceLock`,同一二进制里
    /// `..._sessions_are_bounded` 会灌满它并淘汰最旧项,直接查表会与那条打架(踩过)。
    /// 所以连 poll 两次,断言第二次**不是** 400「会话已过期」。
    #[tokio::test]
    async fn cursor_login_transient_failure_keeps_session() {
        let (app, _store) = app_with_workers(vec![]);
        let (_s, v) = cursor_start(&app, "cursor-keep", "").await;
        let flow_id = v["flow_id"].as_str().unwrap().to_string();
        let (first, _b1) = cursor_poll(&app, &flow_id).await;
        let (second, _b2) = cursor_poll(&app, &flow_id).await;
        // 无外网 → 两次都 502;有外网 → 两次都 200 pending。无论哪种,
        // 第二次都不该变 400(那意味着第一次把会话吃掉了)。
        assert_ne!(
            second,
            StatusCode::BAD_REQUEST,
            "第一次 poll 之后会话就没了(first={first}),后续轮询全废"
        );
        assert_eq!(first, second, "同一会话连续两次 poll 的结果应一致");
    }

    /// 会话表有硬上限:灌满后淘汰**最旧**的,内存有界。
    ///
    /// 用自己的 map 测 `cursor_make_room`,**不碰进程级全局表** —— 在全局表里灌
    /// 64 个会话会挤掉并发跑的其它测试的会话,那种 flaky 极难定位(为此排查了四轮)。
    #[test]
    fn cursor_login_room_making_evicts_oldest_and_bounds_memory() {
        use std::collections::HashMap;
        let mut m: HashMap<String, crate::admin::accounts::PendingCursorLogin> = HashMap::new();
        let cap = 4usize;
        // 灌 cap+6 个,每个 created_at 递增(靠 sleep 太慢,直接构造递增时刻)
        let base = std::time::Instant::now();
        for i in 0..(cap + 6) {
            crate::admin::accounts::cursor_make_room(&mut m, cap);
            m.insert(
                format!("flow-{i}"),
                crate::admin::accounts::PendingCursorLogin {
                    flow: gw_cursor::login::start(),
                    proxy: None,
                    account_id: format!("a{i}"),
                    group: String::new(),
                    max_concurrency: 2,
                    priority: 100,
                    // 越晚插入的越新
                    created_at: base + std::time::Duration::from_millis(i as u64),
                },
            );
            assert!(m.len() <= cap, "第 {i} 轮后 {} 个,超过上限 {cap}", m.len());
        }
        // 留下的必须是**最新**那批,最旧的已被淘汰
        assert!(!m.contains_key("flow-0"), "最旧的应被淘汰");
        assert!(m.contains_key(&format!("flow-{}", cap + 5)), "最新的应留下");
    }

    #[tokio::test]
    async fn cursor_login_endpoints_require_auth() {
        for path in ["/accounts/cursor/login/start", "/accounts/cursor/login/poll"] {
            let (app, _) = app();
            let r = Request::builder()
                .method("POST")
                .uri(path)
                .body(Body::empty())
                .unwrap();
            assert_eq!(
                app.oneshot(r).await.unwrap().status(),
                StatusCode::UNAUTHORIZED,
                "{path} 必须鉴权"
            );
        }
    }
    // CHUNK_ANCHOR_CURSOR_LOGIN_TESTS
    // CHUNK_ANCHOR_OAUTH_TESTS2

    /// start 命中真实 claude-dario 组 → 返回 authorize_url + state。
    #[tokio::test]
    async fn oauth_start_returns_authorize_url_for_dario_group() {
        let w = spawn_fake_worker("G0", "claude-dario", 200, serde_json::json!({})).await;
        let (app, store) = app_with_workers(vec![w]);
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
        store.create_group("G0", "", "").unwrap();
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
