//! admin 自动补货端点。
//!
//! - `GET  /restock/state`      实况:开关、熔断、水位、库存余额、今日花费、决策流水
//! - `GET  /restock/params`     运行时参数(含上下限,前端据此渲染表单)
//! - `PUT  /restock/params`     改参数,即时生效无需重启
//! - `GET  /restock/credits`    积分曲线 + 按周预测 + 画像
//! - `GET  /restock/accounts`   ksk_ 账号清单(带成本与积分)
//! - `POST /restock/buy-now`    手动补一个(仍受花钱闸门约束)
//! - `POST /restock/reset-breaker`
//!
//! 鉴权由 `admin_api_router` 的 `route_layer` 统一加,这里不写。
//!
//! **密钥不出现在任何响应里**:drop 的 api_key 只在 `SystemConfig` 里,
//! 本模块从不回显它(连掩码形态都不给 —— 前端没有任何理由需要它)。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use gw_store::SqliteStore;

use super::{internal_error, AdminState};
use crate::restock::engine::{self, Engine};
use crate::restock::{forecast, params, registry};

pub fn router() -> Router<AdminState> {
    Router::new()
        .route("/restock/state", get(state))
        .route("/restock/params", get(get_params).put(put_params))
        .route("/restock/suppliers", get(get_suppliers).put(put_suppliers))
        .route("/restock/credits", get(credits))
        .route("/restock/accounts", get(accounts))
        .route("/restock/buy-now", post(buy_now))
        .route("/restock/reset-breaker", post(reset_breaker))
}

/// 4xx 错误响应(请求体问题)。与其它 admin 模块同形。
fn api_error(status: StatusCode, msg: &str) -> axum::response::Response {
    (status, Json(serde_json::json!({"type":"error","error":{"message": msg}}))).into_response()
}

/// 未配置 `restock:` 段时的统一回应。前端据 `configured:false` 把整块置灰,
/// 而不是显示一堆 0 让人以为「补货坏了」。
fn not_configured() -> axum::response::Response {
    Json(serde_json::json!({
        "configured": false,
        "hint": "未配置 restock 段。在 config/system.yaml 加 restock.enabled/base_url/api_key 后重启 router"
    }))
    .into_response()
}

/// 按需构造引擎。
///
/// 补货的后台循环在别处跑(受 DB 租约保护),这里只是为了让手动操作(立即补一个)
/// 和读数走同一套逻辑。每次 new 一个 reqwest client 有点浪费,但这些都是人手点的
/// 低频操作,不值得为它引一层共享状态。
fn engine_of(st: &AdminState) -> Option<Engine> {
    let cfg = &st.yaml_config.restock;
    if !cfg.enabled {
        return None;
    }
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
        .ok()?;
    Some(Engine::build(
        st.store.clone(),
        cfg,
        std::sync::Arc::new((*st.workers).clone()),
        http,
        // 管理面每次操作都是一个**独立的执行体**,必须有自己的 holder 身份 ——
        // 花钱锁靠它把「手动购买」和「后台循环」区分开并互斥。
        // 借用后台循环那个 id 会让两者互相认作自己人,锁形同虚设。
        format!("admin-{}", uuid::Uuid::new_v4()),
    ))
}

fn read_roster(st: &AdminState) -> Vec<registry::SupplierCfg> {
    let raw = st.store.get_kv(registry::KEY_SUPPLIERS).ok().flatten();
    registry::parse_roster(raw.as_deref())
}

