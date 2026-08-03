//! 补货决策引擎:什么时候买、买完怎么上号、死号怎么回收。
//!
//! **闸门顺序不可随意调换。** 分两类:
//! - **自动化闸门**(开关 / 高峰窗口 / 水位 / 闲时抑制):面板的「立即补一个」可以越过,
//!   因为那是人明确要买。
//! - **花钱闸门**(熔断 / 日上限 / 余额 / 单价 / dry-run):**任何情况都不许越过**,
//!   包括手动触发。这是自动花钱系统的底线。
//!
//! 顺序上把便宜的判断放前面(读本地 DB),把要打对方接口的放后面 —— 水位没破就
//! 根本不该去问对方库存,那是白白增加对上游的请求密度。

use std::sync::Arc;

use gw_core::config::WorkerConfig;
use gw_core::store::{AccountPatch, RestockDecision};
use gw_store::{MembershipOutcome, SqliteStore};

use super::drop::{max_total_cny_for, new_order_id, DropClient};
use super::forecast;
use super::params::Params;

/// 面板快照在 `settings` 表里的键名。轮询线程写、admin 读。
pub const KEY_SNAPSHOT: &str = "restock_snapshot";
/// 熔断状态(非空即熔断,内容是原因)。
pub const KEY_BREAKER: &str = "restock_breaker";
/// 连续「买到却没上号」的次数。
pub const KEY_FAIL_STREAK: &str = "restock_import_fail_streak";

/// 一轮决策的结论。
#[derive(Debug, Clone, Default)]
pub struct Decision {
    pub act: bool,
    pub reason: String,
    pub healthy: Option<i64>,
    pub stock: Option<i64>,
    pub price_usd: Option<f64>,
    pub balance_cny: Option<f64>,
}

impl Decision {
    fn skip(reason: impl Into<String>) -> Self {
        Self { act: false, reason: reason.into(), ..Default::default() }
    }
}

/// ksk_ 号池的健康度快照。
#[derive(Debug, Clone, Default)]
pub struct Health {
    /// caio 里状态「正常」的 ksk_ 号数。**补货水位比的就是它。**
    ///
    /// 开了排队模式(`queue_enabled`)后 429 只设 `paced_until`、账号不下线,
    /// 所以一个号掉出正常态基本只剩一个原因:上游 403 `TEMPORARILY_SUSPENDED`。
    /// 而那是**永久的**(实测 0/35 个号在 1 小时冷却后恢复过),所以「只数正常号」
    /// 是精确判据而非近似。
    pub healthy: i64,
    pub cooling: i64,
    pub dead: i64,
    pub total: i64,
    /// 至少有一个 worker 在线且给出了运行态。为 false 时**不许下单** ——
    /// 读不到健康度就当成 0 个健康号会触发连环购买。
    pub any_online: bool,
}

pub struct Engine {
    pub store: Arc<SqliteStore>,
    pub drop: DropClient,
    pub workers: Arc<Vec<WorkerConfig>>,
    /// 打 worker loopback `/health` 与 `/sync` 用。短超时:worker 离线要快速跳过。
    pub http: reqwest::Client,
}

