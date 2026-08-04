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
    /// **实证还在服务**的 ksk_ 号数。补货水位比的就是它。
    ///
    /// ⚠️ 不能只看 caio 的 `reason == ""`。曾经这么做过,后果是补货在池子全死时
    /// 照样报「还有一个号」拒绝下单 —— 机制见 [`classify`] 的注释。
    pub healthy: i64,
    /// caio 说「正常」但实证已死(窗口内有尝试、零成功)。**只做展示**,
    /// 让人一眼看出「面板上那几个正常号是尸体」。
    pub zombie: i64,
    pub cooling: i64,
    pub dead: i64,
    pub total: i64,
    /// 至少有一个 worker 在线且给出了运行态。为 false 时**不许下单** ——
    /// 读不到健康度就当成 0 个健康号会触发连环购买。
    pub any_online: bool,
    /// 健康号里**最年轻**那个的年龄(秒)。提前量判据用它:取最小值是因为
    /// 只要还有一个新号,就不需要提前买;全都老了才说明整池即将到期。
    pub youngest_healthy_age_secs: Option<i64>,
}

/// worker 运行态里我们关心的三个字段。抽出来是为了让 [`classify`] 不依赖 HTTP。
#[derive(Debug, Clone)]
pub struct RuntimeRow {
    pub account_id: String,
    pub reason: String,
    pub disabled: bool,
}

