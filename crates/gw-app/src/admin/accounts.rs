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
    /// provider 专属凭据字段(refresh_token 等),原样存为 extra JSON。
    #[serde(default)]
    extra: Option<serde_json::Map<String, serde_json::Value>>,
    /// 上号选择的出口网关:""/缺省/"direct"=直连;"auto"=自动均衡;数字=egress_pool 索引。
    /// 仅在 extra 未显式带 proxy 时生效。
    #[serde(default)]
    egress: Option<String>,
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
}

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/accounts", get(list_accounts).post(create_account))
        .route("/accounts/import", post(import_accounts))
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
    serde_json::json!({
        "account_id": row.account_id,
        "group_name": row.group_name,
        "provider": row.provider,
        "max_concurrency": row.max_concurrency,
        "disabled": row.disabled,
        "extra": extra,
        "created_at": row.created_at,
    })
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
    let extra_json = match serde_json::to_string(&extra_map) {
        Ok(s) => s,
        Err(e) => return internal_error(e),
    };
    let group = body.group.as_deref().unwrap_or("");
    // 非空组名必须真实存在,防"幽灵分组"(typo 的账号永远不被任何 worker 服务,
    // groups 页也看不见;审查 Minimalist#2)。
    if !group.is_empty() {
        match st.store.group_exists(group) {
            Ok(true) => {}
            Ok(false) => return api_error(StatusCode::BAD_REQUEST, "分组不存在"),
            Err(e) => return internal_error(e),
        }
    }
    let provider = body.provider.as_deref().filter(|p| !p.is_empty()).unwrap_or("kiro");
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
    if !group.is_empty() {
        match st.store.group_exists(group) {
            Ok(true) => {}
            Ok(false) => return api_error(StatusCode::BAD_REQUEST, "分组不存在"),
            Err(e) => return internal_error(e),
        }
    }

    let root: serde_json::Value = match serde_json::from_str(&body.json) {
        Ok(v) => v,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, &format!("JSON 解析失败: {e}")),
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
        match st.store.group_exists(g) {
            Ok(true) => {}
            Ok(false) => return api_error(StatusCode::BAD_REQUEST, "分组不存在"),
            Err(e) => return internal_error(e),
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

    use crate::admin::tests_support::{app, req};

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
}