impl Engine {
    /// 读当前运行时参数(面板改完即时生效,所以每轮都重读)。
    pub fn params(&self) -> Params {
        self.store
            .get_kv(SqliteStore::KEY_RESTOCK_PARAMS)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save_params(&self, p: &Params) -> anyhow::Result<()> {
        self.store
            .upsert_kv(SqliteStore::KEY_RESTOCK_PARAMS, &serde_json::to_string(p)?)
    }

    // ───────────────────────── 健康度 ─────────────────────────

    /// 逐 worker 拉 `/health`,与 DB 里的 ksk_ 账号集求交,数出健康度。
    ///
    /// 运行态(冷却/封禁/在途并发)只存在于 worker 内存,router 侧必须扇出去问。
    pub async fn health(&self) -> anyhow::Result<Health> {
        let ksk: std::collections::HashSet<String> =
            self.store.restock_ksk_account_ids()?.into_iter().collect();
        let mut h = Health { total: ksk.len() as i64, ..Default::default() };
        let fetches = self.workers.iter().map(|w| {
            let http = self.http.clone();
            let url = format!("http://{}/health", w.listen);
            async move { http.get(&url).send().await.ok()?.json::<serde_json::Value>().await.ok() }
        });
        let mut seen = std::collections::HashSet::new();
        for v in futures::future::join_all(fetches).await.into_iter().flatten() {
            h.any_online = true;
            let Some(arr) = v.get("accounts_status").and_then(|a| a.as_array()) else {
                continue;
            };
            for a in arr {
                let Some(id) = a.get("account_id").and_then(|x| x.as_str()) else {
                    continue;
                };
                if !ksk.contains(id) || !seen.insert(id.to_string()) {
                    continue;
                }
                let reason = a.get("reason").and_then(|x| x.as_str()).unwrap_or("");
                let disabled = a.get("disabled").and_then(|x| x.as_bool()).unwrap_or(false);
                match reason {
                    "" if !disabled => h.healthy += 1,
                    // quota_exhausted / invalid_refresh_token 是真死;其余(限流/临时封禁/
                    // 空响应/连败)caio 归为会自愈的冷却 —— 但 temporarily_suspended 实测
                    // 从不自愈,所以这里只做展示区分,水位一律只认 healthy。
                    "quota_exhausted" | "invalid_refresh_token" => h.dead += 1,
                    _ => h.cooling += 1,
                }
            }
        }
        Ok(h)
    }

    /// 刷新面板快照(健康度每轮刷,drop 的库存/余额按 `stock_max_age` 节流)。
    ///
    /// 面板每 15 秒轮询一次 `/restock/state`,不能每次都去打对方接口 ——
    /// 那是白白增加对上游的请求密度,而库存和余额本来就不需要秒级精度。
    /// `force` 用于刚下过单之后:不强刷的话面板会在几分钟内显示购买前的余额,
    /// 看到「买了但余额没动」必然让人以为没扣款。
    pub async fn refresh_snapshot(&self, stock_max_age: i64, force: bool) {
        let now = now_ts();
        let mut snap: serde_json::Value = self
            .store
            .get_kv(KEY_SNAPSHOT)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(|| serde_json::json!({}));

        if let Ok(h) = self.health().await {
            snap["healthy"] = h.healthy.into();
            snap["cooling"] = h.cooling.into();
            snap["dead"] = h.dead.into();
            snap["total"] = h.total.into();
            snap["any_online"] = h.any_online.into();
            snap["at"] = now.into();
        }

        let last = snap.get("stock_at").and_then(|v| v.as_i64()).unwrap_or(0);
        if force || now - last >= stock_max_age {
            match self.drop.stock().await {
                Ok(st) => {
                    snap["stock"] = st.stock.into();
                    snap["price_usd"] = st.price_usd.into();
                    snap["balance_cny"] = st.balance_cny.into();
                    snap["stock_at"] = now.into();
                    snap["drop_ok"] = true.into();
                }
                Err(e) => {
                    snap["drop_ok"] = false.into();
                    snap["drop_error"] = e.to_string().into();
                    tracing::warn!("补货:读取 drop 库存失败: {e}");
                }
            }
        }
        let _ = self
            .store
            .upsert_kv(KEY_SNAPSHOT, &snap.to_string());
    }

    // ───────────────────────── 决策 ─────────────────────────

    pub async fn evaluate(&self, force: bool) -> Decision {
        let p = self.params();
        let now = now_ts();

        // ── 自动化闸门(force 可越过)──
        if !force && !p.enabled {
            return Decision::skip("自动补货开关处于关闭状态");
        }

        // ── 花钱闸门:熔断(force 也不许越过)──
        if let Ok(Some(r)) = self.store.get_kv(KEY_BREAKER) {
            if !r.is_empty() {
                return Decision::skip(format!("熔断已触发,停止自动购买: {r}"));
            }
        }

        if !force && !p.in_peak_window(now) {
            return Decision::skip(format!(
                "不在高峰窗口 {}-{}(UTC{:+})",
                p.peak_start,
                p.peak_end,
                p.utc_offset_minutes / 60
            ));
        }

        // ── 花钱闸门:日上限 ──
        let day_start = p.local_day_start(now);
        let (spent, _bought) = self.store.restock_spent_since(day_start).unwrap_or((0.0, 0));
        if spent >= p.daily_cap_cny {
            return Decision::skip(format!(
                "当日已花费 ¥{spent:.2} 达上限 ¥{:.2}",
                p.daily_cap_cny
            ));
        }

        // 健康度读失败即中止本轮,**绝不"当作 0 个健康号"去买** ——
        // worker 短暂不可用时那会触发连环购买。
        let health = match self.health().await {
            Ok(h) if h.any_online => h,
            Ok(_) => return Decision::skip("没有 worker 在线,读不到运行态,本轮跳过"),
            Err(e) => return Decision::skip(format!("读取健康度失败,本轮跳过: {e}")),
        };

        if !force && health.healthy >= p.min_healthy {
            return Decision {
                healthy: Some(health.healthy),
                ..Decision::skip(format!(
                    "正常 ksk_ 号 {} 个 ≥ 阈值 {},无需补货(冷却 {},真死 {})",
                    health.healthy, p.min_healthy, health.cooling, health.dead
                ))
            };
        }

        // ── 闲时抑制:只会阻止购买、永不促成购买 ──
        // 水位已经破了,但如果预测接下来根本没人用,买来的号只会闲置到被上游扫号扫死
        // (实测号平均只用掉 13.6% 配额就被封)。等有需求再买。
        // `healthy == 0` 时硬豁免:断供的代价永远大于一个号的钱。
        if !force && p.idle_skip_ratio > 0.0 && health.healthy > 0 {
            if let Some(why) = self.idle_check(&p, now) {
                return Decision { healthy: Some(health.healthy), ..Decision::skip(why) };
            }
        }

        // ── 到这里才去问对方库存(前面没破水位就不该增加上游请求密度)──
        let st = match self.drop.stock().await {
            Ok(s) => s,
            Err(e) => {
                return Decision {
                    healthy: Some(health.healthy),
                    ..Decision::skip(format!("查询 drop 库存失败: {e}"))
                }
            }
        };
        let base = Decision {
            healthy: Some(health.healthy),
            stock: Some(st.stock),
            price_usd: Some(st.price_usd),
            balance_cny: Some(st.balance_cny),
            ..Default::default()
        };
        if st.stock <= 0 {
            return Decision { reason: "drop 无库存".into(), ..base };
        }
        // ── 花钱闸门:单价异常兜底 ──
        if st.price_usd > p.max_price_usd {
            return Decision {
                reason: format!(
                    "单价 ${:.2} 高于上限 ${:.2},不买",
                    st.price_usd, p.max_price_usd
                ),
                ..base
            };
        }

        let count = p.max_per_purchase.min(st.stock).max(1);
        let need = max_total_cny_for(count, st.price_usd, p.rate_cap);

        // ── 花钱闸门:余额 ──
        if st.balance_cny < need + p.min_balance_reserve_cny {
            return Decision {
                reason: format!(
                    "余额 ¥{:.2} 不足(需 ¥{need:.2} + 保留 ¥{:.2})",
                    st.balance_cny, p.min_balance_reserve_cny
                ),
                ..base
            };
        }
        // ── 花钱闸门:本单会不会突破日上限 ──
        if spent + need > p.daily_cap_cny {
            return Decision {
                reason: format!(
                    "本单预计 ¥{need:.2} 会突破日上限(已花 ¥{spent:.2} / ¥{:.2})",
                    p.daily_cap_cny
                ),
                ..base
            };
        }

        let trigger = if force {
            "手动触发".to_string()
        } else {
            format!("正常 ksk_ 号 {} < 阈值 {}", health.healthy, p.min_healthy)
        };
        Decision {
            act: true,
            reason: format!("{trigger},库存 {},买 {count} 个(限价 ¥{need:.2})", st.stock),
            ..base
        }
    }

    /// 预测接下来是闲时则返回跳过理由。判据是**相对**的(预测/历史峰值),
    /// 因为流量总盘会随客户增减而变,写死一个绝对值过两周就不对了。
    fn idle_check(&self, p: &Params, now: i64) -> Option<String> {
        let cur_hour = now - now.rem_euclid(3600);
        let hist: Vec<(i64, f64, f64)> = self
            .store
            .restock_credit_hours(0)
            .ok()?
            .into_iter()
            .filter(|(t, _, _)| *t < cur_hour)
            .collect();
        // 拿三五个样本猜出来的「闲时」不配挡购买。
        if hist.len() < 24 {
            return None;
        }
        // 峰值也取**总量**,与预测同口径 —— 混用会让比值失去意义。
        let peak = hist.iter().map(|&(_, _, t)| t).fold(0.0_f64, f64::max);
        if peak <= 0.0 {
            return None;
        }
        let pts = forecast::forecast(
            &hist,
            p.utc_offset_secs(),
            cur_hour + 3600,
            p.forecast_hours.max(1),
        );
        // 用窗口内的**最大**小时而非平均:平均会被凌晨那几个零小时拉平,
        // 把「两小时后就要起量」错判成闲时。
        let ahead = pts.iter().map(|x| x.credits).fold(0.0_f64, f64::max);
        let ratio = ahead / peak;
        if ratio >= p.idle_skip_ratio {
            return None;
        }
        Some(format!(
            "闲时抑制: 未来 {}h 预测峰值 {ahead:.0} 分/时,仅为历史峰值 {peak:.0} 的 {:.0}%\
             (阈值 {:.0}%),依据 {}。买了也是闲置等被扫,推迟到有需求时再补",
            p.forecast_hours,
            ratio * 100.0,
            p.idle_skip_ratio * 100.0,
            pts.first().map(|x| x.basis).unwrap_or(forecast::BASIS_NONE),
        ))
    }

    // ───────────────────────── 执行 ─────────────────────────

    pub async fn run_once(&self, force: bool) -> Decision {
        let d = self.evaluate(force).await;
        if !d.act {
            self.log("skip", &d);
            tracing::info!("补货跳过: {}", d.reason);
            return d;
        }
        let p = self.params();
        if p.dry_run {
            let mut dd = d.clone();
            dd.reason = format!("[DRY-RUN] 本应购买 —— {}", d.reason);
            self.log("skip", &dd);
            tracing::warn!("[DRY-RUN] 本应购买: {}", d.reason);
            return d;
        }
        self.buy_and_onboard(&p, &d).await;
        d
    }

    async fn buy_and_onboard(&self, p: &Params, d: &Decision) {
        let count = p.max_per_purchase.min(d.stock.unwrap_or(1)).max(1);
        let price = d.price_usd.unwrap_or(0.0);
        let cap = max_total_cny_for(count, price, p.rate_cap);
        let order_id = new_order_id();

        // ① 幂等键先落库。落库失败就**不发请求** —— 否则崩在途中会丢失 order_id,
        //    重试即变成重复扣款。
        if let Err(e) = self.store.restock_create_order(&order_id, count, cap) {
            tracing::error!("补货:幂等键落库失败,放弃本轮购买: {e}");
            self.log_msg("error", &format!("幂等键落库失败,未发出购买请求: {e}"));
            return;
        }

        let res = match self.drop.purchase(count, &order_id, cap).await {
            Ok(r) => r,
            Err(e) => {
                let _ = self.store.restock_mark_status(&order_id, "failed", &e.to_string());
                self.log_msg("error", &format!("购买失败: {e}"));
                tracing::error!("补货:购买失败 order={order_id}: {e}");
                // 409 是正常的竞争失败(库存/价格/余额),不计熔断;其余按故障处理。
                if !e.is_price_or_stock_conflict() {
                    self.maybe_trip(p, &format!("购买异常: {e}"));
                }
                return;
            }
        };

        // ② key 一到手立刻落库,后续任何失败都不会让它蒸发。
        let spent = self
            .store
            .restock_mark_purchased(&order_id, &res.keys, d.balance_cny, res.remaining_cny)
            .unwrap_or(0.0);
        tracing::warn!(
            "补货:购买成功 order={order_id} 数量={} 实扣=¥{spent:.2} 余额=¥{:.2}",
            res.purchased,
            res.remaining_cny
        );
        self.log_msg(
            "buy",
            &format!(
                "购买 {} 个,实扣 ¥{spent:.2},余额 ¥{:.2}",
                res.purchased, res.remaining_cny
            ),
        );
        if d.balance_cny.is_none() || spent <= 0.0 {
            // 拿不到余额差就无法准确记账,日预算阀会失真 —— 必须让人看见。
            tracing::error!("补货:order={order_id} 无法计算实际扣款,日预算累计可能失真");
        }

        if res.keys.is_empty() {
            let _ = self
                .store
                .restock_mark_status(&order_id, "failed", "响应未包含任何 ksk_ key");
            self.maybe_trip(p, "购买成功但响应无 key");
            return;
        }
        self.onboard(p, &order_id, &res.keys).await;
    }

    /// 建号 → 逐组提权 → 调并发 → 开排队 → 捅 worker 同步。
    ///
    /// 搬进 caio 之后这些全是**内部调用**,不再走 admin HTTP。
    async fn onboard(&self, p: &Params, order_id: &str, keys: &[String]) {
        let json = match serde_json::to_string(keys) {
            Ok(j) => j,
            Err(e) => {
                self.fail_import(p, order_id, &format!("序列化 key 失败: {e}"));
                return;
            }
        };
        let parsed = match gw_kiro::import::parse_accounts_export(
            &serde_json::from_str::<serde_json::Value>(&json).unwrap_or_default(),
        ) {
            Ok(v) => v,
            Err(e) => {
                self.fail_import(p, order_id, &format!("解析 key 失败: {e}"));
                return;
            }
        };
        if parsed.is_empty() {
            self.fail_import(p, order_id, "解析后没有可导入的账号");
            return;
        }

        let mut new_ids = Vec::new();
        for acc in &parsed {
            let extra = serde_json::Value::Object(acc.extra.clone()).to_string();
            match self.store.create_account(
                &acc.account_id,
                &p.import_group,
                "kiro",
                p.new_account_concurrency,
                &extra,
            ) {
                // false = 已存在(重复 key),不算新号但也不是错误。
                Ok(true) => new_ids.push(acc.account_id.clone()),
                Ok(false) => tracing::warn!("补货:账号 {} 已存在,跳过", acc.account_id),
                Err(e) => {
                    self.fail_import(p, order_id, &format!("建号失败: {e}"));
                    return;
                }
            }
        }
        if new_ids.is_empty() {
            self.fail_import(p, order_id, "导入未产生新账号(可能重复)");
            return;
        }

        // 提权与调参。号已经在系统里了,这些失败不算"钱白花",故**不计熔断**,
        // 但必须记录 —— 一个 priority 没提上去的号等于白买(排在 100 档吃不到流量)。
        let (groups, bad) = p.parsed_member_groups();
        let mut failed: Vec<String> = bad
            .iter()
            .map(|b| format!("成员边配置无法解析: {b}"))
            .collect();
        for aid in &new_ids {
            for (g, pri) in &groups {
                match self.store.upsert_membership(aid, g, *pri) {
                    Ok(MembershipOutcome::Ok) => {}
                    Ok(other) => failed.push(format!("{aid}@{g}: {other:?}")),
                    Err(e) => failed.push(format!("{aid}@{g}: {e}")),
                }
            }
            // create_account 已按目标并发建号,这里兜一道(将来若默认值变了也不会漏)。
            if let Err(e) = self.store.update_account(
                aid,
                &AccountPatch {
                    max_concurrency: Some(p.new_account_concurrency),
                    ..Default::default()
                },
            ) {
                failed.push(format!("{aid} 并发: {e}"));
            }
            if p.new_account_queue_enabled {
                // **必须用 merge 而不是整块替换 extra** —— 后者会把并发进行中的
                // token 刷新写回抹掉。
                let delta = serde_json::json!({ "queue_enabled": true }).to_string();
                if let Err(e) = self.store.merge_account_extra(aid, &delta) {
                    failed.push(format!("{aid} 排队模式: {e}"));
                }
            }
        }

        let _ = self.store.restock_record_owned(order_id, &new_ids);
        let _ = self.store.restock_mark_status(order_id, "imported", &failed.join("; "));
        let _ = self.store.upsert_kv(KEY_FAIL_STREAK, "0");
        self.poke_workers().await;

        let gtxt = groups
            .iter()
            .map(|(g, p)| format!("{g}@{p}"))
            .collect::<Vec<_>>()
            .join(",");
        self.log_msg(
            "import",
            &format!(
                "上号成功: {} → {gtxt}, 并发 {}{}{}",
                new_ids.join(", "),
                p.new_account_concurrency,
                if p.new_account_queue_enabled { "、排队模式" } else { "" },
                if failed.is_empty() {
                    String::new()
                } else {
                    format!(" ⚠ 有 {} 处设置失败", failed.len())
                }
            ),
        );
        tracing::warn!("补货:上号完成 order={order_id} 账号={new_ids:?} 组={gtxt}");
    }

    fn fail_import(&self, p: &Params, order_id: &str, why: &str) {
        // 状态停在 purchased = 钱花了号没进系统,面板会把它单列成孤儿订单。
        let _ = self.store.restock_mark_status(order_id, "purchased", why);
        self.log_msg("error", &format!("导入失败: {why}"));
        tracing::error!("补货:导入失败 order={order_id}: {why} —— key 已存库待人工处理");
        self.maybe_trip(p, why);
    }

    /// 连续「买到却没上号」达阈值即熔断。**钱花了号没进系统是最坏情况,必须刹住。**
    fn maybe_trip(&self, p: &Params, reason: &str) {
        let n: i64 = self
            .store
            .get_kv(KEY_FAIL_STREAK)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
            + 1;
        let _ = self.store.upsert_kv(KEY_FAIL_STREAK, &n.to_string());
        if n >= p.import_fail_breaker {
            let msg = format!("连续 {n} 次失败: {reason}");
            let _ = self.store.upsert_kv(KEY_BREAKER, &msg);
            self.log_msg("error", &format!("熔断触发: {msg}"));
            tracing::error!("补货:熔断触发,已停止自动购买 —— {msg}");
        }
    }

    /// 捅一下各 worker 立即同步账号,否则新号最多 30s 才被加载
    /// (期间验活/改优先级会误报「没有 worker 持有该账号」)。
    async fn poke_workers(&self) {
        let fanout = self.workers.iter().map(|w| {
            let http = self.http.clone();
            let url = format!("http://{}/sync", w.listen);
            async move {
                if let Err(e) = http.post(&url).send().await {
                    tracing::debug!(instance = w.instance, "补货:sync 扇出失败(worker 离线?): {e}");
                }
            }
        });
        futures::future::join_all(fanout).await;
    }

    // ───────────────────────── 启动对账 ─────────────────────────

    /// 处理卡在 `pending` 的订单。
    ///
    /// 对方保证「同 order_id + 同 count 可安全重试」,所以用原 id 重放一次就能问出
    /// 真实结果:若上次其实成功了,这次会返回同一张订单而不会重复扣款。
    pub async fn reconcile_pending(&self) {
        let p = self.params();
        let Ok(pending) = self.store.restock_pending_orders() else {
            return;
        };
        for o in pending {
            tracing::warn!("补货:发现未完成订单 {},用原幂等键重放确认真实结果", o.client_order_id);
            if p.dry_run {
                continue;
            }
            let Ok(st) = self.drop.stock().await else { continue };
            let cap = max_total_cny_for(o.count, st.price_usd, p.rate_cap);
            match self.drop.purchase(o.count, &o.client_order_id, cap).await {
                Ok(res) if !res.keys.is_empty() => {
                    let _ = self.store.restock_mark_purchased(
                        &o.client_order_id,
                        &res.keys,
                        Some(st.balance_cny),
                        res.remaining_cny,
                    );
                    self.onboard(&p, &o.client_order_id, &res.keys).await;
                }
                Ok(_) => {
                    let _ = self.store.restock_mark_status(
                        &o.client_order_id,
                        "failed",
                        "重放未返回 key",
                    );
                }
                Err(e) => {
                    let _ = self.store.restock_mark_status(
                        &o.client_order_id,
                        "failed",
                        &format!("重放失败: {e}"),
                    );
                }
            }
        }
    }

    // ───────────────────────── 回收 ─────────────────────────

    /// 删除本服务买入、已确认死亡且超过 TTL 的账号。
    ///
    /// **只处理自购号** —— 线上那 200+ 人工上的历史死号不归自动化处置。
    pub async fn reclaim(&self) -> usize {
        let p = self.params();
        let Ok(h) = self.health().await else { return 0 };
        if !h.any_online {
            return 0;
        }
        // 复用 health 的口径太粗(它只给计数),这里重新扇出拿到具体 id。
        let dead = match self.dead_account_ids().await {
            Ok(d) => d,
            Err(_) => return 0,
        };
        let ttl = p.grave_ttl_hours.max(1) * 3600;
        let now = now_ts();
        let mut removed = 0;
        let Ok(owned) = self.store.restock_owned_alive() else { return 0 };
        for (aid, created) in owned {
            if !dead.contains(&aid) || now - created < ttl {
                continue;
            }
            match self.store.delete_account(&aid) {
                Ok(_) => {
                    let _ = self.store.restock_mark_reclaimed(&aid);
                    removed += 1;
                    tracing::warn!("补货:回收自购死号 {aid}");
                }
                Err(e) => tracing::warn!("补货:回收 {aid} 失败: {e}"),
            }
        }
        if removed > 0 {
            self.log_msg("reclaim", &format!("回收自购死号 {removed} 个"));
            self.poke_workers().await;
        }
        removed
    }

    async fn dead_account_ids(&self) -> anyhow::Result<std::collections::HashSet<String>> {
        let mut out = std::collections::HashSet::new();
        let fetches = self.workers.iter().map(|w| {
            let http = self.http.clone();
            let url = format!("http://{}/health", w.listen);
            async move { http.get(&url).send().await.ok()?.json::<serde_json::Value>().await.ok() }
        });
        for v in futures::future::join_all(fetches).await.into_iter().flatten() {
            let Some(arr) = v.get("accounts_status").and_then(|a| a.as_array()) else {
                continue;
            };
            for a in arr {
                let reason = a.get("reason").and_then(|x| x.as_str()).unwrap_or("");
                if matches!(reason, "quota_exhausted" | "invalid_refresh_token") {
                    if let Some(id) = a.get("account_id").and_then(|x| x.as_str()) {
                        out.insert(id.to_string());
                    }
                }
            }
        }
        Ok(out)
    }

    // ───────────────────────── 流水 ─────────────────────────

    fn log(&self, action: &str, d: &Decision) {
        let _ = self.store.restock_log_decision(&RestockDecision {
            ts: now_ts(),
            action: action.into(),
            reason: d.reason.clone(),
            healthy: d.healthy,
            stock: d.stock,
            price_usd: d.price_usd,
            balance_cny: d.balance_cny,
            detail: String::new(),
        });
    }

    fn log_msg(&self, action: &str, reason: &str) {
        let _ = self.store.restock_log_decision(&RestockDecision {
            ts: now_ts(),
            action: action.into(),
            reason: reason.into(),
            healthy: None,
            stock: None,
            price_usd: None,
            balance_cny: None,
            detail: String::new(),
        });
    }
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