fn read_params(st: &AdminState) -> params::Params {
    st.store
        .get_kv(SqliteStore::KEY_RESTOCK_PARAMS)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

// ───────────────────────── 实况 ─────────────────────────

async fn state(State(st): State<AdminState>) -> axum::response::Response {
    // 「配好了吗」= 这个二进制允许参与,**且名册上至少有一家拿得出密钥**。
    // 只看 yaml 的 drop 密钥会让「只配了 kiroapp」的部署被面板判成未配置。
    let configured = st.yaml_config.restock.enabled
        && registry::usable_count(&read_roster(&st), &st.yaml_config.restock.api_key) > 0;
    let p = read_params(&st);
    let snap: serde_json::Value = st
        .store
        .get_kv(engine::KEY_SNAPSHOT)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let breaker = st.store.get_kv(engine::KEY_BREAKER).ok().flatten().unwrap_or_default();
    let now = engine::now_ts();
    let (spent, bought) = st
        .store
        .restock_spent_since(p.local_day_start(now))
        .unwrap_or((0.0, 0));
    let decisions = st.store.restock_recent_decisions(60).unwrap_or_default();
    let orphans = st.store.restock_orphan_orders().map(|v| v.len()).unwrap_or(0);
    let in_flight = st.store.restock_pending_count().unwrap_or(0);
    let holder = st.store.restock_lease_holder().ok().flatten();

    Json(serde_json::json!({
        "configured": configured,
        "enabled": p.enabled,
        "dry_run": p.dry_run,
        "breaker": breaker,
        "in_peak": p.in_peak_window(now),
        "peak_window": format!("{}-{}", p.peak_start, p.peak_end),
        "min_healthy": p.min_healthy,
        "daily_cap_cny": p.daily_cap_cny,
        "spent_today": (spent * 100.0).round() / 100.0,
        "bought_today": bought,
        // 「买到了 key 却没上号」——钱花了号没进系统,必须单独醒目地报出来。
        "orphan_orders": orphans,
        // 「在途」——结果未知、正等对账重放确认的订单。与孤儿分开数:
        // 孤儿要人工上号,在途的**不要人碰**,碰了就是重复扣款。
        // 这个数长时间不降说明对账没在跑或对方一直确认不了,那才要人介入。
        "in_flight_orders": in_flight,
        // 哪个进程在跑补货。生产上有多个 router,这个数能让人确认互斥真的生效了。
        "lease_holder": holder,
        "snapshot": snap,
        "decisions": decisions,
    }))
    .into_response()
}

// ───────────────────────── 参数 ─────────────────────────

async fn get_params(State(st): State<AdminState>) -> axum::response::Response {
    let p = read_params(&st);
    let spec: Vec<serde_json::Value> = params::BOUNDS
        .iter()
        .map(|b| {
            serde_json::json!({
                "key": b.key, "kind": b.kind, "min": b.min, "max": b.max,
                "label": b.label, "hint": b.hint,
            })
        })
        .collect();
    Json(serde_json::json!({
        "configured": st.yaml_config.restock.enabled
            && registry::usable_count(&read_roster(&st), &st.yaml_config.restock.api_key) > 0,
        "spec": spec,
        "values": p,
    }))
    .into_response()
}

/// 部分更新。只认 `BOUNDS` 里列出的键,逐个校验后合并回整段 JSON。
///
/// 逐键校验而不是整段反序列化:后者遇到一个越界值会把整次保存打回,而且报不出
/// 是哪个字段的问题。
async fn put_params(
    State(st): State<AdminState>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let Some(obj) = body.as_object() else {
        return api_error(StatusCode::BAD_REQUEST, "请求体必须是对象");
    };
    if obj.is_empty() {
        return api_error(StatusCode::BAD_REQUEST, "请求体不能为空");
    }
    let mut cur = match serde_json::to_value(read_params(&st)) {
        Ok(serde_json::Value::Object(m)) => m,
        _ => return internal_error("参数序列化失败"),
    };
    let mut applied = Vec::new();
    for (k, v) in obj {
        match params::coerce(k, v) {
            Ok(good) => {
                applied.push(format!("{k}={good}"));
                cur.insert(k.clone(), good);
            }
            Err(e) => return api_error(StatusCode::BAD_REQUEST, &e),
        }
    }
    let merged = serde_json::Value::Object(cur);
    // 回读一遍确认能反序列化成 Params —— 挡住「单键都合法但组合起来结构不对」。
    let parsed: params::Params = match serde_json::from_value(merged.clone()) {
        Ok(p) => p,
        Err(e) => return api_error(StatusCode::BAD_REQUEST, &format!("参数组合非法: {e}")),
    };
    if let Err(e) = st
        .store
        .upsert_kv(SqliteStore::KEY_RESTOCK_PARAMS, &merged.to_string())
    {
        return internal_error(e);
    }
    tracing::warn!("补货:面板改参 {}", applied.join(", "));
    let _ = st.store.restock_log_decision(&gw_core::store::RestockDecision {
        ts: engine::now_ts(),
        action: "skip".into(),
        reason: format!("面板改参: {}", applied.join(", ")),
        healthy: None,
        stock: None,
        price_usd: None,
        balance_cny: None,
        detail: String::new(),
    });
    Json(serde_json::json!({ "values": parsed })).into_response()
}

// ───────────────────────── 货源名册 ─────────────────────────

/// 名册 + 每家的熔断状态。**响应里没有密钥**,只有 `has_key`。
async fn get_suppliers(State(st): State<AdminState>) -> axum::response::Response {
    let roster = read_roster(&st);
    let items: Vec<serde_json::Value> = roster
        .iter()
        .map(|c| {
            let mut v = c.redacted();
            let breaker = st
                .store
                .get_kv(&registry::breaker_key(c.id.trim()))
                .ok()
                .flatten()
                .unwrap_or_default();
            v["breaker"] = breaker.into();
            v
        })
        .collect();
    Json(serde_json::json!({ "items": items })).into_response()
}

#[derive(serde::Deserialize)]
pub struct PutSuppliersBody {
    items: Vec<SupplierPatch>,
}

/// 一家的可写字段。`api_key` **缺省 = 保留原值**,这样面板改并发/上限时
/// 不需要知道密钥,也不会像掩码方案那样把 `***` 当成真值存回去。
#[derive(serde::Deserialize)]
pub struct SupplierPatch {
    id: String,
    kind: String,
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    base_url: String,
    #[serde(default)]
    api_key: Option<String>,
    #[serde(default)]
    daily_cap_cny: f64,
    /// 档位,数值越小越优先。**缺省 = 保留原值**,与 `api_key` 同语义。
    ///
    /// ⚠️ 绝不能做成 `#[serde(default)] i64`(缺省 0)。这是整份替换的接口:
    /// 部署瞬间浏览器里缓存着的旧版面板 JS 不认识这个字段,它保存一次名册
    /// (哪怕只是想改个日上限)就会把**所有档位静默归零** —— 排序退回纯比价,
    /// 也就是这次改动要消灭的行为被悄悄回滚,而只留下一条 warn 日志。
    #[serde(default)]
    priority: Option<i64>,
    /// 逐货架档位覆盖(键是货架标识,如 `us` / `eu`)。**缺省 = 保留原值**,理由同上。
    #[serde(default)]
    shelf_priority: Option<std::collections::BTreeMap<String, i64>>,
}

/// 整份替换名册。校验通过才落库;**下一轮补货循环自动重建客户端,不用重启**。
async fn put_suppliers(
    State(st): State<AdminState>,
    Json(body): Json<PutSuppliersBody>,
) -> axum::response::Response {
    let old = read_roster(&st);
    let mut next = Vec::new();
    for p in body.items {
        // 同 id 的旧配置。缺省字段一律沿用它 —— 改名等于换一家,那种情况本来就该重填。
        let prev = old.iter().find(|o| o.id.trim() == p.id.trim());
        let api_key = match p.api_key {
            Some(k) => k.trim().to_string(),
            None => prev.map(|o| o.api_key.clone()).unwrap_or_default(),
        };
        let priority = p.priority.unwrap_or_else(|| prev.map(|o| o.priority).unwrap_or(0));
        let shelf_priority = match p.shelf_priority {
            // 货架键两端去空白:面板上手输的 " eu" 匹配不到任何货架,而那是**静默失效**。
            // trim 后撞车(`eu` 与 ` eu` 同时存在)取后写入的那个 —— 面板产生不了这种输入,
            // 手写 JSON 撞车时静默择一也好过整份名册被拒。
            Some(m) => m.into_iter().map(|(k, v)| (k.trim().to_string(), v)).collect(),
            None => prev.map(|o| o.shelf_priority.clone()).unwrap_or_default(),
        };
        next.push(registry::SupplierCfg {
            id: p.id.trim().to_string(),
            kind: p.kind.trim().to_string(),
            enabled: p.enabled,
            base_url: p.base_url.trim().to_string(),
            api_key,
            daily_cap_cny: p.daily_cap_cny,
            priority,
            shelf_priority,
        });
    }
    if let Err(e) = registry::validate_roster(&next) {
        return api_error(StatusCode::BAD_REQUEST, &e);
    }
    // ── 在途订单的货源**不许被删掉或停用** ──
    //
    // 一张 pending 单的语义是「钱可能已经扣了,正等着向那一家确认」。把那一家从名册里
    // 移走(删除、改名、或停用)之后,`reconcile_pending` 找不到客户端,只能记一条日志
    // 然后永久跳过 —— 那笔钱和那批 key 就再也回不来了。
    //
    // 停用也拦:`registry::build` 只建 enabled 的家,停用与删除对对账是同一个后果。
    // 想撤掉一家的正确顺序是:先等在途单收尾(或人工处理),再改名册。
    let live: std::collections::HashSet<String> =
        next.iter().filter(|c| c.enabled).map(|c| c.id.trim().to_string()).collect();
    if let Ok(pending) = st.store.restock_pending_orders(0) {
        let mut orphaned: Vec<String> = pending
            .iter()
            .map(|o| if o.supplier.is_empty() { "drop".to_string() } else { o.supplier.clone() })
            .filter(|s| !live.contains(s))
            .collect();
        orphaned.sort();
        orphaned.dedup();
        if !orphaned.is_empty() {
            return api_error(
                StatusCode::CONFLICT,
                &format!(
                    "货源 {} 还有在途订单(钱可能已扣、正等对账),现在删除或停用会让那笔钱\
                     再也找不回来。请先等在途订单收尾或人工处理,再改名册。",
                    orphaned.join("、")
                ),
            );
        }
    }
    let json = match serde_json::to_string(&next) {
        Ok(j) => j,
        Err(e) => return internal_error(e),
    };
    if let Err(e) = st.store.upsert_kv(registry::KEY_SUPPLIERS, &json) {
        return internal_error(e);
    }
    // 档位进日志:选家规则改了之后,「为什么这轮买的是它」的第一手证据是名册,
    // 而名册只在被改的这一刻留得下痕迹。
    let names: Vec<String> = next
        .iter()
        .map(|c| {
            let ov = if c.shelf_priority.is_empty() {
                String::new()
            } else {
                let mut kv: Vec<String> =
                    c.shelf_priority.iter().map(|(k, v)| format!("{k}={v}")).collect();
                kv.sort();
                format!("[{}]", kv.join(","))
            };
            format!("{}@{}{ov}{}", c.id, c.priority, if c.enabled { "" } else { "(停用)" })
        })
        .collect();
    tracing::warn!("补货:面板改货源名册 → {}", names.join(", "));
    let _ = st.store.restock_log_decision(&gw_core::store::RestockDecision {
        ts: engine::now_ts(),
        action: "skip".into(),
        reason: format!("面板改货源名册: {}", names.join(", ")),
        healthy: None,
        stock: None,
        price_usd: None,
        balance_cny: None,
        detail: String::new(),
    });
    Json(serde_json::json!({
        "items": next.iter().map(|c| c.redacted()).collect::<Vec<_>>()
    }))
    .into_response()
}

// ───────────────────────── 积分曲线 ─────────────────────────

async fn credits(
    State(st): State<AdminState>,
    axum::extract::Query(q): axum::extract::Query<CreditsQuery>,
) -> axum::response::Response {
    let p = read_params(&st);
    let off = p.utc_offset_secs();
    let hours = q.hours.unwrap_or(48).clamp(6, 24 * 30);
    let now = engine::now_ts();
    let cur_hour = now - now.rem_euclid(3600);
    let since = cur_hour - (hours - 1) * 3600;

    let series = match st.store.restock_credit_series(since) {
        Ok(v) => v,
        Err(e) => return internal_error(e),
    };
    // 逐小时补零:缺的小时是「那小时没流量」,不是「时间跳过去了」。
    // 少了这步横轴会撒谎 —— 密集与稀疏的时段在图上宽度一样。
    let mut buckets: std::collections::BTreeMap<i64, (f64, f64, i64, i64)> = Default::default();
    let mut models: std::collections::HashMap<String, f64> = Default::default();
    for r in &series {
        let e = buckets.entry(r.hour_ts).or_default();
        e.1 += r.credits;
        e.3 += r.calls;
        if r.ksk {
            e.0 += r.credits;
            e.2 += r.calls;
            *models.entry(r.model.clone()).or_default() += r.credits;
        }
    }
    let mut out = Vec::new();
    let mut t = since;
    while t <= cur_hour {
        let (ksk, all, kc, ac) = buckets.get(&t).copied().unwrap_or_default();
        out.push(serde_json::json!({
            "ts": t,
            "hour": forecast::local_hour(t, off),
            "weekday": forecast::local_weekday(t, off),
            "ksk": (ksk * 10.0).round() / 10.0,
            "credits": (all * 10.0).round() / 10.0,
            "ksk_calls": kc,
            "calls": ac,
            "partial": t >= cur_hour,
        }));
        t += 3600;
    }

    // 预测拿**全部**历史建周画像,不止显示窗口。只喂已走完的小时。
    let hist: Vec<(i64, f64, f64)> = st
        .store
        .restock_credit_hours(0)
        .unwrap_or_default()
        .into_iter()
        .filter(|(ts, _, _)| *ts < cur_hour)
        .collect();
    let fc = forecast::forecast(&hist, off, cur_hour + 3600, p.forecast_hours.max(1));
    let cov = forecast::coverage(&hist, off);
    let mut top: Vec<_> = models.into_iter().collect();
    top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    top.truncate(8);

    let buys: Vec<serde_json::Value> = st
        .store
        .restock_recent_orders(500)
        .unwrap_or_default()
        .into_iter()
        .filter(|o| o.created_at >= since && matches!(o.status.as_str(), "purchased" | "imported"))
        .map(|o| serde_json::json!({ "ts": o.created_at, "spent_cny": o.spent_cny }))
        .collect();

    Json(serde_json::json!({
        "series": out,
        "forecast": fc,
        "forecast_hours": p.forecast_hours,
        "forecast_demand": forecast::total_demand(&fc),
        "coverage": cov,
        "models": top.iter().map(|(m, c)| serde_json::json!({
            "model": m, "credits": (c * 10.0).round() / 10.0
        })).collect::<Vec<_>>(),
        "buys": buys,
        "peak_start": p.peak_start,
        "peak_end": p.peak_end,
        "utc_offset_minutes": p.utc_offset_minutes,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
pub struct CreditsQuery {
    hours: Option<i64>,
}

// ───────────────────────── 账号清单 ─────────────────────────

/// ksk_ 号清单,**按创建时间倒序**,带成本、实测积分、服务时长与死法。
///
/// caio 自己的账号页给不出「这个号花了多少钱、产出多少调用」—— 那要跨账号表、
/// usage 表和补货订单表才算得出来,而这恰恰是管理这批短命号最需要的两个数。
///
/// 另外两列是 2026-08-04 那次复盘逼出来的:实测这批号一律只活 0.7–0.9 小时,
/// **且与烧速无关**(号按墙上时钟死),所以「花了多少 ÷ 榨出多少积分」才是判断
/// 买贵没买贵的尺子;而「死法」分得出「被风控封」和「key 被吊销」——
/// 后者占了 9/22,是供货质量问题,光看总产出根本看不出来。
async fn accounts(State(st): State<AdminState>) -> axum::response::Response {
    let rows = match st.store.restock_account_inventory() {
        Ok(v) => v,
        Err(e) => return internal_error(e),
    };
    // 死法只存在于 worker 内存,得扇出去问。这是页面触发的读路径,不是热路径;
    // worker 离线也不影响主体,那一列显示空即可。
    let mut reason: std::collections::HashMap<String, String> = Default::default();
    let fetches = st.workers.iter().map(|w| {
        let http = st.http.clone();
        let url = format!("http://{}/health", w.listen);
        async move { http.get(&url).send().await.ok()?.json::<serde_json::Value>().await.ok() }
    });
    for v in futures::future::join_all(fetches).await.into_iter().flatten() {
        for a in v.get("accounts_status").and_then(|x| x.as_array()).unwrap_or(&vec![]) {
            if let Some(id) = a.get("account_id").and_then(|x| x.as_str()) {
                reason
                    .entry(id.to_string())
                    .or_insert_with(|| {
                        a.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string()
                    });
            }
        }
    }
    // 自购号的成本:把订单金额摊到该单买到的号上(单次买 1 个时就是全额)。
    let mut cost: std::collections::HashMap<String, f64> = Default::default();
    for o in st.store.restock_recent_orders(500).unwrap_or_default() {
        if !matches!(o.status.as_str(), "purchased" | "imported") || o.count <= 0 {
            continue;
        }
        let per = o.spent_cny / o.count as f64;
        for aid in st.store.restock_accounts_of_order(&o.client_order_id).unwrap_or_default() {
            cost.insert(aid, per);
        }
    }
    let items: Vec<serde_json::Value> = rows
        .into_iter()
        .map(|r| {
            let c = cost.get(&r.account_id).copied();
            // 服务时长 = 首末次用量之差。号只活 0.7–0.9 小时,分钟才是有意义的刻度。
            let served = match (r.first_used_at, r.last_used_at) {
                (Some(a), Some(b)) if b > a => Some(b - a),
                _ => None,
            };
            serde_json::json!({
                "account_id": r.account_id,
                "created_at": r.created_at,
                "disabled": r.disabled,
                "max_concurrency": r.max_concurrency,
                "calls": r.calls,
                "success": r.success,
                "credits": (r.credits * 10.0).round() / 10.0,
                "groups": r.groups,
                "cost_cny": c,
                "self_bought": c.is_some(),
                "unit_cost": c.filter(|_| r.success > 0)
                    .map(|c| (c / r.success as f64 * 10000.0).round() / 10000.0),
                // ¥/积分:积分才是号的真实刻度(opus 一次约 1.1 分、haiku 一次 0.03 分,
                // 差 40 倍),按「次」算单价会被模型结构带偏。
                "unit_cost_per_credit": c.filter(|_| r.credits > 0.0)
                    .map(|c| (c / r.credits * 10000.0).round() / 10000.0),
                "served_secs": served,
                "first_used_at": r.first_used_at,
                "last_used_at": r.last_used_at,
                "reason": reason.get(&r.account_id).cloned().unwrap_or_default(),
            })
        })
        .collect();
    Json(serde_json::json!({ "count": items.len(), "items": items })).into_response()
}

// ───────────────────────── 操作 ─────────────────────────

async fn buy_now(State(st): State<AdminState>) -> axum::response::Response {
    let Some(eng) = engine_of(&st) else {
        return not_configured();
    };
    // force 只越过自动化闸门(开关/窗口/水位/闲时),**花钱闸门一律照旧**。
    let d = eng.run_once(true).await;
    let p = read_params(&st);
    eng.refresh_snapshot(0, true).await;
    Json(serde_json::json!({
        "act": d.act,
        "message": if p.dry_run { format!("[DRY-RUN] {}", d.reason) } else { d.reason },
    }))
    .into_response()
}

/// 解除熔断。**一次清干净**:全局的那一个,加名册上每一家自己的。
///
/// 不做成「逐家解除」是因为这个按钮的语义是人已经看过原因、决定继续 ——
/// 留着某一家还熔着,人会以为已经恢复了,然后对着「为什么还是不买」再查一遍。
async fn reset_breaker(State(st): State<AdminState>) -> axum::response::Response {
    if let Err(e) = st.store.upsert_kv(engine::KEY_BREAKER, "") {
        return internal_error(e);
    }
    let _ = st.store.upsert_kv(engine::KEY_FAIL_STREAK, "0");
    for c in read_roster(&st) {
        let id = c.id.trim();
        let _ = st.store.upsert_kv(&registry::breaker_key(id), "");
        let _ = st.store.upsert_kv(&registry::fault_streak_key(id), "0");
    }
    tracing::warn!("补货:面板解除熔断(全局 + 各货源)");
    let _ = st.store.restock_log_decision(&gw_core::store::RestockDecision {
        ts: engine::now_ts(),
        action: "skip".into(),
        reason: "面板操作: 熔断已解除(全局与各货源)".into(),
        healthy: None,
        stock: None,
        price_usd: None,
        balance_cny: None,
        detail: String::new(),
    });
    Json(serde_json::json!({ "ok": true })).into_response()
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
    async fn restock_requires_auth() {
        let (app, _store) = app();
        let unauth = axum::http::Request::builder()
            .method("GET")
            .uri("/restock/state")
            .body(axum::body::Body::empty())
            .unwrap();
        let resp = app.oneshot(unauth).await.unwrap();
        assert_eq!(resp.status(), 401);
    }

    #[tokio::test]
    async fn 未配置时状态可读且标记未配置() {
        // tests_support 用的是 SystemConfig::default(),restock 段为空。
        let (app, _store) = app();
        let resp = app.oneshot(req("GET", "/restock/state", None)).await.unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(v["configured"], false, "未配置要显式标记,前端据此置灰而不是显示一堆 0");
        assert_eq!(v["enabled"], false, "默认必须是关的");
        assert_eq!(v["dry_run"], true);
    }

    #[tokio::test]
    async fn 未配置时手动购买不会尝试花钱() {
        let (app, _store) = app();
        let resp = app.oneshot(req("POST", "/restock/buy-now", None)).await.unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(v["configured"], false);
    }

    #[tokio::test]
    async fn 改参数校验越界并即时生效() {
        let (app, store) = app();
        let resp = app
            .clone()
            .oneshot(req("PUT", "/restock/params", Some(r#"{"min_healthy":3}"#)))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(body_json(resp).await["values"]["min_healthy"], 3);
        // 落库了才算即时生效。
        let saved = store.get_kv(gw_store::SqliteStore::KEY_RESTOCK_PARAMS).unwrap().unwrap();
        assert!(saved.contains("\"min_healthy\":3"));

        // 越界必须挡住 —— 手机上误触一个数字不该让服务失控。
        let bad = app
            .clone()
            .oneshot(req("PUT", "/restock/params", Some(r#"{"min_healthy":9999}"#)))
            .await
            .unwrap();
        assert_eq!(bad.status(), 400);
        // 未知键也挡住,免得前端拼错字段却静默无效。
        let unknown = app
            .oneshot(req("PUT", "/restock/params", Some(r#"{"nope":1}"#)))
            .await
            .unwrap();
        assert_eq!(unknown.status(), 400);
    }

    #[tokio::test]
    async fn 解除熔断会清空熔断与连败计数() {
        let (app, store) = app();
        store.upsert_kv(crate::restock::engine::KEY_BREAKER, "连续 2 次失败").unwrap();
        store.upsert_kv(crate::restock::engine::KEY_FAIL_STREAK, "2").unwrap();
        let resp = app.oneshot(req("POST", "/restock/reset-breaker", None)).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(store.get_kv(crate::restock::engine::KEY_BREAKER).unwrap().unwrap(), "");
        assert_eq!(store.get_kv(crate::restock::engine::KEY_FAIL_STREAK).unwrap().unwrap(), "0");
    }

    #[tokio::test]
    async fn 空积分库也能出曲线不报错() {
        let (app, _store) = app();
        let resp = app.oneshot(req("GET", "/restock/credits?hours=12", None)).await.unwrap();
        assert_eq!(resp.status(), 200);
        let v = body_json(resp).await;
        assert_eq!(v["series"].as_array().unwrap().len(), 12, "缺的小时要补零,横轴不能撒谎");
        assert_eq!(v["coverage"]["mature"], false);
    }
}
