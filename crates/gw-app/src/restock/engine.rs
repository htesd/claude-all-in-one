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

use super::drop::{max_total_cny_for, new_order_id, BoughtKey};
use super::forecast;
use super::notify;
use super::params::Params;
use super::registry::{self, SupplierCfg};
use super::supplier::{rank_shelves, BuyOutcome, Shelf, Supplier, Survey};

/// 面板快照在 `settings` 表里的键名。轮询线程写、admin 读。
pub const KEY_SNAPSHOT: &str = "restock_snapshot";
/// 熔断状态(非空即熔断,内容是原因)。
pub const KEY_BREAKER: &str = "restock_breaker";
/// 连续「买到却没上号」的次数。
pub const KEY_FAIL_STREAK: &str = "restock_import_fail_streak";
/// 抢货实况 `{since, probes}`,非抢货期间为空串。
///
/// 落库而不是只留在循环的内存里,是因为**面板与循环不在同一个执行体**:
/// admin 读不到后台任务的局部变量。而抢货期间恰恰是**不写决策流水**的
/// (见 `run_once_opts`),没有这个键的话面板上会连着几小时一动不动 ——
/// 那正是 2026-08-03 那次「功能做了但用户从来没看到」的同一种失败。
pub const KEY_HUNT: &str = "restock_hunt";

/// 一轮决策的结论。
#[derive(Debug, Clone, Default)]
pub struct Decision {
    pub act: bool,
    pub reason: String,
    pub healthy: Option<i64>,
    pub stock: Option<i64>,
    /// 决策流水的历史列,单位 **USD**(为了不改已有的表与图表口径而保留)。
    /// 多供应商下它装的是**选中货架**的单价折回美元,没选中货架时为空。
    pub price_usd: Option<f64>,
    pub balance_cny: Option<f64>,
    /// 选中的货架与本单参数。`act == true` 时必然是 `Some`。
    pub candidate: Option<Candidate>,
    /// **闸门全过、就差有货**。抢货模式的唯一触发条件。
    ///
    /// 必须是结构化字段而不是拿 `reason` 去匹配字符串:那句话是给人看的,
    /// 改一个字就会让提速悄悄失效,而失效的表现是「一切正常,只是又没抢到」。
    ///
    /// 与「有货但过不了闸门」严格区分 —— 后者提速毫无意义(闸门不会因为多问几次
    /// 就放行),只会白白增加对方的请求密度。
    pub out_of_stock: bool,
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

/// 花钱临界区锁的 TTL(秒)。
///
/// 必须**长于**一次购买往返(实测 3.4–4.2s,客户端超时 20s),又必须**短于**单轮外层
/// 超时(120s)—— 前者保证锁不会在请求还在飞的时候过期放第二个人进来,
/// 后者保证持有者被掐断/SIGKILL 时锁能自己过期,不会把补货永久锁死。
const PURCHASE_LOCK_TTL_SECS: i64 = 90;

/// 对账时「多久以前的在途订单才敢碰」。见 `restock_pending_orders` 的文档:
/// 必须大于单轮外层超时(120s),否则会重放**别人正在途中**的那一单。
const RECONCILE_MIN_AGE_SECS: i64 = 300;

pub struct Engine {
    pub store: Arc<SqliteStore>,
    /// 已启用且配置完整的货源。**顺序无关** —— 选家看的是名册里的档位与价格,
    /// 不是这个 Vec 的下标,见 [`choose_shelf`]。
    pub suppliers: Vec<Arc<dyn Supplier>>,
    /// 名册原文。客户端里查不到每家的日上限与启用状态,那些只在配置里。
    pub roster: Vec<SupplierCfg>,
    pub workers: Arc<Vec<WorkerConfig>>,
    /// 打 worker loopback `/health` 与 `/sync` 用。短超时:worker 离线要快速跳过。
    pub http: reqwest::Client,
    /// 本执行体在花钱锁里的身份。后台循环与 `POST /restock/buy-now` 各自构造 Engine,
    /// 但**共用同一把锁**,所以这个值必须每个执行体唯一。
    pub holder: String,
}

/// 一个通过了全部花钱闸门的候选货架。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub shelf: Shelf,
    /// 这家此刻的余额(CNY),进决策流水。
    pub balance_cny: f64,
    /// 本单买几个。
    pub count: i64,
    /// 本单限价(CNY)。
    pub need_cny: f64,
}

/// **多供应商的唯一调度判断:买哪个货架。**
///
/// 规则一句话:**能买的里面档位最高的那个,同档比价**。
///
/// 为什么不是纯比价(2026-08-05 修正):上线时的规则是「能买的里面最便宜的那个」,
/// 理由写的是「实测两家的号价值等价」。那个前提**被证伪了** —— 同为 KIRO POWER,
/// drop 侧 29 个号 0 次 `temporarily_suspended`,kiroapp/eu 侧 12 个号 12 次,
/// 其中一个零成功请求即被封。号的封禁率无法观测、也无法编码进单价,只能由人按
/// 观察结果表达成档位([`SupplierCfg::priority`] / `shelf_priority`)。
///
/// 档位是**软优先**:首选档缺货或过不了闸门时自动落到下一档,绝不会因为首选家没货
/// 就停止补货 —— drop 常年 0 库存,硬绑定等于亲手制造多供应商要消除的断供。
/// 全部缺省(0)时退化成纯比价,与本次改动前逐字节同序。
///
/// 仍然没有的东西:轮转、粘性、按质量自动加权。档位是**人写的**,因为「这家的号能不能用」
/// 目前只能靠人看封禁数据判断,自动加权会把一次抽样噪声放大成长期偏置。
///
/// 纯函数,不碰网络与 DB:这是整套多供应商里唯一真正花钱的判断,它必须能被钉死在
/// 单元测试里,而不是只能在生产上观察。
///
/// 返回 `Err` 时带**逐个货架被否的理由** —— 面板上「为什么这轮没买」必须答得出来,
/// 只说一句「没有合适的货源」等于让人去翻日志。
pub fn choose_shelf(
    p: &Params,
    surveys: &[Survey],
    blocked: &std::collections::HashMap<String, String>,
    caps: &std::collections::HashMap<String, (f64, f64)>,
    spent_today: f64,
    demand_rate: f64,
    force: bool,
) -> Result<Candidate, String> {
    let balances: std::collections::HashMap<&str, f64> =
        surveys.iter().map(|s| (s.supplier_id.as_str(), s.balance_cny)).collect();
    let ranked = rank_shelves(surveys.iter().flat_map(|s| s.shelves.clone()).collect());
    if ranked.is_empty() {
        return Err("所有货源都没有库存".into());
    }
    // 单价上限的参数是 USD,货架是 CNY —— 在**同一处**折算,不要让两种单位在闸门里并存。
    let max_price_cny = max_total_cny_for(1, p.max_price_usd, p.rate_cap);
    let mut whys: Vec<String> = Vec::new();

    for shelf in ranked {
        let label = shelf.label();
        if let Some(why) = blocked.get(&shelf.supplier_id) {
            whys.push(format!("{label} 被跳过({why})"));
            continue;
        }
        if shelf.unit_price_cny > max_price_cny {
            whys.push(format!(
                "{label} 单价 ¥{:.2} 高于上限 ¥{max_price_cny:.2}",
                shelf.unit_price_cny
            ));
            continue;
        }
        // 三个量闸取最小:我方意愿、对方库存、对方单笔上限。
        // ⚠️ 对方的上限**不能靠它自己执行** —— kiroapp 实测对超限的 count 是 clamp
        // 并成交(发 99 买走 10 个)。所以这里必须我方先夹好再发。
        let count = p
            .max_per_purchase
            .min(shelf.stock)
            .min(shelf.max_per_order.max(1))
            .max(1);
        // 单价已含限价汇率与向上取整,乘 count 即本单限价(count=1 时与
        // `max_total_cny_for(count, ..)` 逐字节相同;count>1 时略高,方向安全)。
        let need = shelf.unit_price_cny * count as f64;

        if !force {
            if let Some(why) = unit_cost_veto(p, shelf.unit_price_cny, demand_rate) {
                whys.push(format!("{label} {why}"));
                continue;
            }
        }
        let balance = balances.get(shelf.supplier_id.as_str()).copied().unwrap_or(0.0);
        if balance < need + p.min_balance_reserve_cny {
            whys.push(format!(
                "{label} 余额 ¥{balance:.2} 不足(需 ¥{need:.2} + 保留 ¥{:.2})",
                p.min_balance_reserve_cny
            ));
            continue;
        }
        if spent_today + need > p.daily_cap_cny {
            whys.push(format!(
                "{label} 本单 ¥{need:.2} 会突破日上限(已花 ¥{spent_today:.2} / ¥{:.2})",
                p.daily_cap_cny
            ));
            continue;
        }
        // ── 花钱闸门:本单会不会突破**这一家**的日上限 ──
        //
        // 必须把 `need` 算进去。只在「已花 >= 上限」时屏蔽是拦不住的:上限 ¥20、
        // 已花 ¥19 时这家看着还没到顶,一单 ¥15 下去就是 ¥34 —— 单家敞口上限名存实亡。
        // 而单家上限存在的全部意义,就是限制一家出问题时能从我这里拿走多少钱。
        if let Some(&(sup_spent, sup_cap)) = caps.get(&shelf.supplier_id) {
            if sup_cap > 0.0 && sup_spent + need > sup_cap {
                whys.push(format!(
                    "{label} 本单 ¥{need:.2} 会突破本家上限(已花 ¥{sup_spent:.2} / ¥{sup_cap:.2})"
                ));
                continue;
            }
        }
        return Ok(Candidate { shelf, balance_cny: balance, count, need_cny: need });
    }
    Err(whys.join(";"))
}