/// 把「caio 的运行态」与「请求日志里的实证」合成健康度。**纯函数,可直接测。**
///
/// ## 为什么不能只信 `reason`
///
/// `status_snapshot()` 干的第一件事是 `heal_cooldowns()`(见 `worker/scheduler.rs`),
/// 而 `TemporarilySuspended` 被归进「可冷却自愈」类。于是每 `suspended_cooldown_secs`
/// (默认 3600)一到,一个**永久死掉**的号就被复活成 `reason == ""`。
/// 更糟的是:**补货每轮拉 `/health` 这个动作本身就会触发那次复活**,然后把复活的
/// 尸体数进健康号 —— 观测行为改变了被观测量。
///
/// 2026-08-04 实测:22 个成功率 0% 的号各自每 51 分钟被轮到一次(正是 3600s 周期),
/// 11 小时白烧 258 个客户请求;而在全池零成功的那 66 分钟里,引擎有 3 轮报
/// `healthy=1` 拒绝补货。这套判据在 Python 原型里推导正确过,搬进 Rust 时丢了。
///
/// ## 三态判定
///
/// | 窗口内该号的请求记录 | 判定 | 为什么 |
/// |---|---|---|
/// | 有成功 | 活 | 唯一的正面证据 |
/// | 有尝试、零成功 | **僵尸** | caio 说正常但打不通,就是死了 |
/// | 一条都没有 | **不下结论**,维持 caio 的判断 | 没被选中 ≠ 打不通。全局没流量时(夜里、或整个网关刚崩过)所有号都零成功,一律判死会触发连环购买 |
///
/// 新号(建号不足 `new_account_grace_secs`)直接算健康:它还没来得及跑请求。
pub fn classify(
    rows: &[RuntimeRow],
    activity: &std::collections::HashMap<String, (i64, i64)>,
    created_at: &std::collections::HashMap<String, i64>,
    p: &Params,
    now: i64,
) -> Health {
    let mut h = Health { total: created_at.len() as i64, ..Default::default() };
    let mut seen = std::collections::HashSet::new();
    for r in rows {
        if !created_at.contains_key(&r.account_id) || !seen.insert(r.account_id.clone()) {
            continue;
        }
        match r.reason.as_str() {
            "" if !r.disabled => {
                let age = created_at.get(&r.account_id).map(|c| now - c).unwrap_or(0);
                let fresh = age < p.new_account_grace_secs.max(0);
                let alive = match activity.get(&r.account_id) {
                    _ if fresh => true,
                    Some(&(_, ok)) if ok > 0 => true,
                    // 有尝试、零成功 —— caio 说正常,请求说打不通。信请求。
                    Some(_) => false,
                    // 窗口内根本没被选中:证明不了任何事,维持 caio 的判断。
                    None => true,
                };
                if alive {
                    h.healthy += 1;
                    h.youngest_healthy_age_secs =
                        Some(h.youngest_healthy_age_secs.map_or(age, |m| m.min(age)));
                } else {
                    h.zombie += 1;
                }
            }
            // quota_exhausted / invalid_refresh_token 是持久禁用;其余(限流/临时封禁/
            // 空响应/连败)caio 归为会自愈的冷却 —— 但 temporarily_suspended 实测从不
            // 自愈(0/35),所以它其实也是死号,回收逻辑另行处理,这里只做展示区分。
            "quota_exhausted" | "invalid_refresh_token" => h.dead += 1,
            _ => h.cooling += 1,
        }
    }
    h
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

    /// 逐 worker 拉 `/health`,与 DB 里的 ksk_ 账号集求交,再叠上请求日志的实证,
    /// 数出健康度。判定规则见 [`classify`]。
    ///
    /// 运行态(冷却/封禁/在途并发)只存在于 worker 内存,router 侧必须扇出去问。
    pub async fn health(&self) -> anyhow::Result<Health> {
        let p = self.params();
        let now = now_ts();
        let created: std::collections::HashMap<String, i64> =
            self.store.restock_ksk_accounts()?.into_iter().collect();
        let activity = self
            .store
            .restock_account_activity(now - p.liveness_window_secs.max(60))
            .unwrap_or_default();
        let (rows, any_online) = self.runtime_rows().await;
        let mut h = classify(&rows, &activity, &created, &p, now);
        h.any_online = any_online;
        Ok(h)
    }

    /// 扇出各 worker 的 `/health`,摊平成 [`RuntimeRow`]。
    /// 第二个返回值 = 至少有一个 worker 给出了**可用的**运行态(为 false 时**绝不许下单**)。
    async fn runtime_rows(&self) -> (Vec<RuntimeRow>, bool) {
        let fetches = self.workers.iter().map(|w| {
            let http = self.http.clone();
            let url = format!("http://{}/health", w.listen);
            async move { http.get(&url).send().await.ok()?.json::<serde_json::Value>().await.ok() }
        });
        let bodies: Vec<serde_json::Value> =
            futures::future::join_all(fetches).await.into_iter().flatten().collect();
        parse_runtime(&bodies)
    }

    /// 当前需求速率(积分/时)= `max(最近窗口实测, 预测下一小时)`。
    ///
    /// 取 max 而不是只用其一,两边各防一件事:
    /// - **实测**应对突发(画像里没有的流量高峰),也应对客户作息刚变。
    /// - **预测**防自锁:整个网关刚崩过时实测会是 0,只看实测就永远不买号,
    ///   而那恰恰是最需要补货的时刻。
    ///
    /// 口径是**全池**(含贵号池)—— 问的是「有多少活儿等着干」,不是「便宜号干了多少」。
    fn demand_rate(&self, p: &Params, now: i64) -> f64 {
        let measured = self
            .store
            .restock_recent_credit_rate(p.demand_window_secs)
            .unwrap_or(0.0);
        let predicted = (|| {
            let cur_hour = now - now.rem_euclid(3600);
            let hist: Vec<(i64, f64, f64)> = self
                .store
                .restock_credit_hours(0)
                .ok()?
                .into_iter()
                .filter(|(t, _, _)| *t < cur_hour)
                .collect();
            // 拿三五个样本猜出来的画像不配参与决策。
            if hist.len() < 24 {
                return None;
            }
            let pts = forecast::forecast(&hist, p.utc_offset_secs(), cur_hour + 3600, 1);
            pts.first().map(|x| x.credits)
        })()
        .unwrap_or(0.0);
        measured.max(predicted)
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

        let p = self.params();
        if let Ok(h) = self.health().await {
            snap["healthy"] = h.healthy.into();
            snap["zombie"] = h.zombie.into();
            snap["cooling"] = h.cooling.into();
            snap["dead"] = h.dead.into();
            snap["total"] = h.total.into();
            snap["any_online"] = h.any_online.into();
            snap["at"] = now.into();
        }
        // 需求与预期单位成本:面板上这两个数才解释得了「为什么现在不买」。
        let demand = self.demand_rate(&p, now);
        snap["demand_rate"] = ((demand * 10.0).round() / 10.0).into();
        let unit = p.expected_unit_cost(self.cached_price_cny(&p), demand);
        snap["expected_unit_cost"] = if unit.is_finite() {
            ((unit * 10000.0).round() / 10000.0).into()
        } else {
            serde_json::Value::Null
        };
        // 实测寿命中位数,**只展示不自动生效**:这个值估短了就是每轮提前下单、花费翻倍,
        // 该由人看过再决定要不要调 expected_lifetime_secs。
        if let Ok(mut v) = self.store.restock_measured_lifetimes(20, 1800) {
            if !v.is_empty() {
                v.sort_unstable();
                snap["measured_lifetime_secs"] = v[v.len() / 2].into();
                snap["measured_lifetime_samples"] = v.len().into();
            }
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
                    // 成功了就得把旧报错抹掉,否则面板会一直挂着 drop_ok=true 加一条
                    // 早就修好的错误 —— 看的人只能靠猜哪个是真的。
                    snap["drop_error"] = serde_json::Value::Null;
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
        //
        // ⚠️ 顺序:健康度必须算在所有「可能跳过」的闸门**之前**,这样每一条 skip 流水
        // 都带着当时的水位。原先高峰窗口排在它前面直接 return,于是整夜 419 轮流水
        // 全是 healthy=NULL —— 事后根本查不出「那 7 小时池子到底是空是满」。
        let health = match self.health().await {
            Ok(h) if h.any_online => h,
            Ok(_) => return Decision::skip("没有 worker 在线,读不到运行态,本轮跳过"),
            Err(e) => return Decision::skip(format!("读取健康度失败,本轮跳过: {e}")),
        };
        let zomb = if health.zombie > 0 {
            format!(",另有 {} 个 caio 报正常但实证已死", health.zombie)
        } else {
            String::new()
        };

        // ── 硬禁买时段(可选)。默认 start == end = 全天允许,由下面的单位成本闸
        //    负责「什么时候买划算」—— 钟点表写死的窗口会在客户作息变化后悄悄失准,
        //    而且它拦不住「窗口内但没需求」,也放不过「窗口外但正断供」。 ──
        if !force && !p.in_peak_window(now) {
            return Decision {
                healthy: Some(health.healthy),
                ..Decision::skip(format!(
                    "不在允许购买的时段 {}-{}(UTC{:+});当前存活 {}{zomb}",
                    p.peak_start,
                    p.peak_end,
                    p.utc_offset_minutes / 60,
                    health.healthy,
                ))
            };
        }

        // ── 水位。`lead_time_secs > 0` 时,活号快到期也算破水位(默认 0 = 关)。──
        let due = p.lead_time_secs > 0
            && health.youngest_healthy_age_secs.is_some_and(|age| {
                age >= p.expected_lifetime_secs.saturating_sub(p.lead_time_secs)
            });
        if !force && health.healthy >= p.min_healthy && !due {
            return Decision {
                healthy: Some(health.healthy),
                ..Decision::skip(format!(
                    "存活 ksk_ 号 {} 个 ≥ 阈值 {},无需补货(冷却 {},真死 {}{zomb})",
                    health.healthy, p.min_healthy, health.cooling, health.dead
                ))
            };
        }

        // ── 单位成本闸:这单划不划算 ──
        //
        // 实测结论:号按**墙上时钟**死(烧速差 3 倍、存活一律 0.7–0.9 小时),所以
        // 一个号的产出 = 需求速率 × 寿命,与号本身无关 —— 「什么时候买」就是全部策略。
        // 需求不够时买号 = 花 ¥20 买 45 分钟只用掉三成,单位成本反而高过贵号池。
        //
        // 价格取**快照缓存**而不是现打 drop:夜里水位常年破,那会变成整夜每 30 秒
        // 轮询对方接口。读不到就用限价兜底(价格取高 = 门槛更严 = 宁可不买)。
        let demand = self.demand_rate(&p, now);
        if !force {
            if let Some(why) = unit_cost_veto(&p, self.cached_price_cny(&p), demand) {
                return Decision { healthy: Some(health.healthy), ..Decision::skip(why) };
            }
        }

        // ── 闲时抑制(相对判据,默认关)。与上面的绝对判据并存时更保守。
        //    只会阻止购买、永不促成购买,所以预测错了最坏也只是晚买。──
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

        // 拿到真实报价后再复核一次单位成本。上面那次用的是快照缓存价(最多陈旧 300s),
        // 对方涨价时会偏乐观 —— 掏钱之前必须按**这一单的实际价**再算一遍。
        if !force {
            if let Some(why) = unit_cost_veto(&p, need / count.max(1) as f64, demand) {
                return Decision { reason: why, ..base };
            }
        }

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
        } else if health.healthy < p.min_healthy {
            format!(
                "存活 ksk_ 号 {} < 阈值 {}(需求 {demand:.0} 分/时)",
                health.healthy, p.min_healthy
            )
        } else {
            format!(
                "活号已存活 {} 分钟、接近预期寿命 {} 分钟,提前补位",
                health.youngest_healthy_age_secs.unwrap_or(0) / 60,
                p.expected_lifetime_secs / 60
            )
        };
        Decision {
            act: true,
            reason: format!("{trigger},库存 {},买 {count} 个(限价 ¥{need:.2})", st.stock),
            ..base
        }
    }

    /// 快照里缓存的「买一个号大约要花多少钱(¥)」。
    ///
    /// 单位成本闸必须排在问库存**之前** —— 池子空着的时候水位每轮都破,
    /// 若为了拿价格而每轮现打 drop,夜里就变成整夜每 30 秒轮询对方接口。
    /// 快照由 `refresh_snapshot` 每 300s 刷一次,足够做「划不划算」的量级判断。
    ///
    /// 读不到就用限价兜底:价格取高 → 门槛更严 → 宁可不买。方向必须是 fail-closed,
    /// 反过来会在快照缺失时放行一堆亏本单。
    fn cached_price_cny(&self, p: &Params) -> f64 {
        let cached = self
            .store
            .get_kv(KEY_SNAPSHOT)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .and_then(|v| v.get("price_usd").and_then(|x| x.as_f64()))
            .filter(|x| *x > 0.0)
            .unwrap_or(p.max_price_usd);
        max_total_cny_for(1, cached, p.rate_cap)
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

    /// 已确认死亡的账号 id。
    ///
    /// ⚠️ 原先只认 `quota_exhausted | invalid_refresh_token`,**恰好漏掉了实际死法**:
    /// 这批号绝大多数死于 403 `TEMPORARILY_SUSPENDED`,caio 把它归为「会自愈的冷却」,
    /// 于是永远不进回收名单。2026-08-04 实测后果:22 个成功率 0% 的号赖在池子里,
    /// 每小时各被轮到一次(3600s 冷却到期即复活),11 小时白烧 258 个客户请求。
    ///
    /// 所以 `temporarily_suspended` 也要收,但**必须叠加实证**(近 `SUSPENDED_DEAD_WINDOW`
    /// 内有尝试且零成功)—— 光凭 reason 会误伤刚撞上一次限流、其实还能用的号。
    /// 号龄与「只动自购号」的判断留在 `reclaim` 里,与原设计一致。
    async fn dead_account_ids(&self) -> anyhow::Result<std::collections::HashSet<String>> {
        const SUSPENDED_DEAD_WINDOW: i64 = 6 * 3600;
        let activity = self
            .store
            .restock_account_activity(now_ts() - SUSPENDED_DEAD_WINDOW)
            .unwrap_or_default();
        let (rows, _) = self.runtime_rows().await;
        let mut out = std::collections::HashSet::new();
        for r in rows {
            let dead = match r.reason.as_str() {
                "quota_exhausted" | "invalid_refresh_token" => true,
                "temporarily_suspended" => {
                    matches!(activity.get(&r.account_id), Some(&(n, 0)) if n > 0)
                }
                _ => false,
            };
            if dead {
                out.insert(r.account_id);
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

/// 把各 worker `/health` 的响应体摊平成 [`RuntimeRow`],并判定「运行态是否可用」。
/// **纯函数,可直接测。**
///
/// ⚠️ `any_online` 必须以「**拿到了 `accounts_status` 数组**」为准,不能只看「响应能解析
/// 成 JSON」。后者会把「worker 返回了别的 JSON」(端口配错打到别的服务、反代的 JSON 错误页、
/// 将来字段改名)当成在线,于是运行态是空的 → `healthy = 0` → 每轮都判定池子空了 →
/// **一路买到日预算打满**。这条保护的整个意义就是「读不到运行态就别花钱」,
/// 判据松一格就等于没有。空数组是合法的(该 worker 名下没有账号),照样算在线。
pub fn parse_runtime(bodies: &[serde_json::Value]) -> (Vec<RuntimeRow>, bool) {
    let mut out = Vec::new();
    let mut any_online = false;
    for v in bodies {
        let Some(arr) = v.get("accounts_status").and_then(|a| a.as_array()) else {
            continue;
        };
        any_online = true;
        for a in arr {
            let Some(id) = a.get("account_id").and_then(|x| x.as_str()) else {
                continue;
            };
            out.push(RuntimeRow {
                account_id: id.to_string(),
                reason: a.get("reason").and_then(|x| x.as_str()).unwrap_or("").to_string(),
                disabled: a.get("disabled").and_then(|x| x.as_bool()).unwrap_or(false),
            });
        }
    }
    (out, any_online)
}

/// 单位成本闸:这单划不划算。返回 `Some(理由)` = 该拦。**纯函数,可直接测。**
///
/// 实测的决定性事实(2026-08-04,22 个自购号的全生命周期):号按**墙上时钟**死 ——
/// 烧速差 3 倍(676 vs 1990 分/时),存活一律 0.7–0.9 小时,烧得最猛的反而活得最久。
/// 于是一个号的产出 = 需求速率 × 寿命,**与号本身无关**,「什么时候买」就是全部策略:
///
/// * 需求够时:并发拉满、榨得越快越好,省着烧的部分到点直接蒸发;
/// * 需求不够时:花 ¥20 买 45 分钟只用掉三成,单位成本反而高过它要替代的贵号池。
///
/// 用「¥/积分」而不是「积分/时的阈值」做旋钮,是为了让它随价格自适应:
/// drop 从 $2.95 降到 $2.20 时,可接受的需求门槛自动跟着降,不用人去改配置。
pub fn unit_cost_veto(p: &Params, price_cny: f64, demand_rate: f64) -> Option<String> {
    if p.max_unit_cost_cny_per_credit <= 0.0 {
        return None; // 0 = 关闭这道闸
    }
    let unit = p.expected_unit_cost(price_cny, demand_rate);
    if unit <= p.max_unit_cost_cny_per_credit {
        return None;
    }
    Some(format!(
        "需求撑不起这一单:近期 {demand_rate:.0} 分/时 × 预期寿命 {:.0} 分钟 ≈ 产出 {:.0} 分,\
         单价 ¥{price_cny:.2} → 预期 ¥{unit:.4}/分,高于上限 ¥{:.4}/分。\
         号是按时钟死的,买了闲置就是纯亏,等有需求再补",
        p.expected_lifetime_secs as f64 / 60.0,
        p.expected_yield(demand_rate),
        p.max_unit_cost_cny_per_credit,
    ))
}

pub fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const NOW: i64 = 1_785_800_000;

    fn row(id: &str, reason: &str, disabled: bool) -> RuntimeRow {
        RuntimeRow { account_id: id.into(), reason: reason.into(), disabled }
    }

    /// 全部号都是「很久以前建的」,以便默认落在宽限期之外。
    fn born_long_ago(ids: &[&str]) -> HashMap<String, i64> {
        ids.iter().map(|i| ((*i).to_string(), NOW - 10_000)).collect()
    }

    #[test]
    fn 有尝试零成功的号不算健康() {
        // caio 报 reason="" ,但窗口内 12 次请求一次没成 —— 这就是尸体。
        let act: HashMap<String, (i64, i64)> = [("a".to_string(), (12, 0))].into();
        let h = classify(
            &[row("a", "", false)],
            &act,
            &born_long_ago(&["a"]),
            &Params::default(),
            NOW,
        );
        assert_eq!(h.healthy, 0, "打不通的号绝不能计入水位");
        assert_eq!(h.zombie, 1);
    }

    #[test]
    fn 窗口内没被选中时维持caio的判断() {
        // 夜里没流量 / 整个网关刚崩过:所有号都零成功。此时「零成功」什么也证明不了,
        // 一律判死会让补货每轮都以为池子空了,连环购买。
        let h = classify(
            &[row("a", "", false)],
            &HashMap::new(),
            &born_long_ago(&["a"]),
            &Params::default(),
            NOW,
        );
        assert_eq!(h.healthy, 1, "没有证据时不许下结论");
        assert_eq!(h.zombie, 0);
    }

    #[test]
    fn 新号在宽限期内算健康() {
        // 刚上号还没跑过任何请求;没有宽限期的话它会被自己判成僵尸,于是不停买。
        let mut p = Params::default();
        p.new_account_grace_secs = 300;
        let created: HashMap<String, i64> = [("a".to_string(), NOW - 60)].into();
        let act: HashMap<String, (i64, i64)> = [("a".to_string(), (3, 0))].into();
        let h = classify(&[row("a", "", false)], &act, &created, &p, NOW);
        assert_eq!(h.healthy, 1, "宽限期内即使零成功也算健康");

        // 宽限期一过,同样的数据就该判死。
        let old: HashMap<String, i64> = [("a".to_string(), NOW - 600)].into();
        let h2 = classify(&[row("a", "", false)], &act, &old, &p, NOW);
        assert_eq!(h2.healthy, 0);
        assert_eq!(h2.zombie, 1);
    }

    /// 2026-08-04 的生产现场:池子里 1 个 reason="" 的尸体 + 一堆冷却号,
    /// 而全池零成功。旧判据在这里报 `healthy=1` 拒绝补货,断供 66 分钟。
    #[test]
    fn 生产回归_全池零成功时水位必须是零() {
        let ids = ["z", "c1", "c2", "d1"];
        let rows = [
            row("z", "", false),                        // 复活的尸体
            row("c1", "temporarily_suspended", true),
            row("c2", "temporarily_suspended", true),
            row("d1", "invalid_refresh_token", true),
        ];
        let act: HashMap<String, (i64, i64)> = [("z".to_string(), (5, 0))].into();
        let h = classify(&rows, &act, &born_long_ago(&ids), &Params::default(), NOW);
        assert_eq!(h.healthy, 0, "全池零成功时说「还有一个号」正是要修掉的 bug");
        assert_eq!(h.zombie, 1);
        assert_eq!(h.cooling, 2);
        assert_eq!(h.dead, 1);
        assert_eq!(h.total, 4);
    }

    #[test]
    fn 非ksk号与重复上报都不计入() {
        let act: HashMap<String, (i64, i64)> = [("a".to_string(), (1, 1))].into();
        // "x" 不在 created_at(= 不是 ksk_ 号);"a" 在两个 worker 上各报一次。
        let h = classify(
            &[row("a", "", false), row("a", "", false), row("x", "", false)],
            &act,
            &born_long_ago(&["a"]),
            &Params::default(),
            NOW,
        );
        assert_eq!(h.healthy, 1);
        assert_eq!(h.total, 1);
    }

    #[test]
    fn 最年轻活号的年龄取最小值() {
        // 只要还有一个新号就不该提前补位,所以取 min 而不是 max。
        let created: HashMap<String, i64> =
            [("old".to_string(), NOW - 2600), ("new".to_string(), NOW - 100)].into();
        let act: HashMap<String, (i64, i64)> =
            [("old".to_string(), (9, 9)), ("new".to_string(), (9, 9))].into();
        let h = classify(
            &[row("old", "", false), row("new", "", false)],
            &act,
            &created,
            &Params::default(),
            NOW,
        );
        assert_eq!(h.healthy, 2);
        assert_eq!(h.youngest_healthy_age_secs, Some(100));
    }

    // ───────────────────── 运行态解析 ─────────────────────

    /// **红线**:worker 回了别的 JSON(端口配错、反代错误页、字段改名)时,
    /// 绝不能算「在线」。算在线 = 运行态为空 = `healthy=0` = 每轮都以为池子空了,
    /// 一路买到日预算打满(¥800)。这条保护的全部意义就是「读不到运行态就别花钱」。
    #[test]
    fn 形状不对的health响应不算在线() {
        let bad = serde_json::json!({"status": "ok", "role": "worker"}); // 没有 accounts_status
        let (rows, online) = parse_runtime(&[bad]);
        assert!(rows.is_empty());
        assert!(!online, "拿不到账号运行态就必须判定离线,否则会一路买到预算打满");

        let err_page = serde_json::json!({"error": "bad gateway"});
        assert!(!parse_runtime(&[err_page]).1);
    }

    #[test]
    fn 没有账号的worker仍然算在线() {
        // 空数组是合法状态(该 worker 名下没有账号),不能因此判定整个运行态不可读。
        let (rows, online) = parse_runtime(&[serde_json::json!({"accounts_status": []})]);
        assert!(rows.is_empty());
        assert!(online);
    }

    #[test]
    fn 一个worker坏掉不影响另一个() {
        let bodies = [
            serde_json::json!({"oops": 1}),
            serde_json::json!({"accounts_status": [
                {"account_id": "a", "reason": "", "disabled": false},
                {"reason": "", "disabled": false},               // 缺 id,跳过
                {"account_id": "b", "reason": "temporarily_suspended", "disabled": true},
            ]}),
        ];
        let (rows, online) = parse_runtime(&bodies);
        assert!(online);
        assert_eq!(rows.len(), 2, "缺 account_id 的条目要跳过,其余照收");
        assert_eq!(rows[1].reason, "temporarily_suspended");
        assert!(rows[1].disabled);
    }

    // ───────────────────── 单位成本闸 ─────────────────────

    fn econ_params() -> Params {
        let mut p = Params::default();
        p.expected_lifetime_secs = 2700; // 45 分钟
        p.account_throughput_credits_per_hour = 1900.0;
        p.max_unit_cost_cny_per_credit = 0.04;
        p
    }

    #[test]
    fn 低谷时段不许买号() {
        // 凌晨 6 点实测 77 分/时:一个 ¥20 的号只能榨出约 58 分 → ¥0.35/分,
        // 比它要替代的贵号池(约 ¥0.068/分)还贵 5 倍。
        let p = econ_params();
        let why = unit_cost_veto(&p, 20.06, 77.0).expect("低谷必须拦住");
        assert!(why.contains("需求撑不起"), "理由要说人话:{why}");
    }

    #[test]
    fn 需求足够时放行() {
        // 白天 1000 分/时 → 产出 750 分 → ¥0.027/分,低于上限。
        assert!(unit_cost_veto(&econ_params(), 20.06, 1000.0).is_none());
    }

    #[test]
    fn 零需求必须被拦而不是当成算不出来() {
        // 预期产出为 0 时单位成本是 INFINITY。若图省事返回 None(「算不出来就放行」),
        // 就会在完全没人用的时候照买不误。
        assert!(unit_cost_veto(&econ_params(), 20.06, 0.0).is_some());
    }

    #[test]
    fn 阈值设零即关闭这道闸() {
        let mut p = econ_params();
        p.max_unit_cost_cny_per_credit = 0.0;
        assert!(unit_cost_veto(&p, 20.06, 0.0).is_none(), "0 = 显式关闭,不该再拦");
    }

    #[test]
    fn 降价会自动放宽需求门槛() {
        // 这是用「¥/积分」而不是「积分/时」当旋钮的理由:同样的需求,
        // 号便宜了就该买,不用人去改配置。
        let p = econ_params();
        let demand = 700.0;
        assert!(unit_cost_veto(&p, 21.24, demand).is_some(), "$2.95 时这个需求不划算");
        assert!(unit_cost_veto(&p, 15.84, demand).is_none(), "$2.20 时同样的需求就划算了");
    }
}