/// 花钱临界区的 RAII 守卫:`drop` 时归还锁。
///
/// 不用手工 `release` 是因为临界区里有十几条 early return(预算不足、库存查询失败、
/// 单价超限……),漏掉任何一条都会把锁一直押到 TTL 过期,而那 90 秒里谁都买不了号。
struct PurchaseGuard<'a> {
    store: &'a SqliteStore,
    holder: &'a str,
}

impl Drop for PurchaseGuard<'_> {
    fn drop(&mut self) {
        let _ = self
            .store
            .release_lock(SqliteStore::KEY_RESTOCK_PURCHASE_LOCK, self.holder);
    }
}

impl Engine {
    /// 进入花钱临界区。拿不到 = 别的执行体正在花钱,本轮直接放弃。
    ///
    /// **`buy-now` 与后台循环都必须过这道门。** 在此之前 `buy-now` 不受任何互斥保护
    /// (它连 leader 租约都不抢),手动点两次就能让两个执行读到同一个 `spent`、
    /// 各自下单扣款。
    fn enter_purchase_section(&self) -> Option<PurchaseGuard<'_>> {
        match self.store.try_acquire_lock(
            SqliteStore::KEY_RESTOCK_PURCHASE_LOCK,
            &self.holder,
            PURCHASE_LOCK_TTL_SECS,
        ) {
            Ok(true) => Some(PurchaseGuard { store: &self.store, holder: &self.holder }),
            Ok(false) => None,
            // 拿锁这件事本身失败(DB 忙/坏)→ **不许花钱**。方向必须 fail-closed。
            Err(e) => {
                tracing::error!("补货:获取花钱锁失败,本轮放弃购买: {e}");
                None
            }
        }
    }

    /// 我此刻是否仍是补货 leader。
    ///
    /// 花钱锁保证「同一时刻只有一个执行体在花钱」,但**保证不了「这个执行体还该不该花」**:
    /// 租约 TTL 最短 30s,而从抢到租约走到真正下单,中间隔着健康度扇出、报价、决策 ——
    /// 完全可能已经被别的 router 接管。那个新 leader 会做自己的决策并买号;
    /// 若旧 leader 恢复后接着买,就是同一个水位缺口被补了两次。
    ///
    /// 读失败按**不持有**处理(fail-closed):读不到就别花钱。
    fn holds_lease(&self) -> bool {
        self.store
            .holds_lock(SqliteStore::KEY_RESTOCK_LEASE, &self.holder)
            .unwrap_or(false)
    }

    /// 按名册建出引擎。名册在 DB 里,面板改完**下一轮就生效**,不用重启。
    ///
    /// `yaml` 是 `system.yaml` 的 restock 段:名册里 drop 那家不填密钥时回落到它,
    /// 所以老部署不需要迁移任何凭据。
    pub fn build(
        store: Arc<SqliteStore>,
        yaml: &gw_core::config::RestockConfig,
        workers: Arc<Vec<WorkerConfig>>,
        http: reqwest::Client,
        holder: String,
    ) -> Self {
        let raw = store.get_kv(registry::KEY_SUPPLIERS).ok().flatten();
        let roster = registry::parse_roster(raw.as_deref());
        let suppliers = registry::build(&roster, yaml.base_url(), &yaml.api_key);
        Self { store, suppliers, roster, workers, http, holder }
    }

    /// 并发问所有货源的报价与余额。
    ///
    /// 并发而不是串行:一家挂掉时串行会把整轮拖满超时,而多供应商存在的全部意义
    /// 就是「一家不行还有另一家」—— 让慢的那家拖死快的那家等于自废武功。
    async fn survey_all(&self, p: &Params) -> (Vec<Survey>, Vec<(String, String)>) {
        let fx = p.rate_cap;
        let calls = self.suppliers.iter().map(|s| {
            let s = s.clone();
            async move { (s.id().to_string(), s.survey(fx).await) }
        });
        let mut ok: Vec<Survey> = Vec::new();
        let mut bad = Vec::new();
        for (id, r) in futures::future::join_all(calls).await {
            match r {
                Ok(v) => ok.push(v),
                Err(e) => {
                    tracing::warn!("补货:询价 {id} 失败: {e}");
                    bad.push((id, e));
                }
            }
        }
        // ── 档位回填 ──
        //
        // 适配器不知道也不该知道自己的档位(一家货源不该自己声明自己有多重要),
        // 所以档位在这里、**在所有排序之前**统一按名册盖上去。
        //
        // 放在 survey_all 内部而不是各个调用点:漏盖一处的后果是那条路径悄悄退回
        // 纯比价 —— 而那正是引入档位要修的行为,漏了不会报错只会静默不生效。
        for s in &mut ok {
            for sh in &mut s.shelves {
                sh.priority = registry::shelf_priority_of(&self.roster, &s.supplier_id, &sh.shelf_id);
            }
        }
        (ok, bad)
    }

    /// 此刻**不许花钱**的货源 → 原因。两类:本家熔断,和本家当日花费已到顶。
    ///
    /// 逐家而不是全局:kiroapp 的 key 失效不该让 drop 也停下 —— 那会亲手制造出
    /// 多供应商本来要消除的断供。
    fn blocked_suppliers(&self, p: &Params, day_start: i64) -> std::collections::HashMap<String, String> {
        let mut out = std::collections::HashMap::new();
        for c in &self.roster {
            let id = c.id.trim();
            if id.is_empty() {
                continue;
            }
            if let Ok(Some(r)) = self.store.get_kv(&registry::breaker_key(id)) {
                if !r.is_empty() {
                    out.insert(id.to_string(), format!("已熔断: {r}"));
                    continue;
                }
            }
            if c.daily_cap_cny > 0.0 {
                let spent = self
                    .store
                    .restock_spent_since_by_supplier(day_start, id)
                    .unwrap_or(0.0);
                if spent >= c.daily_cap_cny {
                    out.insert(
                        id.to_string(),
                        format!("本家今日已花 ¥{spent:.2} 达上限 ¥{:.2}", c.daily_cap_cny),
                    );
                }
            }
        }
        let _ = p;
        out
    }

    /// 每家的 `(今日已花, 本家上限)`。`choose_shelf` 要靠它把**本单**算进单家上限。
    fn supplier_caps(&self, day_start: i64) -> std::collections::HashMap<String, (f64, f64)> {
        self.roster
            .iter()
            .filter(|c| c.daily_cap_cny > 0.0)
            .map(|c| {
                let id = c.id.trim().to_string();
                let spent = self
                    .store
                    .restock_spent_since_by_supplier(day_start, &id)
                    .unwrap_or(f64::INFINITY); // 读不出来就当成已到顶:fail-closed
                (id, (spent, c.daily_cap_cny))
            })
            .collect()
    }

    /// 按 id 找客户端。名册被改小之后,老订单的对账仍然要找得到那一家。
    fn supplier_of(&self, id: &str) -> Option<&Arc<dyn Supplier>> {
        self.suppliers.iter().find(|s| s.id() == id)
    }

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
            let (surveys, failed) = self.survey_all(&p).await;
            let blocked = self.blocked_suppliers(&p, p.local_day_start(now));
            snap["suppliers"] = self.suppliers_view(&p, &surveys, &failed, &blocked, now);
            // ── 顶层的 stock/price_usd/balance_cny 保留,装的是**首选货架**的数 ──
            // 它们是决策链上游(`cached_price_cny`)与历史图表的输入。多供应商之后
            // 「库存/单价/余额」不再是一家的属性,但「下一单会买的那个货架现在什么价」
            // 仍然是单个有意义的数,而且正是成本预测要的那个。
            //
            // ⚠️ 引入档位后这**不再等于最低价**:首选货架可能比末档贵。这是有意的 ——
            // 成本预测要预测的是我们**真的会付**的价,不是市面上最便宜的价。
            // ── 「下一单会买哪个」由 `choose_shelf` 本人回答 ──
            //
            // 以前面板自己按价格排一遍来猜这个答案。排序**能**复刻,闸门复刻不了:
            // 余额、单价上限、`unit_cost_veto`、全局与单家日上限,任何一道否掉首选,
            // 引擎买的就不是面板宣称的那个。面板在这种时刻说谎最贵 —— 值班的人
            // 看到「下一单会买 drop」就不会再去查为什么池子里全是 kiroapp 的号。
            //
            // 所以这里直接调真正的判断函数,把结论(以及被否时的逐货架理由)写进快照,
            // 前端只渲染,不再自己算一遍。
            let day_start = p.local_day_start(now);
            let (spent_today, _) = self.store.restock_spent_since(day_start).unwrap_or((0.0, 0));
            match choose_shelf(
                &p,
                &surveys,
                &blocked,
                &self.supplier_caps(day_start),
                spent_today,
                snap.get("demand_rate").and_then(|v| v.as_f64()).unwrap_or(0.0),
                false,
            ) {
                Ok(c) => {
                    snap["next_pick"] = c.shelf.label().into();
                    snap["next_pick_why"] = serde_json::Value::Null;
                }
                Err(why) => {
                    snap["next_pick"] = serde_json::Value::Null;
                    snap["next_pick_why"] = why.into();
                }
            }
            // 顶层数字仍取**排序第一名**而不是 `next_pick`:闸门是瞬时的(余额、日上限),
            // 让成本预测的输入随闸门开合来回跳会让历史曲线失去可读性。
            // 「首选货架的价」是稳定量,「这一秒买不买得成」是另一件事。
            let best = rank_shelves(surveys.iter().flat_map(|s| s.shelves.clone()).collect())
                .into_iter()
                .next();
            match best {
                Some(b) => {
                    let bal = surveys
                        .iter()
                        .find(|s| s.supplier_id == b.supplier_id)
                        .map(|s| s.balance_cny)
                        .unwrap_or(0.0);
                    snap["stock"] = b.stock.into();
                    // 折回 USD 只为了兼容既有列与图表口径;比价一律用 CNY。
                    snap["price_usd"] = (b.unit_price_cny / p.rate_cap.max(0.01)).into();
                    snap["price_cny"] = b.unit_price_cny.into();
                    snap["balance_cny"] = bal.into();
                    snap["best_shelf"] = b.label().into();
                }
                None => {
                    snap["stock"] = 0.into();
                    snap["best_shelf"] = serde_json::Value::Null;
                }
            }
            // 全池余额:多家之后「我还剩多少钱」不再等于任何单独一家的余额。
            snap["balance_total_cny"] =
                surveys.iter().map(|s| s.balance_cny).sum::<f64>().into();
            snap["stock_at"] = now.into();
            // 至少一家问得通才算「上游正常」。全挂了才是真的没得买。
            snap["drop_ok"] = (!surveys.is_empty()).into();
            snap["drop_error"] = if failed.is_empty() {
                serde_json::Value::Null
            } else {
                failed
                    .iter()
                    .map(|(id, e)| format!("{id}: {e}"))
                    .collect::<Vec<_>>()
                    .join("; ")
                    .into()
            };
        }
        let _ = self
            .store
            .upsert_kv(KEY_SNAPSHOT, &snap.to_string());
    }

    /// 面板要的逐家视图:余额、货架、状态、今日花费。
    ///
    /// 一次算完写进快照,而不是让 admin 每次轮询都去打各家接口 —— 面板 15 秒一次,
    /// 那会变成对上游的持续压测。
    fn suppliers_view(
        &self,
        p: &Params,
        surveys: &[Survey],
        failed: &[(String, String)],
        blocked: &std::collections::HashMap<String, String>,
        now: i64,
    ) -> serde_json::Value {
        let day_start = p.local_day_start(now);
        let items: Vec<serde_json::Value> = self
            .roster
            .iter()
            .map(|c| {
                let id = c.id.trim();
                let sv = surveys.iter().find(|s| s.supplier_id == id);
                let err = failed.iter().find(|(fid, _)| fid == id).map(|(_, e)| e.clone());
                let spent = self.store.restock_spent_since_by_supplier(day_start, id).unwrap_or(0.0);
                let shelves: Vec<serde_json::Value> = sv
                    .map(|s| {
                        s.shelves
                            .iter()
                            .map(|sh| {
                                serde_json::json!({
                                    "shelf": sh.shelf_id,
                                    "label": sh.label(),
                                    "region": sh.account_region,
                                    "stock": sh.stock,
                                    "unit_price_cny":
                                        (sh.unit_price_cny * 100.0).round() / 100.0,
                                    "max_per_order": sh.max_per_order,
                                    // **生效**档位(已含逐货架覆盖)。面板显示它是覆盖键
                                    // 写错时的唯一发现途径 —— 见 SupplierCfg::shelf_priority。
                                    "priority": sh.priority,
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                serde_json::json!({
                    "id": id,
                    "kind": c.kind,
                    "enabled": c.enabled,
                    "configured": self.supplier_of(id).is_some(),
                    // 空 = 此刻可以从这家买。非空是人读的原因,直接显示。
                    "blocked": blocked.get(id).cloned().unwrap_or_default(),
                    "error": err,
                    "balance_cny": sv.map(|s| (s.balance_cny * 100.0).round() / 100.0),
                    "balance_native": sv.map(|s| s.balance_native.clone()).unwrap_or_default(),
                    "spent_today_cny": (spent * 100.0).round() / 100.0,
                    "daily_cap_cny": c.daily_cap_cny,
                    "priority": c.priority,
                    "shelf_priority": c.shelf_priority,
                    "shelves": shelves,
                })
            })
            .collect();
        serde_json::Value::Array(items)
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

        // ── 到这里才去问各家库存(前面没破水位就不该增加上游请求密度)──
        let (surveys, failed) = self.survey_all(&p).await;
        if surveys.is_empty() {
            let why = if self.suppliers.is_empty() {
                "没有配置任何可用货源".to_string()
            } else {
                failed.iter().map(|(i, e)| format!("{i}: {e}")).collect::<Vec<_>>().join("; ")
            };
            // ⚠️ 这里**不**置 `out_of_stock`:询价全失败时最可能的原因恰恰是对方在限流
            // 我们,而抢货模式会把请求密度再提高 6 倍。让它退回常规轮询是自动的退避 ——
            // 「问不到」与「问到了但没货」必须走相反的方向。
            return Decision {
                healthy: Some(health.healthy),
                ..Decision::skip(format!("所有货源询价失败,本轮跳过 —— {why}"))
            };
        }
        let blocked = self.blocked_suppliers(&p, day_start);
        let total_stock: i64 = surveys.iter().flat_map(|s| &s.shelves).map(|x| x.stock).sum();
        let total_balance: f64 = surveys.iter().map(|s| s.balance_cny).sum();

        // ── 选家:能买的里面最便宜的那个。所有花钱闸门都在这里面逐货架跑一遍。──
        let caps = self.supplier_caps(day_start);
        let cand = match choose_shelf(&p, &surveys, &blocked, &caps, spent, demand, force) {
            Ok(c) => c,
            Err(why) => {
                return Decision {
                    healthy: Some(health.healthy),
                    stock: Some(total_stock),
                    balance_cny: Some(total_balance),
                    // 走到这里意味着前面每一道闸门都放行了,唯独没选出货架。
                    // 「一件都没有」才值得盯着 —— 有货却被闸门否掉时再问一遍还是同一个答案。
                    out_of_stock: total_stock <= 0,
                    ..Decision::skip(format!("无可买货架 —— {why}"))
                }
            }
        };

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
            reason: format!(
                "{trigger};选中 {} @ ¥{:.2}/个(库存 {}),买 {} 个,限价 ¥{:.2}",
                cand.shelf.label(),
                cand.shelf.unit_price_cny,
                cand.shelf.stock,
                cand.count,
                cand.need_cny,
            ),
            healthy: Some(health.healthy),
            stock: Some(cand.shelf.stock),
            price_usd: Some(cand.shelf.unit_price_cny / p.rate_cap.max(0.01)),
            balance_cny: Some(cand.balance_cny),
            candidate: Some(cand),
            out_of_stock: false,
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
        let snap = self
            .store
            .get_kv(KEY_SNAPSHOT)
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
        // 优先读 CNY 原值:多供应商之后快照里的 `price_usd` 是最便宜货架折回美元的结果,
        // 再折一次会来回丢精度。老快照没有 `price_cny`,回落 USD 路径。
        snap.as_ref()
            .and_then(|v| v.get("price_cny").and_then(|x| x.as_f64()))
            .filter(|x| *x > 0.0)
            .unwrap_or_else(|| {
                let usd = snap
                    .as_ref()
                    .and_then(|v| v.get("price_usd").and_then(|x| x.as_f64()))
                    .filter(|x| *x > 0.0)
                    .unwrap_or(p.max_price_usd);
                max_total_cny_for(1, usd, p.rate_cap)
            })
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
        self.run_once_opts(force, true).await
    }

    /// `log_skips == false` 时**不写跳过流水、不打 info 日志**,只保留返回值。
    ///
    /// 给抢货模式用:5 秒一轮、断供能连着 4.7 小时 → 一次断供就是三千多条一模一样的
    /// 「所有货源都没有库存」。那不只是占地方,它会把**决策流水这个工具本身**毁掉 ——
    /// 面板上翻十页看不到一条有信息量的记录,等于没有流水。
    ///
    /// 抢货的可见性由另一种形态提供:进入/退出各记一条,带**次数与时长**
    /// (见 `restock::spawn`)。那两条比三千条更能回答「刚才断了多久」。
    pub async fn run_once_opts(&self, force: bool, log_skips: bool) -> Decision {
        // 花钱临界区从**读预算**开始,而不是从下单开始 —— `evaluate` 里那次
        // `restock_spent_since` 与后面的 `restock_create_order` 必须在同一把锁内,
        // 否则两个执行体会先后读到同一个 `spent`,各自认为「还有额度」再各自下单。
        //
        // ⚠️ TTL 锁给不了 exactly-once(没有 fencing token):持有者卡死超过
        // `PURCHASE_LOCK_TTL_SECS` 时锁会过期放第二个人进来。这里拿到的是两层保障:
        // 常见情况被串行化,而**最坏情况的总花费**由日预算兜住 —— 后者现在把 `pending`
        // 按限价计入,所以卡死那一单的钱不会被下一个执行体当成没花过。
        let Some(_guard) = self.enter_purchase_section() else {
            let d = Decision::skip("另一个执行体正在补货(花钱锁被占),本轮跳过");
            if log_skips {
                self.log("skip", &d);
                tracing::info!("补货跳过: {}", d.reason);
            }
            return d;
        };
        let d = self.evaluate(force).await;
        if !d.act {
            if log_skips {
                self.log("skip", &d);
                tracing::info!("补货跳过: {}", d.reason);
            }
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
        // 掏钱之前**再确认一次自己仍是 leader**(见 [`Self::holds_lease`])。
        // `force` = 人在面板上点的「立即补一个」,它不参与 leader 选举,跳过这道检查 ——
        // 它的互斥由花钱锁负责。
        if !force && !self.holds_lease() {
            let mut dd = d.clone();
            dd.act = false;
            dd.reason = format!("已不再持有补货租约(可能被别的 router 接管),放弃购买 —— {}", d.reason);
            self.log("skip", &dd);
            tracing::warn!("补货:{}", dd.reason);
            return dd;
        }
        self.buy_and_onboard(&p, &d).await;
        d
    }

    async fn buy_and_onboard(&self, p: &Params, d: &Decision) {
        let Some(cand) = d.candidate.clone() else {
            tracing::error!("补货:决策说要买却没有选中货架,放弃(这是 bug)");
            return;
        };
        let Some(sup) = self.supplier_of(&cand.shelf.supplier_id) else {
            tracing::error!("补货:选中的货源 {} 已不存在,放弃", cand.shelf.supplier_id);
            return;
        };
        let order_id = new_order_id();

        // ① 幂等键先落库,**并且带上货源** —— 落库失败就不发请求(否则崩在途中会丢失
        //    order_id,重试即重复扣款);而没记货源的在途订单在多供应商下是无主的,
        //    对账时谁都不敢去重放,那笔钱就永远找不回来了。
        if let Err(e) = self.store.restock_create_order(
            &order_id,
            cand.count,
            cand.need_cny,
            &cand.shelf.supplier_id,
            &cand.shelf.shelf_id,
            &cand.shelf.account_region,
        ) {
            tracing::error!("补货:幂等键落库失败,放弃本轮购买: {e}");
            self.log_msg("error", &format!("幂等键落库失败,未发出购买请求: {e}"));
            return;
        }

        let outcome = sup
            .buy(&cand.shelf, cand.count, &order_id, cand.need_cny, p.rate_cap)
            .await;
        let r = match outcome {
            BuyOutcome::Ok(r) => r,
            // ── 结果未知(网络失败 / 5xx):**订单必须停在 `pending`** ──
            //
            // 原先所有错误一律标 `failed`,而对账只扫 `pending` —— 于是「对方已扣款、
            // 响应丢了」会永久失去对账机会,钱和 key 双双成孤儿。停在 `pending` 之后:
            // 日预算按限价把它算进当日花费(不会再被当成没花钱),5 分钟后对账用
            // **原幂等键**去确认,key 也就找回来了。
            //
            // 也**不计熔断** —— 网络抖动不是供应商故障,拿它熔断等于自己关掉补货。
            BuyOutcome::Unknown(e) => {
                let _ = self.store.restock_mark_status(&order_id, "pending", &e);
                self.log_msg(
                    "error",
                    &format!("{} 购买结果未知,订单在途待对账(可能已扣款): {e}", cand.shelf.label()),
                );
                tracing::error!("补货:购买结果未知 order={order_id},**不判死**,留给对账确认: {e}");
                return;
            }
            // ── 竞争失败:确定没扣款,不计熔断(一个抢购高峰不该把补货自己关掉)。──
            BuyOutcome::Conflict(e) => {
                let _ = self.store.restock_mark_status(&order_id, "failed", &e);
                self.log_msg("error", &format!("{} 竞争失败: {e}", cand.shelf.label()));
                tracing::warn!("补货:竞争失败 order={order_id}: {e}");
                return;
            }
            // ── 对方拒绝:确定没扣款,但重试一万次也一样 → 计入**这一家**的熔断。──
            BuyOutcome::Fault(e) => {
                let _ = self.store.restock_mark_status(&order_id, "failed", &e);
                self.log_msg("error", &format!("{} 购买失败: {e}", cand.shelf.label()));
                tracing::error!("补货:购买失败 order={order_id}: {e}");
                self.trip_supplier(p, &cand.shelf.supplier_id, &format!("购买异常: {e}"));
                return;
            }
        };

        // ② key 一到手立刻落库,后续任何失败都不会让它蒸发。
        //
        // 落库只存 key 串本身:订单里所有 key 来自同一个货架、共享同一个 region,
        // 所以 region 是**订单级**属性,已在 ① 里随订单记下,不在每个 key 上重复一遍。
        let debited = if r.debited_cny > 0.0 { Some(r.debited_cny) } else { None };
        let spent = self
            .store
            .restock_mark_purchased(
                &order_id,
                &key_strings(&r.keys),
                debited,
                d.balance_cny,
                r.balance_after_cny,
            )
            .unwrap_or(0.0);
        tracing::warn!(
            "补货:{} 购买成功 order={order_id} 数量={} 实扣=¥{spent:.2} 余额=¥{:.2}",
            cand.shelf.label(),
            r.purchased,
            r.balance_after_cny
        );
        self.log_msg(
            "buy",
            &format!(
                "{} 购买 {} 个,实扣 ¥{spent:.2},余额 ¥{:.2}",
                cand.shelf.label(),
                r.purchased,
                r.balance_after_cny
            ),
        );
        if spent <= 0.0 {
            // 记不了账,日预算阀就会失真 —— 必须让人看见,别让它静静地漏。
            tracing::error!("补货:order={order_id} 无法计算实际扣款,日预算累计可能失真");
        }
        // 买成了就把该家的连败计数清零。不清的话它只增不减 ——
        // 上周一次 401、之后几百单成功、下周再一次 401,会被算成「连续 2 次失败」
        // 而熔断一家健康的货源,流量白白落回贵号池。
        let _ = self
            .store
            .upsert_kv(&registry::fault_streak_key(&cand.shelf.supplier_id), "0");
        // 全新的幂等键不该命中重放。命中说明 `new_order_id` 撞了号,
        // 而幂等键一旦重复,「这单是不是已经买过」就永远问不清楚了。
        if r.replayed {
            tracing::error!(
                "补货:全新订单 {order_id} 被对方判为重放 —— 幂等键疑似撞号,请立刻核对"
            );
        }
        // 实扣显著超过本单限价 = 对方在询价与下单之间涨了价,而它**不执行限价**。
        // 这一笔拦不住(钱已经扣了),但必须立刻熔断这家,否则同样的超付会每轮重演。
        // 容差 1 分,躲开浮点与取整噪声。
        if spent > cand.need_cny + 0.01 {
            let msg = format!(
                "{} 实扣 ¥{spent:.2} 超过本单限价 ¥{:.2} —— 对方不执行限价,已熔断该货源",
                cand.shelf.label(),
                cand.need_cny
            );
            self.log_msg("error", &msg);
            tracing::error!("补货:{msg}");
            let _ = self
                .store
                .upsert_kv(&registry::breaker_key(&cand.shelf.supplier_id), &msg);
        }
        // 拿到的 key 数少于付了钱的数量 = 有钱买了却没到手的号。
        // 订单最终会标 imported,那几份价值不会进孤儿告警,只能靠这条流水看见。
        if !r.keys.is_empty() && (r.keys.len() as i64) < r.purchased {
            self.log_msg(
                "error",
                &format!(
                    "⚠ {} 只解析出 {} 个 key,但对方称成交 {} 个 —— 差额已付款未到手",
                    cand.shelf.label(),
                    r.keys.len(),
                    r.purchased
                ),
            );
        }
        // 成交数超过请求数 = 对方放大了我们的订单(kiroapp 实测会 clamp 后成交)。
        // 号照收(钱已经花了,丢掉才是真损失),但必须在流水里留下证据。
        if r.purchased > cand.count {
            self.log_msg(
                "error",
                &format!(
                    "⚠ {} 超卖:请求 {} 个、成交 {} 个,请核对钱包余额",
                    cand.shelf.label(),
                    cand.count,
                    r.purchased
                ),
            );
        }

        if r.keys.is_empty() {
            let _ = self
                .store
                .restock_mark_status(&order_id, "purchased", "响应未包含任何 ksk_ key");
            // 状态停在 `purchased` 而不是 `failed`:钱确实扣了,这是**孤儿订单**,
            // 面板会单列出来交给人处理。判 failed 等于把这笔花费从账上抹掉。
            self.maybe_trip(p, "购买成功但响应无 key");
            return;
        }
        self.onboard(p, &order_id, &r.keys).await;
    }

    /// 某一家连续购买故障达阈值 → **只熔断这一家**。
    ///
    /// 与全局熔断([`Self::maybe_trip`],管的是「买到却没上号」)分开:
    /// 那是系统性问题、该整个停手;这是某一家的问题,停它一家、继续用别家 ——
    /// 用一个全局开关去关掉一家的故障,等于亲手制造多供应商本来要消除的断供。
    fn trip_supplier(&self, p: &Params, id: &str, reason: &str) {
        let key = registry::fault_streak_key(id);
        let n: i64 = self
            .store
            .get_kv(&key)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0)
            + 1;
        let _ = self.store.upsert_kv(&key, &n.to_string());
        if n >= p.import_fail_breaker {
            let msg = format!("连续 {n} 次失败: {reason}");
            let _ = self.store.upsert_kv(&registry::breaker_key(id), &msg);
            self.log_msg("error", &format!("{id} 已熔断: {msg}"));
            tracing::error!("补货:货源 {id} 熔断,暂停从它购买 —— {msg}");
            // 熔断 = 这家从此不再被选中,直到人手动解除。没人盯日志,
            // 而「为什么这几个小时一个号都没补」的答案就藏在这一行里。
            self.notify_detached(
                notify::EV_BREAKER,
                format!("【补货熔断】货源 {id} 已停用:{msg}\n面板「解除熔断」后才会恢复从它购买。"),
                serde_json::json!({ "supplier": id, "reason": msg, "scope": "supplier" }),
            );
        }
    }

    /// 建号 → 逐组提权 → 调并发 → 开排队 → 捅 worker 同步。
    ///
    /// 搬进 caio 之后这些全是**内部调用**,不再走 admin HTTP。
    async fn onboard(&self, p: &Params, order_id: &str, keys: &[BoughtKey]) {
        let json = match serde_json::to_string(&import_payload(keys)) {
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

        // 出口网关。**这一段在 2026-08-06 之前是不存在的** —— `p.egress` 有默认值
        // "auto"、有设置项、有前端下拉,但补货路径直接调 store.create_account(),
        // 从没读过它,于是每个自动买来的号都落在直连(服务器主 IP)上。
        // 后果实测:直连的 42 个号里 25 个被 AWS TEMPORARILY_SUSPENDED(59.5%),
        // 而同期走代理池的 11 个一个没封 —— 上游是按出口 IP 把它们关联起来一锅端的。
        // 手动导入(admin/accounts.rs)一直是对的,只有这条路径漏了。
        let mut egress_picker =
            crate::admin::accounts::EgressPicker::build(&self.store, Some(&p.egress));

        let mut new_ids = Vec::new();
        for acc in &parsed {
            let mut extra_map = acc.extra.clone();
            // 已带 proxy 的不动(导出文件自带出口的情形),其余按 picker 分配。
            let has_proxy = crate::admin::accounts::account_proxy(
                &serde_json::Value::Object(extra_map.clone()).to_string(),
            )
            .is_some();
            if !has_proxy {
                if let Some(url) = egress_picker.next() {
                    extra_map.insert("proxy".into(), serde_json::json!(url));
                }
            }
            let extra = serde_json::Value::Object(extra_map).to_string();
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
            // 全局熔断比单家熔断严重得多:买到号却上不了号,钱花了没东西。
            self.notify_detached(
                notify::EV_BREAKER,
                format!("【补货全局熔断】已停止一切自动购买:{msg}\n通常是「买到却没上号」,请到面板查订单与决策流水。"),
                serde_json::json!({ "reason": msg, "scope": "global" }),
            );
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

    /// 处理卡在 `pending` 的订单:用**原幂等键 + 原参数**向下单的那一家确认真实结果。
    ///
    /// 「向哪一家确认」靠订单自己的 `supplier` 列 —— 它和幂等键同一条 INSERT 落库,
    /// 所以任何在途订单都是有主的。
    ///
    /// 各家的确认手法不同(drop 只能重放、kiroapp 能先只读查单),但对本函数是透明的:
    /// 契约只承诺「返回四态之一」,见 [`Supplier::reconcile`]。
    pub async fn reconcile_pending(&self) {
        let p = self.params();
        let Ok(pending) = self.store.restock_pending_orders(RECONCILE_MIN_AGE_SECS) else {
            return;
        };
        if pending.is_empty() {
            return;
        }
        // 对账会**重放购买请求**,所以它和下单一样属于花钱临界区。
        //
        // 光靠 leader 租约不够:租约 TTL 是 `poll_interval × 3`(默认 90s),而一轮对账
        // 最多处理 100 单、每单一到两次上游往返 —— 完全可能跑过租约到期,于是第二个
        // router 当选后对**同一批 pending** 再重放一次。两个执行体同时重放同一个
        // 幂等键,一旦对方的幂等有任何缝隙就是双扣。
        let Some(_guard) = self.enter_purchase_section() else {
            tracing::info!("补货:对账跳过(花钱锁被占,另一个执行体正在处理)");
            return;
        };
        for o in pending {
            if p.dry_run {
                continue;
            }
            // 历史订单(多供应商之前下的)没有 supplier 列,归给 drop —— 那时只有它一家,
            // 这不是猜,是事实。
            let sid = if o.supplier.is_empty() { "drop" } else { o.supplier.as_str() };
            let Some(sup) = self.supplier_of(sid) else {
                tracing::error!(
                    "补货:在途订单 {} 的货源 {sid} 已不在名册里,无法对账 —— \
                     请先把它加回名册(哪怕只是为了收尾),否则这笔钱找不回来",
                    o.client_order_id
                );
                continue;
            };
            tracing::warn!("补货:发现未完成订单 {}(货源 {sid}),确认真实结果", o.client_order_id);
            // 用**下单时的限价**而不是现价重放:现价可能已经涨过,拿它去重放等于
            // 悄悄放宽了当初的限价保护。
            match sup
                .reconcile(&o.client_order_id, &o.shelf, o.count, o.max_total_cny, p.rate_cap)
                .await
            {
                BuyOutcome::Ok(r) => {
                    // 对方没认出幂等键 = **已经发生第二次真实扣款**。重放安全这个前提没了,
                    // 而整套对账都建立在它上面 —— 立刻熔断这家,别让下一轮接着重放。
                    if r.double_charged {
                        let msg = format!(
                            "{sid} 对账重放未命中幂等,疑似二次扣款(订单 {})",
                            o.client_order_id
                        );
                        let _ = self.store.upsert_kv(&registry::breaker_key(sid), &msg);
                        self.log_msg("error", &msg);
                    }
                    // 扣款 **fail-closed**:适配器说不出实扣时回落到**下单时的限价**,
                    // 绝不能记 0。drop 就说不出(它不下发单笔扣款额),而对账路径又拿不到
                    // 买前余额 —— 记 0 的后果是一笔真实扣款从日预算里凭空消失,
                    // 当天可以据此再多买一单。宁可高估。
                    let debited = if r.debited_cny > 0.0 {
                        r.debited_cny
                    } else {
                        tracing::warn!(
                            "补货:{sid} 订单 {} 对账拿不到实扣额,按限价 ¥{:.2} 记账(宁可高估)",
                            o.client_order_id,
                            o.max_total_cny
                        );
                        o.max_total_cny
                    };
                    let _ = self.store.restock_mark_purchased(
                        &o.client_order_id,
                        &key_strings(&r.keys),
                        Some(debited),
                        None,
                        r.balance_after_cny,
                    );
                    if r.keys.is_empty() {
                        // 确认成交却拿不回 key:状态停在 `purchased` = **孤儿订单**,
                        // 面板醒目地交给人处理。钱已如实记账,预算不会漏。
                        let _ = self.store.restock_mark_status(
                            &o.client_order_id,
                            "purchased",
                            "对账确认已成交但取不回 key,需人工上号",
                        );
                        self.log_msg(
                            "error",
                            &format!("{sid} 订单 {} 已扣款但无 key,转人工", o.client_order_id),
                        );
                    } else {
                        self.onboard(&p, &o.client_order_id, &r.keys).await;
                    }
                }
                // **确定没成交**(对方无此单 / 明确拒绝)→ 判死,把限价占用的预算还回来。
                BuyOutcome::Conflict(e) | BuyOutcome::Fault(e) => {
                    let _ = self.store.restock_mark_status(
                        &o.client_order_id,
                        "failed",
                        &format!("对账确认未成交: {e}"),
                    );
                }
                // 确认本身结果未知 → **继续留在 `pending`**,下一轮再试。
                // 判死等于放弃找回这笔钱,而确认是只读/幂等的,多试几次没有代价。
                BuyOutcome::Unknown(e) => {
                    let _ = self.store.restock_mark_status(
                        &o.client_order_id,
                        "pending",
                        &format!("对账结果未知,继续在途: {e}"),
                    );
                    tracing::warn!("补货:订单 {} 对账结果未知,保持在途", o.client_order_id);
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

    pub(crate) fn log_msg(&self, action: &str, reason: &str) {
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

    // ───────────────────────── 通知 ─────────────────────────

    /// 发一条事件回调,**按事件分组节流**。
    ///
    /// 节流状态落在 `settings` 表而不是进程内存里:抢货循环会随 router 重启而重来,
    /// 内存计数一重启就归零,于是每次部署都会补发一轮通知 —— 而部署往往正发生在
    /// 出事之后,那时最不需要的就是重复告警。
    ///
    /// **永不返回错误**:通知失败的正确后果是「人没收到消息」,不是「补货停了」。
    pub async fn notify(&self, event: &str, text: &str, payload: serde_json::Value) {
        let p = self.params();
        if p.notify_url.trim().is_empty() {
            return;
        }
        let key = notify::throttle_key(event);
        let now = now_ts();
        let last: i64 = self
            .store
            .get_kv(&key)
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        if now - last < p.notify_min_gap_secs.max(1) {
            tracing::debug!("补货:通知 {event} 被节流(距上次 {}s)", now - last);
            return;
        }
        // **先记时间再发送**:发送要几秒,期间可能又触发一次同类事件。
        // 后记的话两条会一起挤过节流门。
        let _ = self.store.upsert_kv(&key, &now.to_string());
        let mut payload = payload;
        if let Some(m) = payload.as_object_mut() {
            m.insert("event".into(), serde_json::json!(event));
            m.insert("ts".into(), serde_json::json!(now));
        }
        match notify::send(&self.http, &p.notify_url, text, payload).await {
            Ok(()) => tracing::info!("补货:已回调通知 {event}"),
            // 地址里带着机器人 key,`notify::send` 已经把它从错误里剥掉了。
            Err(e) => tracing::warn!("补货:通知 {event} 发送失败: {e}"),
        }
    }

    /// 同步上下文里发通知(熔断路径)。
    ///
    /// 单独一条而不是把 `trip_supplier` 改成 async:那两个函数被 `buy_and_onboard`
    /// 的好几条错误分支调用,改签名会把「熔断」这件事的调用点全部搅动一遍,
    /// 而熔断路径是整套补货里最不该在改动中出错的地方。
    fn notify_detached(&self, event: &'static str, text: String, payload: serde_json::Value) {
        // Engine 里除 store 外都是可克隆的轻量句柄,单独造一个只为发通知的实例
        // 比给整个 Engine 加 Arc<Self> 约束简单。
        let store = self.store.clone();
        let http = self.http.clone();
        let workers = self.workers.clone();
        let suppliers = self.suppliers.clone();
        let roster = self.roster.clone();
        let holder = self.holder.clone();
        tokio::spawn(async move {
            let eng = Engine { store, suppliers, roster, workers, http, holder };
            eng.notify(event, &text, payload).await;
        });
    }
}

/// 落库用的 key 串列表(订单 `keys_json` 只记 key 本身)。
pub fn key_strings(keys: &[BoughtKey]) -> Vec<String> {
    keys.iter().map(|k| k.api_key.clone()).collect()
}

/// 上号用的导入载荷。**纯函数,可直接测。**
///
/// ⚠️ **必须是对象数组,不能是裸串数组。** 这是整个 P1 的要害:
///
/// | 形态 | import 走的路 | extra 里有 region 吗 |
/// |---|---|---|
/// | `["ksk_a"]` | `map_account` → `map_api_key(s, None)` | **没有** |
/// | `[{"api_key":"ksk_a","region":"eu-central-1"}]` | `map_account` → `map_flat` → `map_api_key(key, Some(acc))` | **有** |
///
/// 而 region 缺失时 `gw_kiro` 一律按 `us-east-1` 发包,欧洲区的号会**每个请求都 403**
/// (依据见 [`BoughtKey`])。
///
/// **对 drop 的号逐字节等价**:两条路的 `account_id` 派生完全相同
/// (无 email 时都是 `kiro-apikey-{sha256(key)[..12]}`,见 `import.rs:292`),
/// 而空的 `region` / `subscription_title` 这里直接**不写进对象** —— import 侧对这两个字段
/// 用的是 `filter(|s| !s.is_empty())`,写空串与不写等效,但不写能让日志和排障时
/// 一眼看出「这家没给区域」而不是「区域是空的」。
pub fn import_payload(keys: &[BoughtKey]) -> Vec<serde_json::Value> {
    keys.iter()
        .map(|k| {
            let mut o = serde_json::Map::new();
            o.insert("api_key".into(), serde_json::json!(k.api_key));
            if !k.region.is_empty() {
                o.insert("region".into(), serde_json::json!(k.region));
            }
            if !k.subscription_title.is_empty() {
                o.insert("subscription_title".into(), serde_json::json!(k.subscription_title));
            }
            serde_json::Value::Object(o)
        })
        .collect()
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

    // ───────────────────── 出口网关:自动补货必须分配代理 ─────────────────────

    fn store_with_pool(pool: &[&str]) -> gw_store::SqliteStore {
        let store = gw_store::SqliteStore::open_in_memory().unwrap();
        let json = serde_json::json!({ "egress_pool": pool }).to_string();
        store.upsert_settings(&json).unwrap();
        store
    }

    /// `egress:"auto"` 必须真的发出池里的出口,并且**在同一批内轮换**。
    ///
    /// 这条守的是 2026-08-06 查出来的事故:自动补货的号全落在直连(服务器主 IP)上,
    /// 被 AWS 按出口 IP 关联,直连的 42 个里 25 个被 TEMPORARILY_SUSPENDED(59.5%),
    /// 同期走代理池的 11 个一个没封。
    #[test]
    fn 自动分配必须发出池里的出口且同批内轮换() {
        let store = store_with_pool(&["http://a:1", "http://b:2"]);
        let mut picker = crate::admin::accounts::EgressPicker::build(&store, Some("auto"));
        let got: Vec<_> = (0..4).filter_map(|_| picker.next()).collect();
        assert_eq!(got.len(), 4, "每个号都必须拿到出口,拿不到就是又回到直连了");
        assert_eq!(
            got.iter().filter(|u| *u == "http://a:1").count(),
            2,
            "同一批导入必须铺开,不能全堆在第一个出口上(那等于只用了一个 IP)"
        );
    }

    /// 补货默认参数就是 "auto" —— 这个默认值曾经形同虚设(见下面那条结构测试)。
    #[test]
    fn 补货默认出口是auto而不是直连() {
        let p: Params = serde_json::from_str("{}").unwrap();
        assert_eq!(p.egress, "auto");
    }

    /// 结构不变量:补货的建号路径**必须**先经过 `EgressPicker`。
    ///
    /// 行为测试在这里救不了命 —— 这个 bug 是「整段代码不存在」。`p.egress` 有默认值、
    /// 有设置项、有前端下拉,唯独 `onboard()` 从没读过它,直接 `create_account()` 了事,
    /// 于是每个自动买来的号都走直连。参数看着是配好的,面板上一切正常,只有逐个翻
    /// 账号的 `extra.proxy` 才看得出来。谁哪天重构掉这一段,这条当天变红。
    #[test]
    fn 补货建号前必须先分配出口() {
        let src = include_str!("engine.rs");
        let onboard = src
            .split("async fn onboard(")
            .nth(1)
            .expect("onboard() 还在吗?改名了就把这条测试一起改");
        // 锚在真实调用 `self.store.create_account(` 上,不是裸的 `create_account(` ——
        // 后者会匹配到上面那段解释这个 bug 的注释,于是测试红在自己的文档上。
        let create_at = onboard
            .find("self.store.create_account(")
            .expect("onboard 应该建号");
        let picker_at = onboard
            .find("EgressPicker::build(")
            .expect("补货建号前必须先构造 EgressPicker,否则新号全部直连");
        assert!(
            picker_at < create_at,
            "EgressPicker 必须在 create_account 之前构造,否则分配不到 extra.proxy 上"
        );
    }

    fn row(id: &str, reason: &str, disabled: bool) -> RuntimeRow {
        RuntimeRow { account_id: id.into(), reason: reason.into(), disabled }
    }

    /// 全部号都是「很久以前建的」,以便默认落在宽限期之外。
    fn born_long_ago(ids: &[&str]) -> HashMap<String, i64> {
        ids.iter().map(|i| ((*i).to_string(), NOW - 10_000)).collect()
    }

    // ───────────────────── 选家:多供应商唯一花钱的判断 ─────────────────────

    /// 2026-08-05 实测的两家真实报价。drop 只有一个货架,kiroapp 分 us / eu。
    fn two_suppliers(drop_stock: i64, eu_stock: i64, us_stock: i64) -> Vec<Survey> {
        vec![
            Survey {
                supplier_id: "drop".into(),
                balance_cny: 123.29,
                balance_native: String::new(),
                shelves: vec![Shelf {
                    supplier_id: "drop".into(),
                    shelf_id: String::new(),
                    account_region: String::new(),
                    stock: drop_stock,
                    unit_price_cny: 20.95, // $2.91 × 7.2
                    max_per_order: 10,
                    priority: 0,
                }],
            },
            Survey {
                supplier_id: "kiroapp".into(),
                balance_cny: 699.4,
                balance_native: "680 积分".into(),
                shelves: vec![
                    Shelf {
                        supplier_id: "kiroapp".into(),
                        shelf_id: "eu".into(),
                        account_region: "eu-central-1".into(),
                        stock: eu_stock,
                        unit_price_cny: 15.43, // 15 积分 ÷ 7 × 7.2
                        max_per_order: 10,
                        priority: 0,
                    },
                    Shelf {
                        supplier_id: "kiroapp".into(),
                        shelf_id: "us".into(),
                        account_region: "us-east-1".into(),
                        stock: us_stock,
                        unit_price_cny: 30.86, // 30 积分,贵 47%
                        max_per_order: 10,
                        priority: 0,
                    },
                ],
            },
        ]
    }

    /// 同样两家,但盖上生产打算用的档位:drop 0 / kiroapp-us 1 / kiroapp-eu 2。
    /// 模拟 [`Engine::survey_all`] 回填后的形态。
    fn two_suppliers_tiered(drop_stock: i64, eu_stock: i64, us_stock: i64) -> Vec<Survey> {
        let mut v = two_suppliers(drop_stock, eu_stock, us_stock);
        for s in &mut v {
            for sh in &mut s.shelves {
                sh.priority = match (s.supplier_id.as_str(), sh.shelf_id.as_str()) {
                    ("kiroapp", "eu") => 2,
                    ("kiroapp", _) => 1,
                    _ => 0,
                };
            }
        }
        v
    }

    /// 需求足够高,让单位成本闸不参与判断(它是另一条测试线的事)。
    fn busy() -> Params {
        Params::default()
    }

    /// 没有配置任何单家日上限。
    fn no_caps() -> HashMap<String, (f64, f64)> {
        HashMap::new()
    }

    #[test]
    fn 选中能买的里面最便宜的那个() {
        let d = choose_shelf(&busy(), &two_suppliers(5, 18, 3), &HashMap::new(), &no_caps(), 0.0, 1800.0, false)
            .unwrap();
        assert_eq!(d.shelf.label(), "kiroapp/eu", "EU ¥15.43 比 drop ¥20.95 便宜");
        assert_eq!(d.shelf.account_region, "eu-central-1", "区域必须跟着货架走到订单上");
        assert_eq!(d.count, 1);
    }

    #[test]
    fn 便宜那家没货就落到贵的那家而不是不买() {
        // **这才是接第二家的真正理由。** 近 7 天 drop 有 854 轮「就差有货」,
        // 那些轮里流量全落到贵号池。此处 EU 没货 → 必须自动换 drop,而不是空手而归。
        let d =
            choose_shelf(&busy(), &two_suppliers(5, 0, 0), &HashMap::new(), &no_caps(), 0.0, 1800.0, false)
                .unwrap();
        assert_eq!(d.shelf.label(), "drop");
    }

    // ── 档位:让「便宜但号会被封」不再自动获胜 ──

    #[test]
    fn 配了档位后首选家有货就买首选家哪怕它更贵() {
        // 生产口径:drop ¥20.95 档 0,kiroapp/eu ¥15.43 档 2。
        // 纯比价会选 EU;配了档位必须选 drop —— 这就是这次改动要的行为。
        let d = choose_shelf(
            &busy(),
            &two_suppliers_tiered(5, 18, 3),
            &HashMap::new(),
            &no_caps(),
            0.0,
            1800.0,
            false,
        )
        .unwrap();
        assert_eq!(d.shelf.label(), "drop");
        // ⚠️ 这里的 fixture 是**合成的**,`account_region` 留空只是为了覆盖「区域未知」
        // 这个仍然存在的分支(老配置/别家不下发区域)。**drop 现在是按区分货的**
        // (eu-central-1 / us-east-1,见 `drop::REGIONS`),别把这条当成 drop 的现状。
        assert_eq!(d.shelf.account_region, "", "区域未知的货架:留空 = 用上游默认 us-east-1");
    }

    #[test]
    fn 首选家没货时按档位落到次选而不是落到最便宜() {
        // drop 的 us 区常年 0 库存,所以这条路径才是**生产上的常态**:
        // (2026-08-07 起 drop 还有 eu 区,那一档通常有货且更便宜)
        // 落下去之后要落到 kiroapp/us(档 1),不是更便宜的 kiroapp/eu(档 2)。
        let d = choose_shelf(
            &busy(),
            &two_suppliers_tiered(0, 18, 3),
            &HashMap::new(),
            &no_caps(),
            0.0,
            1800.0,
            false,
        )
        .unwrap();
        assert_eq!(d.shelf.label(), "kiroapp/us", "档位比价格优先");
        assert_eq!(d.shelf.account_region, "us-east-1");
    }

    #[test]
    fn 只剩末档有货时仍然买而不是宁缺毋滥() {
        // 档位是**软优先**。末档再不受待见,也好过池子空掉 ——
        // 断供的代价(客户报障)远高于买到一个可能被封的号。
        let d = choose_shelf(
            &busy(),
            &two_suppliers_tiered(0, 18, 0),
            &HashMap::new(),
            &no_caps(),
            0.0,
            1800.0,
            false,
        )
        .unwrap();
        assert_eq!(d.shelf.label(), "kiroapp/eu");
    }

    #[test]
    fn 首选家被熔断时档位不会把补货一起锁死() {
        // 熔断与档位是两条正交的闸。首选家熔断 → 照样往下落,而不是「首选家不可用就不买」。
        let blocked: HashMap<String, String> =
            [("drop".to_string(), "已熔断: 连续 2 次失败".to_string())].into();
        let d = choose_shelf(
            &busy(),
            &two_suppliers_tiered(5, 18, 3),
            &blocked,
            &no_caps(),
            0.0,
            1800.0,
            false,
        )
        .unwrap();
        assert_eq!(d.shelf.label(), "kiroapp/us");
    }

    #[test]
    fn 首选家有货但过不了花钱闸门时照样落到下一档() {
        // 「缺货才落档」只是软优先的一半,而且是生产上更少见的那一半。
        // 更常见的是**有货但买不起**:首选家余额见底、或本家日上限到顶。
        // 这条路径走的是 choose_shelf 里的 continue,rank_shelves 的测试覆盖不到。
        let caps: HashMap<String, (f64, f64)> =
            [("drop".to_string(), (19.0, 30.0))].into(); // 已花 ¥19 / 上限 ¥30,一单 ¥20.95 会突破
        let d = choose_shelf(
            &busy(),
            &two_suppliers_tiered(5, 18, 3),
            &HashMap::new(),
            &caps,
            0.0,
            1800.0,
            false,
        )
        .unwrap();
        assert_eq!(d.shelf.label(), "kiroapp/us", "档 0 超本家上限 → 落到档 1,不是落到最便宜的档 2");

        // 余额不足同理:把 drop 的余额压到一单都不够。
        let mut sv = two_suppliers_tiered(5, 18, 3);
        sv[0].balance_cny = 1.0;
        let d = choose_shelf(&busy(), &sv, &HashMap::new(), &no_caps(), 0.0, 1800.0, false).unwrap();
        assert_eq!(d.shelf.label(), "kiroapp/us");
    }

    #[test]
    fn 档位全缺省时与引入档位前选同一个货架() {
        // 回滚安全:名册没配 priority → 全 0 → 必须仍然选最便宜的 kiroapp/eu。
        let plain = choose_shelf(
            &busy(),
            &two_suppliers(5, 18, 3),
            &HashMap::new(),
            &no_caps(),
            0.0,
            1800.0,
            false,
        )
        .unwrap();
        assert_eq!(plain.shelf.label(), "kiroapp/eu");
    }

    #[test]
    fn 全都没货时说得出是哪一种没有() {
        let e = choose_shelf(&busy(), &two_suppliers(0, 0, 0), &HashMap::new(), &no_caps(), 0.0, 1800.0, false)
            .unwrap_err();
        assert!(e.contains("没有库存"), "面板要答得出「为什么没买」,实际: {e}");
    }

    #[test]
    fn 某家熔断只跳过这一家() {
        // 熔断 kiroapp 不该让 drop 也停 —— 那正是多供应商要消除的断供,
        // 用一个全局开关反手制造出来。
        let blocked: HashMap<String, String> =
            [("kiroapp".to_string(), "已熔断: 连续 2 次失败".to_string())].into();
        let d = choose_shelf(&busy(), &two_suppliers(5, 18, 3), &blocked, &no_caps(), 0.0, 1800.0, false)
            .unwrap();
        assert_eq!(d.shelf.label(), "drop", "被熔断的那家跳过,别家照买");

        // 两家都被挡住时,理由里要点名到货架,不能只说一句「没有合适的货源」。
        let all: HashMap<String, String> = [
            ("kiroapp".to_string(), "已熔断".to_string()),
            ("drop".to_string(), "本家今日已花 ¥50 达上限".to_string()),
        ]
        .into();
        let e =
            choose_shelf(&busy(), &two_suppliers(5, 18, 3), &all, &no_caps(), 0.0, 1800.0, false).unwrap_err();
        assert!(e.contains("kiroapp/eu") && e.contains("drop"), "实际: {e}");
    }

    #[test]
    fn 余额不足的那家会被跳过而不是让整轮失败() {
        let mut sv = two_suppliers(5, 18, 3);
        sv[1].balance_cny = 1.0; // kiroapp 钱包见底
        let d = choose_shelf(&busy(), &sv, &HashMap::new(), &no_caps(), 0.0, 1800.0, false).unwrap();
        assert_eq!(d.shelf.label(), "drop");
    }

    #[test]
    fn 日上限只卡到会突破的那一单不会连累便宜货架() {
        let p = busy(); // daily_cap 默认 200
        // 已花 ¥180:EU 的 ¥15.43 还塞得下(195.43),drop 的 ¥20.95 塞不下(200.95)。
        let d = choose_shelf(&p, &two_suppliers(5, 18, 3), &HashMap::new(), &no_caps(), 180.0, 1800.0, false)
            .unwrap();
        assert_eq!(d.shelf.label(), "kiroapp/eu");
        // 已花 ¥199:两个都塞不下 → 不买,且理由里带得出数字。
        let e = choose_shelf(&p, &two_suppliers(5, 18, 3), &HashMap::new(), &no_caps(), 199.0, 1800.0, false)
            .unwrap_err();
        assert!(e.contains("日上限"), "实际: {e}");
    }

    #[test]
    fn 单家日上限必须把本单算进去否则形同虚设() {
        // 审查发现的真 bug:原先只在「已花 >= 上限」时屏蔽这家,于是
        // 上限 ¥20、已花 ¥19 时它看着还没到顶,一单 ¥15.43 下去就是 ¥34.43。
        // 单家上限存在的全部意义就是限制一家能从我这里拿走多少钱,拦不住就没意义。
        let caps: HashMap<String, (f64, f64)> = [("kiroapp".to_string(), (19.0, 20.0))].into();
        let d = choose_shelf(&busy(), &two_suppliers(5, 18, 3), &HashMap::new(), &caps, 0.0, 1800.0, false)
            .unwrap();
        assert_eq!(d.shelf.label(), "drop", "kiroapp 会超本家上限,应换 drop 而不是照买");

        // 塞得下就照买,别过度保守。
        let ok: HashMap<String, (f64, f64)> = [("kiroapp".to_string(), (19.0, 100.0))].into();
        assert_eq!(
            choose_shelf(&busy(), &two_suppliers(5, 18, 3), &HashMap::new(), &ok, 0.0, 1800.0, false)
                .unwrap()
                .shelf
                .label(),
            "kiroapp/eu"
        );

        // 读不出这家已花多少时,`supplier_caps` 会填 INFINITY —— 必须表现为不买(fail-closed)。
        let broken: HashMap<String, (f64, f64)> =
            [("kiroapp".to_string(), (f64::INFINITY, 20.0))].into();
        assert_eq!(
            choose_shelf(&busy(), &two_suppliers(5, 18, 3), &HashMap::new(), &broken, 0.0, 1800.0, false)
                .unwrap()
                .shelf
                .label(),
            "drop"
        );
    }

    #[test]
    fn 单价上限按cny口径比较不会因为单位混用而误判() {
        let mut p = busy();
        // 上限 $2.50 → ¥18.00。EU(¥15.43)过,drop(¥20.95)不过。
        p.max_price_usd = 2.5;
        let d = choose_shelf(&p, &two_suppliers(5, 18, 0), &HashMap::new(), &no_caps(), 0.0, 1800.0, false)
            .unwrap();
        assert_eq!(d.shelf.label(), "kiroapp/eu");
        // 上限压到 $2.00 → ¥14.40,两个都不过。
        p.max_price_usd = 2.0;
        assert!(choose_shelf(&p, &two_suppliers(5, 18, 0), &HashMap::new(), &no_caps(), 0.0, 1800.0, false)
            .is_err());
    }

    #[test]
    fn 需求撑不起时两家都不买而force能越过() {
        let p = busy(); // max_unit_cost 默认 0.04 ¥/积分
        // 凌晨:100 分/时 × 45 分钟 ≈ 75 分产出,¥15.43 → ¥0.2/分,远超上限。
        let e = choose_shelf(&p, &two_suppliers(5, 18, 3), &HashMap::new(), &no_caps(), 0.0, 100.0, false)
            .unwrap_err();
        assert!(e.contains("需求撑不起"), "实际: {e}");
        // 人在面板上点「立即补一个」时,这道**自动化**闸门可以越过。
        assert!(choose_shelf(&p, &two_suppliers(5, 18, 3), &HashMap::new(), &no_caps(), 0.0, 100.0, true)
            .is_ok());
    }

    #[test]
    fn 单笔数量取我方意愿与对方上限的较小者() {
        let mut p = busy();
        p.max_per_purchase = 10;
        p.daily_cap_cny = 100_000.0;
        let mut sv = two_suppliers(0, 18, 0);
        sv[1].shelves[0].max_per_order = 3;
        // ⚠️ 对方的上限**不能靠它自己执行**:2026-08-05 实测 kiroapp 对超限 count 是
        // clamp 并成交(发 99 买走 10 个、扣 150 积分)。必须我方先夹好再发。
        assert_eq!(choose_shelf(&p, &sv, &HashMap::new(), &no_caps(), 0.0, 1800.0, false).unwrap().count, 3);
        // 库存比两者都小时以库存为准。
        sv[1].shelves[0].stock = 2;
        assert_eq!(choose_shelf(&p, &sv, &HashMap::new(), &no_caps(), 0.0, 1800.0, false).unwrap().count, 2);
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

    // ───────────── 上号载荷:region 贯通 ─────────────

    fn bought(api_key: &str, region: &str) -> BoughtKey {
        BoughtKey {
            api_key: api_key.into(),
            region: region.into(),
            subscription_title: String::new(),
        }
    }

    type IdAndExtra = (String, serde_json::Map<String, serde_json::Value>);

    /// 把导入载荷真正喂给 `gw_kiro::import`,断言 extra 的最终形态。
    fn imported_extra(keys: &[BoughtKey]) -> Vec<IdAndExtra> {
        let json = serde_json::to_string(&import_payload(keys)).unwrap();
        let root: serde_json::Value = serde_json::from_str(&json).unwrap();
        gw_kiro::import::parse_accounts_export(&root)
            .unwrap()
            .into_iter()
            .map(|a| (a.account_id, a.extra))
            .collect()
    }

    #[test]
    fn 区域必须落到extra否则欧洲区的号每个请求都403() {
        let got = imported_extra(&[bought("ksk_eu1", "eu-central-1")]);
        assert_eq!(got.len(), 1);
        let (_, extra) = &got[0];
        assert_eq!(
            extra.get("region").and_then(|v| v.as_str()),
            Some("eu-central-1"),
            "region 丢了 → gw_kiro 按默认 us-east-1 发包 → 实测 403 Invalid token"
        );
        assert_eq!(extra.get("kiro_api_key").and_then(|v| v.as_str()), Some("ksk_eu1"));
        assert_eq!(extra.get("auth_method").and_then(|v| v.as_str()), Some("api_key"));
    }

    #[test]
    fn 无区域时与改动前的裸串路径逐字节等价() {
        // P1 敢单独上线的**全部依据**:drop 的号(不带 region)经过新载荷之后,
        // account_id 与 extra 必须与旧的裸串数组路径一模一样。
        let old: serde_json::Value = serde_json::from_str(r#"["ksk_a","ksk_b"]"#).unwrap();
        let old_out: Vec<_> = gw_kiro::import::parse_accounts_export(&old)
            .unwrap()
            .into_iter()
            .map(|a| (a.account_id, a.extra))
            .collect();

        let new_out = imported_extra(&[bought("ksk_a", ""), bought("ksk_b", "")]);

        assert_eq!(new_out, old_out, "对 drop 的号必须零行为差异");
        // 顺带钉死 account_id 的派生没变(它是账号的稳定身份,变了等于全量重建号)。
        assert!(new_out[0].0.starts_with("kiro-apikey-"), "实际 {}", new_out[0].0);
        assert!(!new_out[0].1.contains_key("region"), "空 region 不该写进 extra");
    }

    #[test]
    fn 档位标签非空才写入() {
        let with = BoughtKey {
            api_key: "ksk_p".into(),
            region: String::new(),
            subscription_title: "KIRO POWER".into(),
        };
        let got = imported_extra(&[with]);
        assert_eq!(
            got[0].1.get("subscription_title").and_then(|v| v.as_str()),
            Some("KIRO POWER")
        );
        // 空的不写:写空串虽然 import 侧也会过滤,但留空能让排障时一眼看出
        // 「这家没给档位」而不是「档位是空的」。
        let payload = import_payload(&[bought("ksk_q", "")]);
        assert!(!payload[0].as_object().unwrap().contains_key("subscription_title"));
        assert!(!payload[0].as_object().unwrap().contains_key("region"));
    }

    #[test]
    fn 落库只记key串() {
        let ks = [bought("ksk_a", "eu-central-1"), bought("ksk_b", "")];
        assert_eq!(key_strings(&ks), vec!["ksk_a".to_string(), "ksk_b".to_string()]);
    }
}
