//! worker 组内账号调度 —— 🟢 移植旧 kiro.rs `MultiTokenManager` 的 v52 会话亲和。
//!
//! ## 为什么在 worker 层
//!
//! kiro.rs 是单进程,一个 `MultiTokenManager` 既做选号又做亲和。claude-all-in-one 拆成多进程:
//! - **router** 做 session→worker 亲和(同会话钉同 worker,已实现);
//! - **worker**(本模块)做 session→**组内账号**亲和:同会话钉同一上游账号。
//!
//! 这一层才是 Kiro prefix cache 命中的命门——**Kiro 缓存按上游账号隔离**,同会话每换
//! 一个账号≈冷启动。v52 实测:同 conversationId 19 秒内横跳 4 个账号 → 命中率塌到个位数。
//!
//! ## v52 亲和铁律:「落在哪个号就认哪个号」
//!
//! - 新会话:在合格账号里按**优先级分层 LRU** 选 primary(最高优先级层内 last_selected 最旧);
//! - 老会话且 primary 当下可用:一直用 primary(缓存热);**唯一例外**=向上迁移(见下);
//! - primary 当下不可用(busy/冷却/禁用):**立即**改选 LRU 候选并**当场转正为新 primary**,
//!   此后钉死新号、**永不主动向下/同层迁回**原号(消除旧版「空↔满」抖动导致的橡皮筋横跳)。
//!
//! ### 例外:两层优先级下的「向上迁移」
//! primary 落在**低优先级层**、而此刻**更高层有空闲 permit** 时,允许迁回高层一次并转正
//! (让稀缺的高优先级号被积极使用,而非被老会话永久粘在低层浪费)。为不重演橡皮筋横跳,
//! 迁移带去抖 [`MIGRATE_UP_DEBOUNCE`](每会话跨层迁移 ≤1 次/窗口);高层饱和(permit=0)时
//! 不迁。**同层内**仍严格「永不迁回」。代价:每次跨层迁移 = 新账号对 Kiro 一次冷启动
//! (cache_sim 按会话键、对换号无感,会短暂高估命中→计费失真;去抖已把频率压到最低)。
//!
//! ## 亲和键 = conversationId(不是 metadata session_id)
//!
//! Claude Code 等客户端常不传 `metadata.user_id`,但 Kiro 请求体里的 conversationId 由
//! converter 从 system 锚点 + 前2条 user 稳定派生(见 gw-kiro converter)。亲和键用它,与
//! cache_sim 的 session_key **同源**(审查 #131③:router/sim/亲和三处身份链统一)。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use gw_core::account::Account;
use gw_core::config::SchedulerConfig;
use gw_core::error::UpstreamErrorKind;
use parking_lot::{Mutex, RwLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 调度参数(由 system.yaml `scheduler` 段注入;默认值见 [`SchedulerConfig`],
/// 对齐 kiro.rs 生产配置)。全部 clamp 到 ≥1,杜绝 0 值造成"必禁用/必过期"。
/// `Copy`:各方法顶部一次性 `let t = *self.tuning.read()` 拷出快照,避免反复持锁。
#[derive(Clone, Copy)]
struct Tuning {
    /// 会话亲和映射 TTL:超时未访问惰性淘汰(等于会话重开,自然再平衡)。
    affinity_ttl: Duration,
    /// 429 限流冷却时长(到期自愈)。
    rate_limit_cooldown: Duration,
    /// 账号被上游临时封禁的冷却时长(默认 1h,比限流长——别频繁重戳封禁号)。
    suspended_cooldown: Duration,
    /// 空响应冷却时长(v58 阈值冷却用)。
    empty_cooldown: Duration,
    /// 空响应固定窗口:窗口内累计 empty 达阈值才冷却(避免误伤偶发 empty 的健康号)。
    empty_window: Duration,
    empty_threshold: u32,
    /// 连续 API 失败达此次数 → 自动禁用(TooManyFailures,可被全灭自愈)。
    max_failures: u32,
    /// worker 后台配额轮询开关(热生效:设置面板改后 30s 内经 update_tuning 替换)。
    quota_poll_enabled: bool,
    /// 单请求换号重试硬上限(默认 2)。杜绝一个失败请求走遍全组(2026-06 雪崩防护)。
    max_switch_attempts: u32,
}

impl From<&SchedulerConfig> for Tuning {
    fn from(c: &SchedulerConfig) -> Self {
        Self {
            affinity_ttl: Duration::from_secs(c.affinity_ttl_secs.max(1)),
            rate_limit_cooldown: Duration::from_secs(c.rate_limit_cooldown_secs.max(1)),
            suspended_cooldown: Duration::from_secs(c.suspended_cooldown_secs.max(1)),
            empty_cooldown: Duration::from_secs(c.empty_response_cooldown_secs.max(1)),
            empty_window: Duration::from_secs(c.empty_response_window_secs.max(1)),
            empty_threshold: c.empty_response_threshold.max(1),
            max_failures: c.max_failures.max(1),
            quota_poll_enabled: c.quota_poll_enabled,
            max_switch_attempts: c.max_switch_attempts.max(1),
        }
    }
}

/// 账号被禁用/冷却的原因(决定自愈策略)。🟢 对齐 kiro.rs DisabledReason。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisabledReason {
    /// 连续 API 失败达阈值(全灭时可自愈重置)。
    TooManyFailures,
    /// 额度耗尽(QuotaExhausted,持久,不自动恢复)。
    QuotaExhausted,
    /// 429 限流,冷却到期自愈。
    RateLimited,
    /// 账号被上游临时封禁/暂停(TEMPORARILY_SUSPENDED),较长冷却到期自愈。
    /// 与 RateLimited 区分:冷却更长(别每 5min 重戳封禁号产生异常指纹)、面板标识不同。
    TemporarilySuspended,
    /// 空响应达阈值,冷却到期自愈。
    EmptyResponse,
    /// refresh_token 永久失效(invalid_grant),持久。
    InvalidRefreshToken,
}

impl DisabledReason {
    /// 是否为可冷却自愈类(到期自动恢复)。其余为持久禁用(需人工/全灭自愈)。
    fn is_cooldown(self) -> bool {
        matches!(
            self,
            DisabledReason::RateLimited
                | DisabledReason::TemporarilySuspended
                | DisabledReason::EmptyResponse
        )
    }
}

/// 单账号运行态(并发槽 + 禁用/冷却 + LRU + 失败计数)。🟢 对齐 kiro.rs CredentialEntry。
struct CredentialState {
    /// 账号配置(含 extra 凭证字段)。刷新后由调度器整体替换为带新 token 的副本。
    account: Arc<Account>,
    /// 单号并发上限信号量(容量 = account.max_concurrency)。
    semaphore: Arc<Semaphore>,
    /// 优先级(数值越小越优先);来自 account.extra.priority,缺省 100。
    priority: i64,
    /// 是否禁用(冷却中也算禁用,到期自愈)。
    disabled: bool,
    /// 禁用原因(决定自愈)。
    disabled_reason: Option<DisabledReason>,
    /// 最近一次从配置(DB)看到的 disabled 值。sync 只在**翻转**时动 runtime:
    /// 配置 false→false 不得反复清掉运行时冷却/封禁(admin 显式开关才算意图)。
    config_disabled: bool,
    /// 内存 extra 比 DB 新且尚未持久化成功(刷新回写失败时置位)。
    /// 置位期间 sync 不得用 DB 旧值覆盖内存(否则丢已 roll 的 token),
    /// 由 worker 的 sync 循环负责重试持久化、成功后清位。
    extra_dirty: bool,
    /// 冷却到期时刻(仅 cooldown 类有效);到点后选号前 sweep 自愈。
    disabled_until: Option<Instant>,
    /// 连续 API 失败次数(成功清零;达 tuning.max_failures 禁用)。
    failure_count: u32,
    /// 会话亲和 LRU:最后被选中的时刻(选中即更新,新会话按"最久未用优先"分配)。
    last_selected_at: Option<Instant>,
    /// 空响应固定窗口起点(v58 阈值冷却)。
    empty_window_start: Option<Instant>,
    /// 当前窗口内 empty 次数。
    empty_count_in_window: u32,
}

impl CredentialState {
    fn new(account: Arc<Account>) -> Self {
        let concurrency = account.max_concurrency.max(1) as usize;
        let priority = account
            .extra
            .get("priority")
            .and_then(|v| v.as_i64())
            .unwrap_or(100);
        Self {
            semaphore: Arc::new(Semaphore::new(concurrency)),
            priority,
            disabled: account.disabled,
            config_disabled: account.disabled,
            extra_dirty: false,
            disabled_reason: None,
            disabled_until: None,
            failure_count: 0,
            last_selected_at: None,
            empty_window_start: None,
            empty_count_in_window: 0,
            account,
        }
    }
}

/// 单账号运行态快照(worker /status → admin 账号页)。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AccountStatusSnapshot {
    pub account_id: String,
    pub priority: i64,
    pub disabled: bool,
    /// 禁用原因:rate_limited / empty_response / quota_exhausted /
    /// invalid_refresh_token / too_many_failures / config('' = 正常)。
    pub reason: String,
    /// 冷却剩余秒(仅冷却类 > 0)。
    pub cooldown_remaining_secs: u64,
    pub failure_count: u32,
    /// 当前空闲并发许可数(max_concurrency - 在途)。
    pub available_permits: usize,
    pub max_concurrency: u32,
}

/// [`AccountScheduler::sync_accounts`] 的变更统计(日志用)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    pub added: usize,
    pub removed: usize,
}

/// 老会话「向上迁移」去抖窗口:一次跨层迁回高层后,距上次迁移不足此窗口不再迁。
/// 把跨层横跳频率硬上限到每会话 ≤1 次/窗口——防止高层在饱和线附近抖动时,会话被
/// 「上迁 → 瞬时挤下 → 上迁」反复拉扯(重演 v52 橡皮筋 cache 崩,见 affinity-rubber-band-v52)。
/// 被挤下的那段时间会话稳定服务于低层(缓存热),窗口过后才再尝试回高层。
const MIGRATE_UP_DEBOUNCE: Duration = Duration::from_secs(60);

/// `(账号, 模型)` 不可用标记(INVALID_MODEL_ID)的存活时长:到期后重新放行该号服务该
/// 模型(重探一次)。AWS 把新模型滚动到该区域后自动恢复;重探即使仍不支持也只失败 1 次
/// 即再标记,代价极小。6h 足够低频重探又不长期误锁。
const MODEL_UNAVAILABLE_TTL: Duration = Duration::from_secs(6 * 3600);

/// 模型级过载窗口时长。收到显式 `MODEL_TEMPORARILY_UNAVAILABLE` 后的这段时间内,
/// 该模型的通用 5xx 也按过载处理(见 `AccountScheduler::model_overloaded`)。
///
/// 取 60s 的依据:2026-07-25 实测事故是**分钟级**成簇的(35 个有 5xx 的分钟里 19 个两种
/// 报文并存),60s 刚好覆盖一簇。取太长会把上游真内部错误也长期误判成过载;取太短
/// (如 5s)则簇内空隙就漏掉,退回逐个禁号。
const MODEL_OVERLOAD_WINDOW: Duration = Duration::from_secs(60);

/// 会话亲和记录:session_key → 当前 primary 账号(带 TTL 淘汰)。
/// v52:只留 primary + last_access,删 alt/streak(「落在哪个号就认哪个号」)。
struct AffinityEntry {
    /// 当前主账号 id(迁移后即更新,不回弹)。
    primary: String,
    /// 最后访问时刻(TTL 淘汰用)。
    last_access: Instant,
    /// 上次「向上迁移」时刻(去抖用;None=从未迁移)。见 [`MIGRATE_UP_DEBOUNCE`]。
    last_upgrade: Option<Instant>,
}

/// 一次成功选号的租约:选中的账号 + 并发许可(持有期间占用并发槽,Drop 即释放)。
pub struct AccountLease {
    /// 选中账号(已确保未禁用;token 有效性由 worker 的 ensure_credentialed 兜底)。
    pub account: Arc<Account>,
    /// 并发许可:持有到响应流结束;Drop 自动归还信号量。
    _permit: OwnedSemaphorePermit,
}

impl AccountLease {
    /// 选中账号的 id(用于成功/失败上报)。
    pub fn account_id(&self) -> &str {
        &self.account.account_id
    }
}

/// 选号失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcquireError {
    /// 组内全部账号禁用(无 cooldown 可救)。
    AllDisabled,
    /// 有可用账号但并发全满,且已等待重试到上限。
    AllBusy,
    /// 组内无任何账号。
    Empty,
    /// 组内没有任何账号支持请求的模型(如全 FREE 订阅请求 opus)。
    /// 与 AllDisabled 区分:这不是故障,是订阅能力不足,换时间重试也无济于事。
    NoModelSupport,
    /// 影子组(低价档)的档位守卫过滤后无可用账号:高优层此刻全被冷却/占满,
    /// 而低优兜底层**按设计**对本档不可见。
    ///
    /// 必须与 [`AcquireError::NoModelSupport`] 严格区分:后者被映射成 **400**
    /// (客户侧可解:换模型/升级订阅),而本变体是**运行时状态**、稍后重试即可恢复,
    /// 必须是 **503**。若把档位过滤混进 `supports` 谓词就会退化成 NoModelSupport→400,
    /// 客户端(SDK/NewAPI 对 400 不重试)会当成自己请求非法而放弃。
    TierExhausted,
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::AllDisabled => write!(f, "组内所有账号均已禁用"),
            AcquireError::AllBusy => write!(f, "组内所有账号并发已满"),
            AcquireError::Empty => write!(f, "组内无账号"),
            AcquireError::NoModelSupport => {
                write!(
                    f,
                    "组内无可服务该模型的账号(订阅等级不足如 FREE 不支持 opus,\
                     或该模型在账号区域/档位未上线)"
                )
            }
            AcquireError::TierExhausted => {
                write!(f, "本档位可用账号此刻全部繁忙或冷却中,请稍后重试")
            }
        }
    }
}

/// 影子组(低价档)的档位守卫:限制**本次请求可见的账号子集**。
///
/// 设计要点(改动前先读):
/// 1. **必须独立于 `supports` 谓词**。`supports` 表达的是"账号能否服务该模型"(近乎静态、
///    客户侧可解),守卫表达的是"本档位准不准用这个号"(运行时状态、稍后可恢复)。
///    合并二者会让守卫过滤后的空集退化成 `NoModelSupport` → 400,语义完全错误。
/// 2. **只用于挑号,不参与自愈**。见 `acquire_tiered` 里 `heal_too_many_failures` 的处理:
///    守卫导致的空集提前 return,绝不触发全灭自愈——否则每个被守卫挡下的低价请求都会把
///    正常组刚合法禁用的号复活并清零失败计数,连续失败保护对所有人失效。
/// 3. **判据必须是单调/稳定量**。两个边界只在 admin 改配置时变,所以会话亲和不会
///    因它反复重钉。**不要**把 `available_permits()` 这类抖动量塞进来:primary 一失格就会
///    走"改选并当场转正、永不迁回"(见 `select_id`),抖动量会让会话反复换号、缓存冷启动,
///    反而放大额度消耗。
///
/// 准入区间是**闭区间** `[min_priority, max_priority]`,两端各自可空。数值越小越优先,
/// 所以两个方向是两种截然不同的档位:
/// - `max = Some(0)`:只看主力号(与主力共享额度,限流了也认)。
/// - `min = Some(1)`:**只看小号**——主力号对本档根本不存在,低价流量烧不到它们。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TierGuard {
    /// 只允许 `priority >= min_priority` 的账号(**排除**比它更优先的号)。
    /// `None` = 下界不限。
    pub min_priority: Option<i64>,
    /// 只允许 `priority <= max_priority` 的账号(数值越小越优先)。
    /// `None` = 上界不限。
    pub max_priority: Option<i64>,
}

impl TierGuard {
    /// 本账号是否被本档位准入(两端边界都满足才算)。
    fn admits(&self, e: &CredentialState) -> bool {
        self.min_priority.is_none_or(|floor| e.priority >= floor)
            && self.max_priority.is_none_or(|cap| e.priority <= cap)
    }
}

/// worker 组内账号调度器:会话亲和选号 + 并发控制 + 冷却/禁用生命周期。
///
/// 线程安全(内部 Mutex),被 `Arc` 多请求共享。**一个 worker 一个实例**,只管本组账号。
pub struct AccountScheduler {
    /// account_id → 运行态。HashMap 以 id 索引(选号在锁内遍历,组规模小,O(n) 可接受)。
    entries: Mutex<HashMap<String, CredentialState>>,
    /// 会话亲和映射:session_key → primary。
    affinity: Mutex<HashMap<String, AffinityEntry>>,
    /// 调度参数(可热更:admin 设置面板改后 worker 30s 轮询经 [`Self::update_tuning`] 替换;
    /// **已生效的冷却**用绝对 `Instant`,不受改参影响,只影响其后新设的冷却/阈值判定)。
    tuning: RwLock<Tuning>,
    /// `(账号, 模型)` → 该号返回过 `INVALID_MODEL_ID`(模型在其区域/订阅档未上线)的时刻。
    /// 选号时把命中且未过 [`MODEL_UNAVAILABLE_TTL`] 的 `(号,模型)` 从合格集剔除,让请求
    /// 路由到有该模型的号,同时**不禁用**该号(它仍能服务其它模型)。TTL 到期后重新放行
    /// (AWS 区域上线新模型后自动恢复);仅内存,重启即清、重新学习。规模小(仅区域受限对)。
    model_unavailable: Mutex<HashMap<(String, String), Instant>>,
    /// **模型级过载窗口**:`模型 → 最近一次收到上游显式过载信号的时刻`。
    ///
    /// 用途见 gw-app `correct_overload_kind`:上游有时报容量不足会带
    /// `MODEL_TEMPORARILY_UNAVAILABLE`,有时只给 `reason:null` 的通用 5xx。实测
    /// 2026-07-25 的 176 条通用 5xx 里 **84.7% 与显式过载出现在同一分钟**,是同一现象。
    /// 于是拿显式信号当真相源:该模型处于窗口内时,通用 5xx 也按过载处理(不惩罚账号 +
    /// 同号退避);窗口外仍是 `ServerError`(**绝不靠猜**——重分类必须有上游显式信号背书)。
    ///
    /// 仅内存,重启即清、重新学习。键规模 = 模型数(个位数)。
    model_overloaded: Mutex<HashMap<String, Instant>>,
}

impl AccountScheduler {
    /// 用一组账号 + 调度参数构造调度器(参数来自 system.yaml `scheduler` 段)。
    pub fn new(accounts: Vec<Arc<Account>>, cfg: &SchedulerConfig) -> Self {
        let mut entries = HashMap::with_capacity(accounts.len());
        for acc in accounts {
            entries.insert(acc.account_id.clone(), CredentialState::new(acc));
        }
        Self {
            entries: Mutex::new(entries),
            affinity: Mutex::new(HashMap::new()),
            tuning: RwLock::new(Tuning::from(cfg)),
            model_unavailable: Mutex::new(HashMap::new()),
            model_overloaded: Mutex::new(HashMap::new()),
        }
    }

    /// 热更新调度参数(worker 30s 轮询经 settings overlay 调用)。整体替换 Tuning 快照;
    /// 不动任何账号运行态(已生效冷却/失败计数保留),只影响其后的判定。
    pub fn update_tuning(&self, cfg: &SchedulerConfig) {
        *self.tuning.write() = Tuning::from(cfg);
    }

    /// 后台配额轮询当前是否启用(热值;worker 轮询每轮读它,设置面板改后 30s 内生效)。
    pub fn quota_poll_enabled(&self) -> bool {
        self.tuning.read().quota_poll_enabled
    }

    /// 组内账号总数。
    pub fn total(&self) -> usize {
        self.entries.lock().len()
    }

    /// 单请求换号重试硬上限(热值;worker 30s 轮询经 [`Self::update_tuning`] 替换)。
    /// 反雪崩:`messages()` 用它把单请求波及的账号数封顶,而非走遍全组。
    pub fn max_switch_attempts(&self) -> usize {
        self.tuning.read().max_switch_attempts as usize
    }

    /// 冷却自愈 sweep:RateLimited/EmptyResponse 且 disabled_until 已到期 → 重新启用。
    /// 在每轮选号前调用(对齐 kiro.rs acquire 开头的 sweep)。
    fn heal_cooldowns(entries: &mut HashMap<String, CredentialState>, now: Instant) {
        for e in entries.values_mut() {
            if e.disabled
                && e.disabled_reason.map(|r| r.is_cooldown()).unwrap_or(false)
                && e.disabled_until.map(|t| now >= t).unwrap_or(true)
            {
                tracing::info!(account = %e.account.account_id, "冷却到期,自动重新启用");
                e.disabled = false;
                e.disabled_reason = None;
                e.disabled_until = None;
            }
        }
    }

    /// 合格账号 id 集:未禁用 + 不在 exclude(busy)内 + 支持本次模型 + 通过档位守卫。
    /// `guard = None`(普通组)时与本特性上线前逐字节等价。
    fn eligible_ids(
        entries: &HashMap<String, CredentialState>,
        exclude: &HashSet<String>,
        supports: &dyn Fn(&Account) -> bool,
        guard: Option<&TierGuard>,
    ) -> Vec<String> {
        entries
            .values()
            .filter(|e| {
                !e.disabled
                    && !exclude.contains(&e.account.account_id)
                    && supports(&e.account)
                    && guard.is_none_or(|g| g.admits(e))
            })
            .map(|e| e.account.account_id.clone())
            .collect()
    }

    /// 分层 LRU:在合格集合里取**最高优先级层**(priority 最小),层内选 last_selected_at
    /// 最旧者(None 视为最久未用,平局按 id)。返回选中 id。调用方保证 ids 非空。
    fn tiered_lru(entries: &HashMap<String, CredentialState>, ids: &[String]) -> String {
        let top_priority = ids
            .iter()
            .filter_map(|id| entries.get(id).map(|e| e.priority))
            .min()
            .unwrap_or(i64::MAX);
        ids.iter()
            .filter(|id| entries.get(*id).map(|e| e.priority == top_priority).unwrap_or(false))
            .min_by(|a, b| {
                let ea = entries.get(*a).and_then(|e| e.last_selected_at);
                let eb = entries.get(*b).and_then(|e| e.last_selected_at);
                match (ea, eb) {
                    (None, None) => a.cmp(b),
                    (None, Some(_)) => std::cmp::Ordering::Less,
                    (Some(_), None) => std::cmp::Ordering::Greater,
                    (Some(x), Some(y)) => x.cmp(&y),
                }
            })
            .cloned()
            .unwrap_or_else(|| ids[0].clone())
    }

    /// 老会话向上迁移的目标:合格集合里「优先级严格高于 `worse_than` 且此刻**有空闲
    /// permit**」的号,层内 LRU 最旧者;无则 `None`(维持原 primary)。
    ///
    /// 关键用 `available_permits()` 而非仅「未禁用/未 busy」——高优先级层被并发占满
    /// (permit=0)时**不迁**,避免高层饱和(用户抱怨的常态)时每请求空跑
    /// select→try_lease busy→回退的浪费。仅当高层真有空位才迁并转正;跨层横跳频率由调用方
    /// [`MIGRATE_UP_DEBOUNCE`] 去抖硬上限。注:permit 的 select→try_lease 之间仍有 TOCTOU
    /// 竞态窗口(读到有空位但真取时被抢),失败由 acquire_where 的 exclude 重试兜底回落到
    /// 低层,不产生错误状态,仅偶发一次空跑。注:该竞态失败路径也会烧掉 `last_upgrade`
    /// (置位在迁移决策点、早于 try_lease),故不破坏去抖上限——被挤下后 60s 内不再重迁。
    fn best_available_higher(
        entries: &HashMap<String, CredentialState>,
        ids: &[String],
        worse_than: i64,
    ) -> Option<String> {
        let cands: Vec<String> = ids
            .iter()
            .filter(|id| {
                entries
                    .get(*id)
                    .is_some_and(|e| e.priority < worse_than && e.semaphore.available_permits() > 0)
            })
            .cloned()
            .collect();
        if cands.is_empty() {
            None
        } else {
            Some(Self::tiered_lru(entries, &cands))
        }
    }

    /// 按会话亲和选一个账号 id(v52「落在哪个号就认哪个号」),并更新亲和表 + last_selected。
    /// `session_key = None` 时退化为分层 LRU(无亲和记忆)。`exclude` = 本轮已 busy 的号。
    /// `supports` = 模型能力过滤:primary 不支持本次模型时同样走「改选 + 当场转正」。
    /// `guard` = 影子组档位过滤(`None` = 普通组,行为不变)。
    fn select_id(
        &self,
        session_key: Option<&str>,
        exclude: &HashSet<String>,
        now: Instant,
        supports: &dyn Fn(&Account) -> bool,
        guard: Option<&TierGuard>,
    ) -> Option<String> {
        let mut entries = self.entries.lock();
        Self::heal_cooldowns(&mut entries, now);
        let eligible = Self::eligible_ids(&entries, exclude, supports, guard);
        if eligible.is_empty() {
            return None;
        }

        let chosen = match session_key {
            None => Self::tiered_lru(&entries, &eligible),
            Some(key) => {
                let affinity_ttl = self.tuning.read().affinity_ttl;
                let mut map = self.affinity.lock();
                map.retain(|_, v| now.duration_since(v.last_access) < affinity_ttl);
                let eligible_set: HashSet<&String> = eligible.iter().collect();
                match map.get_mut(key) {
                    None => {
                        let id = Self::tiered_lru(&entries, &eligible);
                        map.insert(
                            key.to_string(),
                            AffinityEntry { primary: id.clone(), last_access: now, last_upgrade: None },
                        );
                        id
                    }
                    Some(ent) => {
                        ent.last_access = now;
                        if eligible_set.contains(&ent.primary) {
                            // primary 当下可用。同层保持 v52 粘着(缓存热);但若 primary 落在
                            // 低优先级层、而此刻更高层**有空闲 permit**,则向上迁移一次并转正
                            // ——让高优先级号被积极使用。高层饱和 / 已在最高层 → 维持粘着。
                            // 去抖:距上次向上迁移不足 MIGRATE_UP_DEBOUNCE 则**不再迁**(cooled=false
                            // 时连 best_available_higher 扫描都跳过),把跨层横跳频率硬上限到
                            // 1 次/窗口/会话,防高层饱和线附近的橡皮筋抖动(见该常量注释)。
                            let primary_priority = entries
                                .get(&ent.primary)
                                .map(|e| e.priority)
                                .unwrap_or(i64::MAX);
                            let cooled = ent
                                .last_upgrade
                                .map_or(true, |t| now.duration_since(t) >= MIGRATE_UP_DEBOUNCE);
                            let target = if cooled {
                                Self::best_available_higher(&entries, &eligible, primary_priority)
                            } else {
                                None
                            };
                            match target {
                                Some(higher) => {
                                    ent.primary = higher.clone();
                                    ent.last_upgrade = Some(now);
                                    higher
                                }
                                None => ent.primary.clone(),
                            }
                        } else {
                            // primary 不可用 → 立即改选并当场转正,永不迁回(下沉/同层横切同理)。
                            let id = Self::tiered_lru(&entries, &eligible);
                            ent.primary = id.clone();
                            id
                        }
                    }
                }
            }
        };

        // 选中即更新 last_selected_at(让 LRU 反映实时负载,新会话才轮转)。
        if let Some(e) = entries.get_mut(&chosen) {
            e.last_selected_at = Some(now);
        }
        Some(chosen)
    }

    /// 取选中账号的并发许可 + 账号副本。permit 满返回 `Ok(None)`(调用方标 busy 重试);
    /// 账号不存在返回 `Ok(None)`。
    fn try_lease(&self, id: &str) -> Option<AccountLease> {
        let (sem, account) = {
            let entries = self.entries.lock();
            let e = entries.get(id)?;
            (e.semaphore.clone(), e.account.clone())
        };
        match sem.try_acquire_owned() {
            Ok(permit) => Some(AccountLease { account, _permit: permit }),
            Err(_) => None,
        }
    }

    /// 选号 + 取并发许可,**无模型过滤**(等价 `acquire_where(_, |_| true)`)。
    /// 供测试与无能力差异的 provider 使用;worker 主链路走 [`Self::acquire_where`]。
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn acquire(&self, session_key: Option<&str>) -> Result<AccountLease, AcquireError> {
        self.acquire_where(session_key, |_| true).await
    }

    /// 选号 + 取并发许可(v52 亲和 + 模型能力过滤)。返回租约;持有期间占并发槽,Drop 释放。
    ///
    /// `supports` 判定账号能否服务本次模型(来自 `Provider::account_supports_model`,
    /// 对齐 kiro.rs opus 过滤):不支持的号从合格集剔除,绝不会被选中——否则 FREE 号
    /// 接 opus 被上游 403 → 误判 TokenInvalid → 永久禁用健康号。
    ///
    /// 流程(对齐 kiro.rs acquire_context_with_session_and_group):
    /// 1. 冷却 sweep + 分层 LRU 亲和选号(合格 = 未禁用 + 非 busy + 支持模型);
    /// 2. 取并发许可,满了把该号记 busy、换下一个号;
    /// 3. 全 busy 但有可用号 → 短 sleep 等并发释放后重试;
    /// 4. 无任何号支持该模型 → NoModelSupport(换时间重试无济于事,与故障区分);
    /// 5. 全禁用且有 TooManyFailures → 全灭自愈(重置失败计数)再试一轮;否则报错。
    pub async fn acquire_where<F>(
        &self,
        session_key: Option<&str>,
        supports: F,
    ) -> Result<AccountLease, AcquireError>
    where
        F: Fn(&Account) -> bool,
    {
        self.acquire_tiered(session_key, supports, None).await
    }

    /// [`Self::acquire_where`] + 影子组档位守卫。`guard = None` 时与前者**逐字节等价**
    /// (这是"新增低价档不影响现有分组"的机械保证,见测试
    /// `guard_none_matches_acquire_where_exactly`)。
    ///
    /// 守卫与 `supports` 的两点关键差异,别合并二者(见 [`TierGuard`] 文档):
    /// - 守卫过滤后的空集报 [`AcquireError::TierExhausted`](503,可重试),
    ///   而非 `NoModelSupport`(400,客户端不重试);
    /// - **带守卫的请求绝不触发全灭自愈**:否则每个被守卫挡下的低价请求都会把正常组
    ///   刚合法禁用的号复活并清零失败计数,连续失败保护对所有档位一起失效。
    pub async fn acquire_tiered<F>(
        &self,
        session_key: Option<&str>,
        supports: F,
        guard: Option<&TierGuard>,
    ) -> Result<AccountLease, AcquireError>
    where
        F: Fn(&Account) -> bool,
    {
        let total = self.total();
        if total == 0 {
            return Err(AcquireError::Empty);
        }
        // 尝试预算与 max_failures(禁用阈值)**解耦**:复用会让运维把 max_failures
        // 调到 1 时 busy 几乎不等待、全灭自愈后没机会重选(审查 Architect#6/Minimalist#3)。
        const ACQUIRE_ATTEMPTS_PER_ACCOUNT: usize = 5;
        let max_attempts = (total * ACQUIRE_ATTEMPTS_PER_ACCOUNT).max(2);
        let mut attempts = 0;
        let mut busy: HashSet<String> = HashSet::new();
        let mut self_healed = false;

        loop {
            if attempts >= max_attempts {
                return Err(AcquireError::AllBusy);
            }
            let now = Instant::now();

            let Some(id) = self.select_id(session_key, &busy, now, &supports, guard) else {
                // 无合格号:区分"没号支持该模型" vs "本档位无号" vs "全 busy(有可用但
                // 占满)" vs "全禁用"。计数都只看**支持该模型**的号——不支持的号既救不了
                // busy 等待,也不该让错误从 NoModelSupport 误报成 AllDisabled。
                // `tier_any`/`avail_*` 额外过一遍守卫:守卫外的号对本档不存在。
                let (supported_any, tier_any, avail_total, avail_not_busy) = {
                    let entries = self.entries.lock();
                    let mut any = false;
                    let mut tier_any = false;
                    let mut avail = 0usize;
                    let mut not_busy = 0usize;
                    for e in entries.values() {
                        if !supports(&e.account) {
                            continue;
                        }
                        any = true;
                        if !guard.is_none_or(|g| g.admits(e)) {
                            continue;
                        }
                        tier_any = true;
                        if e.disabled {
                            continue;
                        }
                        avail += 1;
                        if !busy.contains(&e.account.account_id) {
                            not_busy += 1;
                        }
                    }
                    (any, tier_any, avail, not_busy)
                };
                if !supported_any {
                    return Err(AcquireError::NoModelSupport);
                }
                // 本档位一个号都没有(配置错/档位卡太严):是配置态而非瞬时故障,但仍
                // 报可重试的 TierExhausted——运维改一下 tier_max_priority 就恢复,
                // 不该让客户端拿到"换模型才能解决"的 400。
                if !tier_any {
                    return Err(AcquireError::TierExhausted);
                }
                if avail_total > 0 && avail_not_busy == 0 && !busy.is_empty() {
                    // 有可用号但全 busy → 等并发释放后重试。
                    busy.clear();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    attempts += 1;
                    continue;
                }
                // 带守卫(低价档)到此为止:高优层此刻全冷却/占满,而低优兜底层按设计
                // 对本档不可见。**绝不下探、也绝不触发全灭自愈**(自愈是全局的,会把正常组
                // 刚合法禁用的号一起复活)。稍后重试即可恢复 → 503。
                if guard.is_some() {
                    return Err(AcquireError::TierExhausted);
                }
                // 全禁用:若有 TooManyFailures,做一次全灭自愈(等价重启)再试。
                // 只复活**支持本次模型**的号:opus 请求不该顺手复活无关 FREE 失败号
                // (与上方"计数只看支持该模型"同语义,审查②R Skeptic#4)。
                if !self_healed && self.heal_too_many_failures(&supports) {
                    self_healed = true;
                    attempts += 1;
                    continue;
                }
                return Err(AcquireError::AllDisabled);
            };

            match self.try_lease(&id) {
                Some(lease) => return Ok(lease),
                None => {
                    // 并发满:标 busy,换号重试。
                    busy.insert(id);
                    attempts += 1;
                }
            }
        }
    }

    /// 全灭自愈:若存在 TooManyFailures 禁用的号,清其禁用 + 失败计数(等价重启)。
    /// 只动 `supports` 通过(支持本次模型)的号。返回是否实际恢复了至少一个号。
    fn heal_too_many_failures(&self, supports: &dyn Fn(&Account) -> bool) -> bool {
        let mut entries = self.entries.lock();
        let mut healed = false;
        for e in entries.values_mut() {
            if e.disabled
                && e.disabled_reason == Some(DisabledReason::TooManyFailures)
                && supports(&e.account)
            {
                e.disabled = false;
                e.disabled_reason = None;
                e.failure_count = 0;
                healed = true;
            }
        }
        if healed {
            tracing::warn!("组内账号全因连续失败禁用,执行全灭自愈(重置失败计数)");
        }
        healed
    }

    /// 用 DB 最新配置同步账号集(后台周期调用,admin 增删改无需重启 worker):
    /// 新增建态、消失移除(并清其亲和)、已有替换配置副本但保留运行态;
    /// 配置 disabled **翻转**才动 runtime(→true 强制禁用,→false 视为 admin
    /// 显式复活,清运行时禁用)。
    pub fn sync_accounts(&self, accounts: Vec<Arc<Account>>) -> SyncOutcome {
        let mut out = SyncOutcome::default();
        let mut entries = self.entries.lock();

        let incoming: HashSet<String> =
            accounts.iter().map(|a| a.account_id.clone()).collect();
        let removed_ids: Vec<String> = entries
            .keys()
            .filter(|id| !incoming.contains(*id))
            .cloned()
            .collect();
        for id in &removed_ids {
            entries.remove(id);
            out.removed += 1;
        }

        for acc in accounts {
            match entries.get_mut(&acc.account_id) {
                None => {
                    tracing::info!(account = %acc.account_id, "sync:新增账号");
                    entries.insert(acc.account_id.clone(), CredentialState::new(acc));
                    out.added += 1;
                }
                Some(e) => {
                    // 配置 disabled 翻转才动 runtime(同值保持,避免周期 sync 洗状态)。
                    if acc.disabled != e.config_disabled {
                        e.config_disabled = acc.disabled;
                        e.disabled = acc.disabled;
                        e.disabled_reason = None;
                        e.disabled_until = None;
                        if acc.disabled {
                            tracing::info!(account = %acc.account_id, "sync:配置禁用");
                        } else {
                            tracing::info!(account = %acc.account_id, "sync:配置启用(清运行时禁用)");
                            e.failure_count = 0;
                        }
                    }
                    // 内存 extra 未持久化(刷新回写失败):跳过配置覆盖,保住新 token;
                    // disabled 翻转仍生效(上方已处理),持久化由 worker 重试后清位。
                    if e.extra_dirty {
                        continue;
                    }
                    // 并发上限变化 → 换新信号量(在途许可持旧信号量,自然衰减)。
                    if acc.max_concurrency != e.account.max_concurrency {
                        e.semaphore =
                            Arc::new(Semaphore::new(acc.max_concurrency.max(1) as usize));
                    }
                    e.priority = acc
                        .extra
                        .get("priority")
                        .and_then(|v| v.as_i64())
                        .unwrap_or(100);
                    e.account = acc;
                }
            }
        }
        drop(entries);

        // 清掉指向已移除账号的亲和(下次该会话自然重选)。
        if !removed_ids.is_empty() {
            let removed: HashSet<&String> = removed_ids.iter().collect();
            self.affinity
                .lock()
                .retain(|_, v| !removed.contains(&v.primary));
            // 一并清掉已移除账号的模型不可用标记(仿亲和清理,防陈旧项积压)。
            self.model_unavailable
                .lock()
                .retain(|(a, _), _| !removed.contains(a));
        }
        out
    }

    /// 标记某账号内存 extra 未持久化(刷新回写 DB 失败时调用)。
    pub fn mark_extra_dirty(&self, id: &str) {
        if let Some(e) = self.entries.lock().get_mut(id) {
            e.extra_dirty = true;
        }
    }

    /// 取所有待持久化账号的内存副本(worker sync 循环重试回写用)。
    pub fn dirty_accounts(&self) -> Vec<Arc<Account>> {
        self.entries
            .lock()
            .values()
            .filter(|e| e.extra_dirty)
            .map(|e| e.account.clone())
            .collect()
    }

    /// 持久化成功后清除脏标记。
    pub fn clear_extra_dirty(&self, id: &str) {
        if let Some(e) = self.entries.lock().get_mut(id) {
            e.extra_dirty = false;
        }
    }

    /// 该账号是否有待持久化的脏 extra。
    pub fn is_extra_dirty(&self, id: &str) -> bool {
        self.entries.lock().get(id).map(|e| e.extra_dirty).unwrap_or(false)
    }

    /// 全账号运行态快照(worker /status → admin 账号页;id 升序稳定输出)。
    /// 先做冷却自愈 sweep:无流量时快照才不会展示已到期的陈旧冷却态。
    pub fn status_snapshot(&self) -> Vec<AccountStatusSnapshot> {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        Self::heal_cooldowns(&mut entries, now);
        let entries = &*entries;
        let mut snap: Vec<AccountStatusSnapshot> = entries
            .values()
            .map(|e| {
                let reason = match e.disabled_reason {
                    Some(DisabledReason::RateLimited) => "rate_limited",
                    Some(DisabledReason::TemporarilySuspended) => "temporarily_suspended",
                    Some(DisabledReason::EmptyResponse) => "empty_response",
                    Some(DisabledReason::QuotaExhausted) => "quota_exhausted",
                    Some(DisabledReason::InvalidRefreshToken) => "invalid_refresh_token",
                    Some(DisabledReason::TooManyFailures) => "too_many_failures",
                    None if e.disabled => "config",
                    None => "",
                };
                AccountStatusSnapshot {
                    account_id: e.account.account_id.clone(),
                    priority: e.priority,
                    disabled: e.disabled,
                    reason: reason.to_string(),
                    cooldown_remaining_secs: e
                        .disabled_until
                        .map(|t| t.saturating_duration_since(now).as_secs())
                        .unwrap_or(0),
                    failure_count: e.failure_count,
                    available_permits: e.semaphore.available_permits(),
                    max_concurrency: e.account.max_concurrency,
                }
            })
            .collect();
        snap.sort_by(|a, b| a.account_id.cmp(&b.account_id));
        snap
    }

    /// 刷新后回写账号(带新 token 的副本),供下次选号使用。保留运行态(并发/计数/LRU)。
    pub fn update_account(&self, account: Arc<Account>) {
        let mut entries = self.entries.lock();
        if let Some(e) = entries.get_mut(&account.account_id) {
            e.account = account;
        }
    }

    /// **原子**替换账号副本并置脏(同一把 entries 锁)。刷新回写专用:
    /// 「先 update 后 mark_dirty」两步之间,30s sync 看到 dirty=false 会用 DB 旧值
    /// 覆盖内存,丢掉刚 roll 的 refresh_token(审查 Architect#1)。
    pub fn update_account_dirty(&self, account: Arc<Account>) {
        let mut entries = self.entries.lock();
        if let Some(e) = entries.get_mut(&account.account_id) {
            e.account = account;
            e.extra_dirty = true;
        }
    }

    /// 锁内就地合并**单个** extra 字段(配额回填 subscription_title 等元数据用)。
    /// 与 [`Self::update_account`] 的整体替换不同:不携带调用方的旧账号快照,
    /// 不会与并发 token 刷新互相覆盖。值未变化返回 false(调用方可跳过持久化)。
    pub fn merge_extra(&self, id: &str, key: &str, value: serde_json::Value) -> bool {
        let mut entries = self.entries.lock();
        let Some(e) = entries.get_mut(id) else { return false };
        if e.account.extra.get(key) == Some(&value) {
            return false;
        }
        let mut acc = (*e.account).clone();
        acc.extra.insert(key.to_string(), value);
        e.account = Arc::new(acc);
        true
    }

    /// 人工救号(admin reset):清运行时禁用/冷却/全部计数,立即回到选号池。
    /// 配置层禁用(admin 开关 disabled=true)**不**在此解除——那是显式运营意图,
    /// 走 PATCH disabled=false。返回是否找到该账号。
    pub fn reset_account(&self, id: &str) -> bool {
        let mut entries = self.entries.lock();
        let Some(e) = entries.get_mut(id) else { return false };
        e.disabled = e.config_disabled;
        e.disabled_reason = None;
        e.disabled_until = None;
        e.failure_count = 0;
        e.empty_window_start = None;
        e.empty_count_in_window = 0;
        tracing::info!(account = %id, "admin reset:清运行时禁用与计数");
        true
    }

    /// 读当前某账号的凭证副本(单飞刷新的二次检查用:他人可能刚刷新好)。
    pub fn account(&self, id: &str) -> Option<Arc<Account>> {
        self.entries.lock().get(id).map(|e| e.account.clone())
    }

    /// 上报成功:清失败计数。
    pub fn report_success(&self, id: &str) {
        let mut entries = self.entries.lock();
        if let Some(e) = entries.get_mut(id) {
            e.failure_count = 0;
        }
    }

    /// 上报一次失败,按 [`UpstreamErrorKind`] 映射生命周期动作(冷却/禁用/计数)。
    /// 🟢 对齐 kiro.rs report_failure / report_rate_limited / report_empty_response /
    /// report_quota_exhausted / report_refresh_token_invalid。
    pub fn report_failure(&self, id: &str, kind: UpstreamErrorKind) {
        let now = Instant::now();
        let tuning = *self.tuning.read();
        let mut entries = self.entries.lock();
        let Some(e) = entries.get_mut(id) else { return };
        // 已禁用(含手动/额度)不覆盖原因,幂等。
        if e.disabled {
            return;
        }
        match kind {
            UpstreamErrorKind::RateLimited => {
                e.disabled = true;
                e.disabled_reason = Some(DisabledReason::RateLimited);
                e.disabled_until = Some(now + tuning.rate_limit_cooldown);
                tracing::warn!(account = %id, "命中限流,冷却 {}s",
                    tuning.rate_limit_cooldown.as_secs());
            }
            UpstreamErrorKind::TemporarilyBlocked => {
                // 账号被上游临时封禁:较长冷却(默认 1h),到期自愈再试、仍封则再冷却。
                // 不换号(worth_switching_account=false)——杜绝把封禁请求扩散到健康号;
                // 比限流长是为了别每 5min 重戳封禁号、产生异常调用指纹加剧风控。
                e.disabled = true;
                e.disabled_reason = Some(DisabledReason::TemporarilySuspended);
                e.disabled_until = Some(now + tuning.suspended_cooldown);
                tracing::warn!(account = %id, "账号被上游临时封禁,冷却 {}s",
                    tuning.suspended_cooldown.as_secs());
            }
            UpstreamErrorKind::EmptyResponse => {
                // v58 固定窗口阈值:窗口内累计达阈值才冷却,避免误伤偶发 empty 的健康号。
                match e.empty_window_start {
                    Some(start) if now.duration_since(start) <= tuning.empty_window => {
                        e.empty_count_in_window += 1;
                    }
                    _ => {
                        e.empty_window_start = Some(now);
                        e.empty_count_in_window = 1;
                    }
                }
                if e.empty_count_in_window >= tuning.empty_threshold {
                    e.disabled = true;
                    e.disabled_reason = Some(DisabledReason::EmptyResponse);
                    e.disabled_until = Some(now + tuning.empty_cooldown);
                    e.empty_window_start = None;
                    e.empty_count_in_window = 0;
                    tracing::warn!(account = %id, "空响应达阈值,冷却 {}s",
                        tuning.empty_cooldown.as_secs());
                }
            }
            UpstreamErrorKind::QuotaExhausted => {
                e.disabled = true;
                e.disabled_reason = Some(DisabledReason::QuotaExhausted);
                e.disabled_until = None;
                tracing::warn!(account = %id, "额度耗尽,禁用");
            }
            UpstreamErrorKind::TokenInvalid => {
                // refresh_token 永久失效:立即禁用(worker 区分 access vs refresh 失效;
                // 这里收到的是已判定的永久失效)。
                e.disabled = true;
                e.disabled_reason = Some(DisabledReason::InvalidRefreshToken);
                e.disabled_until = None;
                tracing::error!(account = %id, "refresh_token 永久失效,禁用");
            }
            UpstreamErrorKind::ServerError | UpstreamErrorKind::Network | UpstreamErrorKind::Other => {
                e.failure_count += 1;
                if e.failure_count >= tuning.max_failures {
                    e.disabled = true;
                    e.disabled_reason = Some(DisabledReason::TooManyFailures);
                    tracing::warn!(account = %id, "连续失败 {} 次,自动禁用", e.failure_count);
                }
            }
            // BadRequest 是请求本身问题,不惩罚账号。
            // ModelNotAvailable(该号不支持此模型)同样**不惩罚账号**——它仍能服务其它模型;
            // 换号 + `(账号,模型)` 不可用标记由调用方处理(见 [`Self::mark_model_unavailable`])。
            // Overloaded(上游模型级容量不足)也**不惩罚账号**:2026-07-25 opus-5 事故正是把它
            // 当 ServerError 记进 failure_count,35 秒内禁光 7 个健康号 + 触发全灭自愈;而禁用
            // 对**所有模型**生效,连带 opus-4-6/sonnet-5 一起挂。
            // 这里刻意**穷举**而非用 `spares_account_health()` 做守卫:守卫会让新增 kind 悄悄
            // 落进某个分支,穷举则编译不过、强迫做决策。两者一致性由
            // `spares_account_health_matches_no_penalty_arms` 测试锁住。
            UpstreamErrorKind::BadRequest
            | UpstreamErrorKind::ModelNotAvailable
            | UpstreamErrorKind::Overloaded => {}
        }
    }

    /// 标记 `(账号, 模型)` 不可用(收到 `INVALID_MODEL_ID` = `ModelNotAvailable` 时调用)。
    /// 后续选号把该 `(号,模型)` 从合格集剔除(见 [`Self::is_model_unavailable`]),路由到
    /// 有该模型的号,而**不禁用**该号(它仍服务其它模型)。
    pub fn mark_model_unavailable(&self, account_id: &str, model: &str) {
        self.model_unavailable
            .lock()
            .insert((account_id.to_string(), model.to_string()), Instant::now());
    }

    /// `(账号, 模型)` 是否在 [`MODEL_UNAVAILABLE_TTL`] 内被标记不可用(选号过滤谓词用)。
    /// 表规模小(仅区域受限对),线性扫描避免每次选号为查询分配 key String。
    pub fn is_model_unavailable(&self, account_id: &str, model: &str) -> bool {
        self.model_unavailable.lock().iter().any(|((a, m), t)| {
            a == account_id && m == model && t.elapsed() < MODEL_UNAVAILABLE_TTL
        })
    }

    /// 记一次该模型的**显式**上游过载信号(收到 `Overloaded` 时调用),开启
    /// [`MODEL_OVERLOAD_WINDOW`] 窗口。见字段 [`Self::model_overloaded`] 的说明。
    pub fn mark_model_overloaded(&self, model: &str) {
        self.model_overloaded
            .lock()
            .insert(model.to_string(), Instant::now());
    }

    /// 该模型当前是否处于过载窗口内(距上次显式过载信号 < [`MODEL_OVERLOAD_WINDOW`])。
    pub fn is_model_overloaded(&self, model: &str) -> bool {
        self.model_overloaded
            .lock()
            .get(model)
            .is_some_and(|t| t.elapsed() < MODEL_OVERLOAD_WINDOW)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn acct(id: &str, concurrency: u32, priority: Option<i64>) -> Arc<Account> {
        let mut extra = BTreeMap::new();
        if let Some(p) = priority {
            extra.insert("priority".to_string(), serde_json::json!(p));
        }
        Arc::new(Account {
            account_id: id.to_string(),
            provider: "kiro".into(),
            max_concurrency: concurrency,
            disabled: false,
            extra,
        })
    }

    fn sched(accounts: Vec<Arc<Account>>) -> AccountScheduler {
        AccountScheduler::new(accounts, &SchedulerConfig::default())
    }

    /// 默认连续失败禁用阈值(配置默认值,测试断言用)。
    fn max_failures() -> u32 {
        SchedulerConfig::default().max_failures
    }

    #[test]
    fn model_unavailable_marks_only_that_pair() {
        let s = sched(vec![acct("a", 2, None), acct("b", 2, None)]);
        assert!(!s.is_model_unavailable("a", "claude-sonnet-5"));
        s.mark_model_unavailable("a", "claude-sonnet-5");
        assert!(s.is_model_unavailable("a", "claude-sonnet-5"));
        // 只影响该 (号,模型) 对:同号其它模型、其它号同模型都不受影响。
        assert!(!s.is_model_unavailable("a", "claude-opus-4-8"));
        assert!(!s.is_model_unavailable("b", "claude-sonnet-5"));
    }

    #[test]
    fn model_not_available_never_disables_or_counts_failure() {
        let s = sched(vec![acct("a", 2, None)]);
        // 反复上报 ModelNotAvailable 也绝不禁用/不计失败(号仍能服务其它模型)。
        for _ in 0..(max_failures() + 3) {
            s.report_failure("a", UpstreamErrorKind::ModelNotAvailable);
        }
        let snap = s.status_snapshot();
        let a = snap.iter().find(|x| x.account_id == "a").unwrap();
        assert!(!a.disabled, "ModelNotAvailable 不得禁用账号");
        assert_eq!(a.failure_count, 0, "ModelNotAvailable 不得计失败");
    }

    #[tokio::test]
    async fn acquire_where_skips_model_unavailable_account() {
        // 标记 a 对 sonnet-5 不可用后,带该过滤谓词选号只落到 b(路由到有该模型的号),
        // 且 a 未被禁用——它仍可服务其它模型。
        let s = sched(vec![acct("a", 4, None), acct("b", 4, None)]);
        s.mark_model_unavailable("a", "claude-sonnet-5");
        let supports = |acc: &Account| !s.is_model_unavailable(&acc.account_id, "claude-sonnet-5");
        for i in 0..6 {
            let key = format!("s{i}");
            let lease = s.acquire_where(Some(&key), supports).await.unwrap();
            assert_eq!(lease.account_id(), "b", "sonnet-5 应绕开被标记的 a、落到 b");
        }
        // a 无过滤时仍可被选(未禁用):新会话可落到 a。
        assert!(!s.is_model_unavailable("a", "claude-opus-4-8"));
    }

    #[tokio::test]
    async fn empty_group_errors() {
        let s = sched(vec![]);
        assert_eq!(s.acquire(Some("k")).await.err(), Some(AcquireError::Empty));
    }

    #[tokio::test]
    async fn same_session_sticks_to_same_account() {
        let s = sched(vec![acct("a", 4, None), acct("b", 4, None), acct("c", 4, None)]);
        let first = s.acquire(Some("sess1")).await.unwrap().account_id().to_string();
        for _ in 0..6 {
            let again = s.acquire(Some("sess1")).await.unwrap();
            assert_eq!(again.account_id(), first, "同会话必须钉同账号(v52)");
        }
    }

    #[tokio::test]
    async fn new_sessions_spread_via_lru() {
        let s = sched(vec![acct("a", 4, None), acct("b", 4, None)]);
        let s1 = s.acquire(Some("s1")).await.unwrap().account_id().to_string();
        let s2 = s.acquire(Some("s2")).await.unwrap().account_id().to_string();
        assert_ne!(s1, s2, "两个新会话应分散到不同账号(LRU)");
    }

    #[tokio::test]
    async fn higher_priority_preferred_for_new_session() {
        let s = sched(vec![acct("low", 4, Some(200)), acct("high", 4, Some(1))]);
        let chosen = s.acquire(Some("s")).await.unwrap();
        assert_eq!(chosen.account_id(), "high", "新会话应选最高优先级层");
    }

    #[tokio::test]
    async fn primary_unavailable_promotes_new_and_sticks() {
        let s = sched(vec![acct("a", 1, Some(1)), acct("b", 4, Some(1))]);
        let lease_a = s.acquire(Some("s")).await.unwrap();
        assert_eq!(lease_a.account_id(), "a");
        let on_b = s.acquire(Some("s")).await.unwrap().account_id().to_string();
        assert_eq!(on_b, "b", "primary 不可用应立即转正到候选");
        drop(lease_a);
        let still_b = s.acquire(Some("s")).await.unwrap();
        assert_eq!(still_b.account_id(), "b", "转正后永不迁回原 primary");
    }

    #[tokio::test]
    async fn rate_limited_account_skipped_until_cooldown() {
        let s = sched(vec![acct("a", 4, Some(1)), acct("b", 4, Some(1))]);
        let id = s.acquire(Some("s")).await.unwrap().account_id().to_string();
        s.report_failure(&id, UpstreamErrorKind::RateLimited);
        let other = s.acquire(Some("s")).await.unwrap().account_id().to_string();
        assert_ne!(other, id, "限流号应被跳过");
    }

    #[tokio::test]
    async fn rate_limited_top_tier_descends_to_lower_priority_pool() {
        // 高优先级层两个号(priority=1)+ 低优先级兜底号(priority=100)。
        // 逐个把高优先级层限流后,acquire 应沿优先级阶梯下探到低优先级兜底号,而非无号可选。
        let s = sched(vec![
            acct("hi-1", 4, Some(1)),
            acct("hi-2", 4, Some(1)),
            acct("lo", 4, Some(100)),
        ]);
        let first = s.acquire(Some("s")).await.unwrap().account_id().to_string();
        assert!(first == "hi-1" || first == "hi-2", "先落最高优先级层: {first}");
        s.report_failure(&first, UpstreamErrorKind::RateLimited);
        let second = s.acquire(Some("s")).await.unwrap().account_id().to_string();
        assert!(
            (second == "hi-1" || second == "hi-2") && second != first,
            "高优先级层还有号时仍停在本层: {second}"
        );
        s.report_failure(&second, UpstreamErrorKind::RateLimited);
        // 高优先级层全冷却 → 下探到低优先级兜底池。
        let third = s.acquire(Some("s")).await.unwrap().account_id().to_string();
        assert_eq!(third, "lo", "高优先级层全限流后应下探到低优先级号");
    }

    #[tokio::test]
    async fn quota_exhausted_top_tier_descends_to_lower_priority_pool() {
        // 额度耗尽与限流同属"良性可下探":高优先级号额度用光后,acquire 落到低优先级兜底号。
        let s = sched(vec![acct("hi", 4, Some(1)), acct("lo", 4, Some(100))]);
        let first = s.acquire(Some("s")).await.unwrap().account_id().to_string();
        assert_eq!(first, "hi", "先落最高优先级层");
        s.report_failure(&first, UpstreamErrorKind::QuotaExhausted);
        let second = s.acquire(Some("s")).await.unwrap().account_id().to_string();
        assert_eq!(second, "lo", "高优先级号额度耗尽后应下探到低优先级号");
    }

    #[tokio::test]
    async fn low_tier_session_migrates_up_when_high_tier_frees() {
        // hi 仅 1 permit;先用别的会话占满 hi,逼会话 s 下沉到低层 lo。
        let s = sched(vec![acct("hi", 1, Some(1)), acct("lo", 4, Some(100))]);
        let occupy = s.acquire(Some("occupy")).await.unwrap();
        assert_eq!(occupy.account_id(), "hi", "占位会话先落最高层");
        let on_lo = s.acquire(Some("s")).await.unwrap();
        assert_eq!(on_lo.account_id(), "lo", "hi 满 → 会话 s 下沉低层并转正");
        drop(on_lo);
        drop(occupy); // 释放高层 permit
        let migrated = s.acquire(Some("s")).await.unwrap();
        assert_eq!(migrated.account_id(), "hi", "高层空出 → 老会话向上迁移到高层");
        drop(migrated);
        // 迁后钉住:再取仍是 hi(不横跳、不下沉)。
        let stay = s.acquire(Some("s")).await.unwrap();
        assert_eq!(stay.account_id(), "hi", "迁后钉在高层");
    }

    #[tokio::test]
    async fn high_tier_primary_stays_when_no_higher_available() {
        // 会话已在最高层:高层内不横跳(无更高层可迁),保持 v52 粘着。
        let s = sched(vec![acct("hi-1", 4, Some(1)), acct("hi-2", 4, Some(1))]);
        let first = s.acquire(Some("s")).await.unwrap().account_id().to_string();
        for _ in 0..5 {
            let again = s.acquire(Some("s")).await.unwrap();
            assert_eq!(again.account_id(), first, "已在最高层 → 粘着,不横跳");
        }
    }

    #[tokio::test]
    async fn no_upward_migration_when_high_tier_busy() {
        // hi 仅 1 permit 且被占位会话占满**不释放**:老会话落 lo 后不误迁到 permit=0 的 hi
        // (证明高层饱和时不空跑 select→busy)。
        let s = sched(vec![acct("hi", 1, Some(1)), acct("lo", 4, Some(100))]);
        let _occupy = s.acquire(Some("occupy")).await.unwrap();
        assert_eq!(_occupy.account_id(), "hi");
        let on_lo = s.acquire(Some("s")).await.unwrap().account_id().to_string();
        assert_eq!(on_lo, "lo", "hi 满 → 落低层");
        for _ in 0..3 {
            let again = s.acquire(Some("s")).await.unwrap();
            assert_eq!(again.account_id(), "lo", "高层 busy(permit=0)时不向上迁移");
        }
    }

    #[tokio::test]
    async fn upgraded_session_debounced_against_rethrash() {
        // 复现对抗审查 Finding 1 的橡皮筋场景并验证去抖:会话上迁到高层后被瞬时挤下低层,
        // MIGRATE_UP_DEBOUNCE(60s)窗口内不再立刻迁回——防高层饱和线附近的反复横跳(cache 崩)。
        let s = sched(vec![acct("hi", 1, Some(1)), acct("lo", 4, Some(100))]);
        // s 先被占位逼到 lo,再(首次,允许)迁回 hi。
        let occ = s.acquire(Some("occ")).await.unwrap();
        assert_eq!(occ.account_id(), "hi");
        assert_eq!(s.acquire(Some("s")).await.unwrap().account_id(), "lo");
        drop(occ);
        assert_eq!(
            s.acquire(Some("s")).await.unwrap().account_id(),
            "hi",
            "首次迁移:高层空出 → 迁回 hi(去抖 last_upgrade 置位)"
        );
        // 再占位逼 s 下沉 lo(既有「primary 不可用→转正」)。
        let occ2 = s.acquire(Some("occ2")).await.unwrap();
        assert_eq!(occ2.account_id(), "hi");
        assert_eq!(
            s.acquire(Some("s")).await.unwrap().account_id(),
            "lo",
            "primary hi 被占 → 下沉 lo"
        );
        drop(occ2); // hi 又空出
        // 去抖窗口内(距上次迁移 <60s)→ 不再立刻迁回,稳定停在 lo(反橡皮筋横跳)。
        for _ in 0..3 {
            assert_eq!(
                s.acquire(Some("s")).await.unwrap().account_id(),
                "lo",
                "去抖窗口内不重复向上迁移"
            );
        }
    }

    #[tokio::test]
    async fn quota_exhausted_disables_permanently() {
        let s = sched(vec![acct("a", 4, None)]);
        s.report_failure("a", UpstreamErrorKind::QuotaExhausted);
        assert_eq!(s.acquire(Some("s")).await.err(), Some(AcquireError::AllDisabled));
    }

    #[tokio::test]
    async fn empty_response_threshold_then_cooldown() {
        let s = sched(vec![acct("a", 4, None)]);
        s.report_failure("a", UpstreamErrorKind::EmptyResponse);
        s.report_failure("a", UpstreamErrorKind::EmptyResponse);
        assert!(s.acquire(Some("s")).await.is_ok(), "未达阈值不应禁用");
        s.report_failure("a", UpstreamErrorKind::EmptyResponse);
        assert_eq!(s.acquire(Some("s")).await.err(), Some(AcquireError::AllDisabled));
    }

    #[tokio::test]
    async fn too_many_failures_then_self_heal() {
        let s = sched(vec![acct("a", 4, None)]);
        for _ in 0..max_failures() {
            s.report_failure("a", UpstreamErrorKind::ServerError);
        }
        assert!(s.acquire(Some("s")).await.is_ok(), "全灭自愈后应恢复");
    }

    #[tokio::test]
    async fn success_resets_failure_count() {
        let s = sched(vec![acct("a", 4, None)]);
        for _ in 0..(max_failures() - 1) {
            s.report_failure("a", UpstreamErrorKind::ServerError);
        }
        s.report_success("a");
        for _ in 0..(max_failures() - 1) {
            s.report_failure("a", UpstreamErrorKind::ServerError);
        }
        assert!(s.acquire(Some("s")).await.is_ok(), "成功后失败计数应清零");
    }

    #[tokio::test]
    async fn no_session_key_still_picks() {
        let s = sched(vec![acct("a", 4, None)]);
        assert!(s.acquire(None).await.is_ok(), "无 session_key 应退化为 LRU 选号");
    }

    #[tokio::test]
    async fn bad_request_does_not_penalize() {
        let s = sched(vec![acct("a", 4, None)]);
        for _ in 0..(max_failures() * 2) {
            s.report_failure("a", UpstreamErrorKind::BadRequest);
        }
        assert!(s.acquire(Some("s")).await.is_ok(), "BadRequest 不应惩罚账号");
    }

    /// 2026-07-25 opus-5 事故的回归测试:上游模型级过载**绝不能**记进账号连续失败。
    ///
    /// 当时把它当 ServerError,35 秒内禁光 7 个健康号并触发全灭自愈;而禁用对**所有模型**
    /// 生效,连带 opus-4-6/sonnet-5 一起挂。单账号场景下"全灭自愈"会掩盖禁用,故这里用
    /// **两个**账号:若 Overloaded 仍计失败,a 会被真禁用而 b 健在,自愈不触发,断言即失败。
    #[tokio::test]
    async fn overloaded_does_not_penalize_account() {
        let s = sched(vec![acct("a", 4, None), acct("b", 4, None)]);
        for _ in 0..(max_failures() * 3) {
            s.report_failure("a", UpstreamErrorKind::Overloaded);
        }
        let snap = s.status_snapshot();
        let a = snap.iter().find(|x| x.account_id == "a").expect("账号 a 应在快照里");
        assert_eq!(a.failure_count, 0, "上游没容量不是账号的错,failure_count 必须为 0");
        assert!(!a.disabled, "Overloaded 绝不能禁用账号(禁用对所有模型生效)");
    }

    /// 锁住 `spares_account_health()` 与 `report_failure` 的"不惩罚"分支一致:
    /// 前者是策略声明,后者是实现,两处分开写就可能漂移。
    #[tokio::test]
    async fn spares_account_health_matches_no_penalty_arms() {
        use UpstreamErrorKind as K;
        const ALL: &[K] = &[
            K::TokenInvalid, K::RateLimited, K::TemporarilyBlocked, K::QuotaExhausted,
            K::Network, K::ServerError, K::Overloaded, K::BadRequest,
            K::ModelNotAvailable, K::EmptyResponse, K::Other,
        ];
        for &kind in ALL {
            // 两个号:避免单号被禁时"全灭自愈"重置计数,掩盖真实惩罚行为。
            let s = sched(vec![acct("a", 4, None), acct("b", 4, None)]);
            s.report_failure("a", kind);
            let snap = s.status_snapshot();
            let a = snap.iter().find(|x| x.account_id == "a").unwrap();
            // 只有走 failure_count 分支的 kind 才会 +1;冷却/禁用类走别的字段。
            let bumped = a.failure_count > 0;
            assert!(
                !(kind.spares_account_health() && bumped),
                "{kind:?} 声明不惩罚账号,却把 failure_count 加到了 {}",
                a.failure_count
            );
        }
    }

    #[tokio::test]
    async fn update_account_preserves_runtime_state() {
        let s = sched(vec![acct("a", 4, None)]);
        s.report_failure("a", UpstreamErrorKind::ServerError);
        let mut extra = BTreeMap::new();
        extra.insert("access_token".to_string(), serde_json::json!("new"));
        let updated = Arc::new(Account {
            account_id: "a".into(),
            provider: "kiro".into(),
            max_concurrency: 4,
            disabled: false,
            extra,
        });
        s.update_account(updated);
        let lease = s.acquire(Some("s")).await.unwrap();
        assert_eq!(lease.account.extra_str("access_token"), Some("new"));
    }

    // ───────── sync_accounts(admin 增删改免重启) ─────────

    fn acct_disabled(id: &str, disabled: bool) -> Arc<Account> {
        let mut a = (*acct(id, 2, None)).clone();
        a.disabled = disabled;
        Arc::new(a)
    }

    #[tokio::test]
    async fn sync_adds_and_removes_accounts() {
        let s = sched(vec![acct("a", 2, None), acct("b", 2, None)]);
        // 钉一个会话到 a(平局按 id,首选 a)。
        let first = s.acquire(Some("sess-pin")).await.unwrap().account_id().to_string();
        assert_eq!(first, "a");

        let out = s.sync_accounts(vec![acct("b", 2, None), acct("c", 2, None)]);
        assert_eq!((out.added, out.removed), (1, 1), "新增 c,移除 a");
        assert_eq!(s.total(), 2);
        // a 已移除:原钉 a 的会话必须改钉存活账号,不得报错。
        let next = s.acquire(Some("sess-pin")).await.unwrap().account_id().to_string();
        assert_ne!(next, "a");
        // c 可被租用。
        assert!(s.account("c").is_some());
        assert!(s.account("a").is_none());
    }

    #[tokio::test]
    async fn sync_config_disable_then_enable() {
        let s = sched(vec![acct("a", 2, None)]);
        // 配置翻转 → 禁用。
        s.sync_accounts(vec![acct_disabled("a", true)]);
        assert_eq!(s.acquire(None).await.err(), Some(AcquireError::AllDisabled));
        // 同配置重复 sync:保持禁用(不抖动)。
        s.sync_accounts(vec![acct_disabled("a", true)]);
        assert_eq!(s.acquire(None).await.err(), Some(AcquireError::AllDisabled));
        // 翻回 false → 复活。
        s.sync_accounts(vec![acct_disabled("a", false)]);
        assert!(s.acquire(None).await.is_ok());
    }

    #[tokio::test]
    async fn sync_unchanged_config_keeps_runtime_disable() {
        let s = sched(vec![acct("a", 2, None)]);
        s.report_failure("a", UpstreamErrorKind::QuotaExhausted);
        assert_eq!(s.acquire(None).await.err(), Some(AcquireError::AllDisabled));
        // 配置一直是 false → false:30s 周期 sync 不得把运行时封禁洗掉。
        s.sync_accounts(vec![acct("a", 2, None)]);
        assert_eq!(
            s.acquire(None).await.err(),
            Some(AcquireError::AllDisabled),
            "配置未翻转,运行时禁用必须保留"
        );
        // admin 显式禁用→启用一轮 = 人工复活。
        s.sync_accounts(vec![acct_disabled("a", true)]);
        s.sync_accounts(vec![acct_disabled("a", false)]);
        assert!(s.acquire(None).await.is_ok(), "显式开关翻转应清运行时禁用");
    }

    #[tokio::test]
    async fn sync_replaces_account_config_keeps_runtime() {
        let s = sched(vec![acct("a", 2, None)]);
        s.report_failure("a", UpstreamErrorKind::ServerError);
        // 换 extra(凭据轮换)+ 提并发。
        let mut newer = (*acct("a", 4, Some(7))).clone();
        newer.extra.insert("refresh_token".into(), serde_json::json!("rt-new"));
        s.sync_accounts(vec![Arc::new(newer)]);
        let got = s.account("a").unwrap();
        assert_eq!(got.extra_str("refresh_token"), Some("rt-new"));
        assert_eq!(got.max_concurrency, 4);
        // 运行态保留:failure_count 不归零(snapshot 验证)。
        let snap = s.status_snapshot();
        let a = snap.iter().find(|x| x.account_id == "a").unwrap();
        assert_eq!(a.failure_count, 1, "sync 不得清运行时失败计数");
        assert_eq!(a.priority, 7, "priority 跟随新配置");
        assert_eq!(a.max_concurrency, 4);
    }

    #[tokio::test]
    async fn sync_skips_extra_overwrite_when_dirty() {
        let s = sched(vec![acct("a", 1, None)]);
        // 刷新成功但回写 DB 失败:内存进新 token + 置脏。
        let mut refreshed = (*acct("a", 1, None)).clone();
        refreshed.extra.insert("refresh_token".into(), serde_json::json!("rt-new"));
        s.update_account(Arc::new(refreshed));
        s.mark_extra_dirty("a");

        // DB 里还是旧 token:sync 不得把内存洗回去。
        let mut stale = (*acct("a", 1, None)).clone();
        stale.extra.insert("refresh_token".into(), serde_json::json!("rt-stale"));
        s.sync_accounts(vec![Arc::new(stale.clone())]);
        assert_eq!(
            s.account("a").unwrap().extra_str("refresh_token"),
            Some("rt-new"),
            "脏标记期间 sync 不得用 DB 旧值覆盖内存新 token"
        );
        assert_eq!(s.dirty_accounts().len(), 1);

        // 持久化成功 → 清脏,之后 sync 恢复正常覆盖。
        s.clear_extra_dirty("a");
        s.sync_accounts(vec![Arc::new(stale)]);
        assert_eq!(s.account("a").unwrap().extra_str("refresh_token"), Some("rt-stale"));
    }

    // ───────── 模型能力过滤(acquire_where) ─────────

    /// 标了 subscription_title 的账号(模型过滤测试用)。
    fn acct_sub(id: &str, concurrency: u32, title: &str) -> Arc<Account> {
        let mut a = (*acct(id, concurrency, None)).clone();
        a.extra
            .insert("subscription_title".into(), serde_json::json!(title));
        Arc::new(a)
    }

    /// 模拟 KiroProvider::account_supports_model 的 opus 过滤谓词。
    fn opus_pred(a: &Account) -> bool {
        a.extra_str("subscription_title")
            .map(|t| !t.to_uppercase().contains("FREE"))
            .unwrap_or(true)
    }

    #[tokio::test]
    async fn acquire_where_skips_unsupported_accounts() {
        // FREE 号 id 字典序更靠前(无过滤时会先选中),验证谓词真正生效。
        let s = sched(vec![acct_sub("a-free", 4, "KIRO FREE"), acct_sub("b-pro", 4, "KIRO PRO")]);
        for sess in ["s1", "s2", "s3"] {
            let lease = s.acquire_where(Some(sess), opus_pred).await.unwrap();
            assert_eq!(lease.account_id(), "b-pro", "FREE 号绝不能被 opus 请求选中");
        }
    }

    #[tokio::test]
    async fn acquire_where_no_capable_account_errors_distinctly() {
        let s = sched(vec![acct_sub("a-free", 4, "KIRO FREE")]);
        assert_eq!(
            s.acquire_where(Some("s"), opus_pred).await.err(),
            Some(AcquireError::NoModelSupport),
            "全 FREE 请求 opus 应报 NoModelSupport,而非 AllDisabled"
        );
        // 非过滤请求(如 sonnet)照常可用。
        assert!(s.acquire(Some("s")).await.is_ok());
    }

    #[tokio::test]
    async fn affinity_primary_unsupported_promotes_capable() {
        let s = sched(vec![acct_sub("a-free", 4, "KIRO FREE"), acct_sub("b-pro", 4, "KIRO PRO")]);
        // 无过滤的请求把会话钉到 FREE 号(字典序先选 a-free)。
        let first = s.acquire(Some("mix")).await.unwrap().account_id().to_string();
        assert_eq!(first, "a-free");
        // 同会话来了 opus 请求:primary 不支持 → 改选 PRO 并当场转正。
        let second = s
            .acquire_where(Some("mix"), opus_pred)
            .await
            .unwrap()
            .account_id()
            .to_string();
        assert_eq!(second, "b-pro");
        // 转正后,后续非 opus 请求也钉在新 primary(落在哪认哪)。
        let third = s.acquire(Some("mix")).await.unwrap().account_id().to_string();
        assert_eq!(third, "b-pro");
    }

    #[tokio::test]
    async fn busy_supported_account_waits_instead_of_falling_to_unsupported() {
        // PRO 号并发 1 且被占满;FREE 号空闲。opus 请求必须等 PRO 释放,绝不落到 FREE。
        let s = sched(vec![acct_sub("a-free", 4, "KIRO FREE"), acct_sub("b-pro", 1, "KIRO PRO")]);
        let hold = s.acquire_where(Some("s1"), opus_pred).await.unwrap();
        assert_eq!(hold.account_id(), "b-pro");
        // 占满时 acquire_where 应进入 busy-wait(而非立刻落到 FREE),释放后拿到 PRO。
        let acquire_fut = s.acquire_where(Some("s2"), opus_pred);
        tokio::pin!(acquire_fut);
        // 先让它跑一小会(进入 busy-wait 循环),确认未错误落到 FREE。
        tokio::select! {
            r = &mut acquire_fut => {
                panic!("PRO 占满时不应立刻成功(更不能落 FREE):{:?}", r.map(|l| l.account_id().to_string()));
            }
            _ = tokio::time::sleep(Duration::from_millis(30)) => {}
        }
        drop(hold); // 释放 PRO 并发
        let got = acquire_fut.await.unwrap();
        assert_eq!(got.account_id(), "b-pro", "释放后应拿到 PRO,而非 FREE");
    }

    #[tokio::test]
    async fn self_heal_respects_model_filter() {
        // a-free 连续失败禁用,b-pro 额度耗尽:opus 请求不得顺手复活无关的 FREE 失败号。
        let s = sched(vec![acct_sub("a-free", 2, "KIRO FREE"), acct_sub("b-pro", 2, "KIRO PRO")]);
        for _ in 0..max_failures() {
            s.report_failure("a-free", UpstreamErrorKind::ServerError);
        }
        s.report_failure("b-pro", UpstreamErrorKind::QuotaExhausted);
        assert_eq!(
            s.acquire_where(Some("s"), opus_pred).await.err(),
            Some(AcquireError::AllDisabled)
        );
        let snap = s.status_snapshot();
        let a = snap.iter().find(|x| x.account_id == "a-free").unwrap();
        assert!(a.disabled, "opus 请求不应复活不支持 opus 的失败号");
        // 无过滤(如 sonnet)请求才触发对它的全灭自愈。
        assert!(s.acquire(Some("s2")).await.is_ok());
    }

    #[tokio::test]
    async fn single_account_low_max_failures_heals_then_acquires() {
        // max_failures=1 的极端配置:一次失败即禁用;acquire 的尝试预算与其解耦,
        // 自愈后必须还有机会重选(否则恒报 AllBusy)。
        let cfg = SchedulerConfig { max_failures: 1, ..SchedulerConfig::default() };
        let s = AccountScheduler::new(vec![acct("a", 2, None)], &cfg);
        s.report_failure("a", UpstreamErrorKind::ServerError);
        let got = s.acquire(None).await;
        assert!(got.is_ok(), "全灭自愈后应能租到刚恢复的账号: {:?}", got.err());
    }

    #[tokio::test]
    async fn update_tuning_changes_empty_threshold_live() {
        // EmptyResponse 是冷却类(不被全灭自愈复活),可在单账号上直接观测。
        // 默认阈值 3:一次 empty 不冷却。热更阈值到 1 后:首个 empty 即冷却。
        let s = AccountScheduler::new(vec![acct("a", 2, None)], &SchedulerConfig::default());
        s.report_failure("a", UpstreamErrorKind::EmptyResponse);
        assert!(s.acquire(None).await.is_ok(), "默认阈值 3:单个 empty 不应冷却");
        s.update_tuning(&SchedulerConfig {
            empty_response_threshold: 1,
            ..SchedulerConfig::default()
        });
        s.report_failure("a", UpstreamErrorKind::EmptyResponse);
        assert_eq!(
            s.acquire(None).await.err(),
            Some(AcquireError::AllDisabled),
            "热更阈值到 1 后,再来一个 empty 即应冷却禁用"
        );
    }

    // ───────── reset_account / merge_extra ─────────

    #[tokio::test]
    async fn reset_account_revives_runtime_disable() {
        let s = sched(vec![acct("a", 2, None)]);
        s.report_failure("a", UpstreamErrorKind::QuotaExhausted);
        assert_eq!(s.acquire(None).await.err(), Some(AcquireError::AllDisabled));
        assert!(s.reset_account("a"));
        assert!(s.acquire(None).await.is_ok(), "reset 后应立即回到选号池");
        assert!(!s.reset_account("ghost"), "不存在的账号返回 false");
    }

    #[tokio::test]
    async fn reset_account_keeps_config_disable() {
        let s = sched(vec![acct_disabled("a", true)]);
        assert!(s.reset_account("a"));
        assert_eq!(
            s.acquire(None).await.err(),
            Some(AcquireError::AllDisabled),
            "配置层禁用不被 reset 解除(显式运营意图,走 PATCH disabled=false)"
        );
    }

    #[tokio::test]
    async fn merge_extra_updates_single_field_in_place() {
        let s = sched(vec![acct("a", 2, None)]);
        assert!(s.merge_extra("a", "subscription_title", serde_json::json!("KIRO PRO")));
        assert_eq!(
            s.account("a").unwrap().extra_str("subscription_title"),
            Some("KIRO PRO")
        );
        // 等值合并返回 false(调用方据此跳过持久化)。
        assert!(!s.merge_extra("a", "subscription_title", serde_json::json!("KIRO PRO")));
        // 不影响其它字段与运行态。
        s.report_failure("a", UpstreamErrorKind::ServerError);
        assert!(s.merge_extra("a", "subscription_title", serde_json::json!("KIRO POWER")));
        let snap = s.status_snapshot();
        assert_eq!(snap[0].failure_count, 1, "merge_extra 不得动运行态");
        assert!(!s.merge_extra("ghost", "k", serde_json::json!(1)));
    }

    // ───────── 配置注入(Tuning) ─────────

    #[tokio::test]
    async fn custom_empty_threshold_from_config() {
        let cfg = SchedulerConfig {
            empty_response_threshold: 1,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![acct("a", 2, None)], &cfg);
        s.report_failure("a", UpstreamErrorKind::EmptyResponse);
        assert_eq!(
            s.acquire(None).await.err(),
            Some(AcquireError::AllDisabled),
            "阈值 1:首个 empty 即应冷却"
        );
    }

    // ───────── status_snapshot ─────────

    #[tokio::test]
    async fn status_snapshot_reflects_runtime_states() {
        let s = sched(vec![acct("a", 2, None), acct("b", 1, None)]);
        s.report_failure("a", UpstreamErrorKind::RateLimited);

        let snap = s.status_snapshot();
        assert_eq!(snap.len(), 2);
        let a = snap.iter().find(|x| x.account_id == "a").unwrap();
        assert!(a.disabled);
        assert_eq!(a.reason, "rate_limited");
        assert!(a.cooldown_remaining_secs > 0 && a.cooldown_remaining_secs <= 300);
        let b = snap.iter().find(|x| x.account_id == "b").unwrap();
        assert!(!b.disabled);
        assert_eq!(b.reason, "");
        assert_eq!(b.available_permits, 1);
        assert_eq!(b.max_concurrency, 1);

        // 在途租约要反映到 available_permits。
        let _lease = s.acquire(None).await.unwrap(); // a 禁用 → 租到 b
        let snap = s.status_snapshot();
        let b = snap.iter().find(|x| x.account_id == "b").unwrap();
        assert_eq!(b.available_permits, 0, "持有租约期间并发槽占用");
    }

    // ───────── 影子组档位守卫(低价档 GLOW) ─────────

    /// 只允许高优层的守卫(GLOW 的典型配置:POWER 主力 priority=0,小号 100)。
    fn hi_tier() -> TierGuard {
        TierGuard { min_priority: None, max_priority: Some(0) }
    }

    /// **只允许低优层**的守卫:把主力号挡在档位外(priority >= 1)。
    /// 这是"保证高价用户稳定"的那一档 —— 低价流量烧不到 priority=0 的主力号。
    fn lo_tier() -> TierGuard {
        TierGuard { min_priority: Some(1), max_priority: None }
    }

    /// 下界守卫必须把**更优先**的号排除在外。
    /// 反向断言证明主力号本来就是首选 —— 否则"没选中主力号"可能只是构造得巧。
    #[tokio::test]
    async fn tier_min_priority_never_selects_mainstay() {
        // 主力号 id 字典序靠前且优先级更高:无守卫时必被首选,能证明过滤真实生效。
        let s = sched(vec![acct("a-main", 4, Some(0)), acct("b-backup", 4, Some(100))]);
        for i in 0..6 {
            let lease =
                s.acquire_tiered(Some(&format!("s{i}")), |_| true, Some(&lo_tier())).await.unwrap();
            assert_eq!(lease.account_id(), "b-backup", "低价档绝不能落到 priority=0 主力号");
        }
        // 反向:无守卫的正常请求确实首选主力号。
        let lease = s.acquire(Some("normal")).await.unwrap();
        assert_eq!(lease.account_id(), "a-main", "正常组本来就该优先用主力号");
    }

    /// 只有主力号时,下界档位报 `TierExhausted`(503 可重试)而**不是**偷偷用主力号。
    /// 这条一旦回归,低价流量会直接打到高价客户的号上 —— 正是本特性要防的事。
    #[tokio::test]
    async fn tier_min_exhausted_rather_than_falling_back_to_mainstay() {
        let s = sched(vec![acct("a-main", 4, Some(0))]);
        assert_eq!(
            s.acquire_tiered(Some("s"), |_| true, Some(&lo_tier())).await.err(),
            Some(AcquireError::TierExhausted),
            "档位内无号必须报 TierExhausted,绝不许下探到主力号"
        );
        assert!(s.acquire(Some("s2")).await.is_ok(), "同一时刻正常组仍可用(号是好的)");
    }

    /// 两端同时给出 = 闭区间。中间层被选中,两侧都被排除。
    #[tokio::test]
    async fn tier_bounds_form_closed_interval() {
        let s = sched(vec![
            acct("a-top", 4, Some(0)),
            acct("b-mid", 4, Some(50)),
            acct("c-low", 4, Some(100)),
        ]);
        let g = TierGuard { min_priority: Some(10), max_priority: Some(60) };
        for i in 0..6 {
            let lease = s.acquire_tiered(Some(&format!("s{i}")), |_| true, Some(&g)).await.unwrap();
            assert_eq!(lease.account_id(), "b-mid", "只有落在 [10,60] 内的号可被选中");
        }
        // 边界是闭的:等于端点的号必须准入。
        let s2 = sched(vec![acct("edge", 4, Some(10))]);
        let g2 = TierGuard { min_priority: Some(10), max_priority: Some(10) };
        assert!(
            s2.acquire_tiered(Some("s"), |_| true, Some(&g2)).await.is_ok(),
            "priority 恰等于上下界时必须准入(闭区间)"
        );
    }

    /// 下界守卫同样不得触发全灭自愈:低价请求不能把正常组刚合法禁用的主力号复活。
    #[tokio::test]
    async fn tier_min_guard_does_not_heal_disabled_mainstay() {
        let s = sched(vec![acct("a-main", 4, Some(0))]);
        s.report_failure("a-main", UpstreamErrorKind::RateLimited);
        assert_eq!(
            s.acquire_tiered(Some("s"), |_| true, Some(&lo_tier())).await.err(),
            Some(AcquireError::TierExhausted)
        );
        // 自愈若被触发,冷却会被清掉、这里就能选出号了。
        assert!(
            s.acquire(Some("n")).await.is_err(),
            "低价档的空集绝不能顺手把主力号的冷却/禁用清零"
        );
    }

    /// **"不影响现有分组"的机械证明**:同一批号、同一串会话,带 `None` 守卫与走老
    /// `acquire_where` 必须选出**逐个相同**的账号序列。这条挂了就说明改动泄漏到了普通组。
    #[tokio::test]
    async fn guard_none_matches_acquire_where_exactly() {
        let mk = || sched(vec![acct("a", 2, Some(0)), acct("b", 2, Some(0)), acct("c", 2, Some(100))]);
        let (old, new) = (mk(), mk());
        let (mut seq_old, mut seq_new) = (Vec::new(), Vec::new());
        for i in 0..12 {
            let sess = format!("s{}", i % 5);
            seq_old.push(old.acquire_where(Some(&sess), |_| true).await.unwrap().account_id().to_string());
            seq_new.push(new.acquire_tiered(Some(&sess), |_| true, None).await.unwrap().account_id().to_string());
        }
        assert_eq!(seq_old, seq_new, "guard=None 必须与老路径逐个选号完全一致");
    }

    /// 守卫生效:低优兜底层对本档不可见。反向断言证明兜底层**本来是活的**——
    /// 否则"没选中 c"可能只是测试构造得巧,而非过滤真的起作用。
    #[tokio::test]
    async fn tier_max_priority_never_selects_lower_tier() {
        // 兜底号 id 字典序最靠前:无过滤时平局会先选它,能证明过滤真实生效。
        let s = sched(vec![acct("a-backup", 4, Some(100)), acct("b-main", 4, Some(0))]);
        for i in 0..6 {
            let lease = s.acquire_tiered(Some(&format!("s{i}")), |_| true, Some(&hi_tier())).await.unwrap();
            assert_eq!(lease.account_id(), "b-main", "低价档绝不能落到 priority=100 兜底层");
        }
        // 反向:主力号全禁用后,**无守卫**的请求会正常下探到兜底层(兜底层确实可用)。
        s.report_failure("b-main", UpstreamErrorKind::RateLimited);
        let lease = s.acquire(Some("normal")).await.unwrap();
        assert_eq!(lease.account_id(), "a-backup", "正常组该下探兜底层时必须能下探");
    }

    /// 主力层全冷却时,低价档报 `TierExhausted`(503 可重试),**不下探**。
    /// 同一状态下无守卫的请求仍应成功落到兜底层 —— 证明隔离是单向的。
    #[tokio::test]
    async fn tier_exhausted_when_high_tier_all_disabled() {
        let s = sched(vec![acct("a-backup", 4, Some(100)), acct("b-main", 4, Some(0))]);
        s.report_failure("b-main", UpstreamErrorKind::RateLimited);
        assert_eq!(
            s.acquire_tiered(Some("s"), |_| true, Some(&hi_tier())).await.err(),
            Some(AcquireError::TierExhausted),
            "高优层全冷却 → 低价档报 TierExhausted,而不是悄悄下探烧兜底号"
        );
        assert!(s.acquire(Some("s2")).await.is_ok(), "同一时刻正常组仍可用");
    }

    /// **头号陷阱**:档位过滤后的空集绝不能报成 `NoModelSupport`(那会被映射成 400,
    /// 客户端认为"换模型才能解决"而停止重试);而真正的模型能力不足仍须报 NoModelSupport。
    #[tokio::test]
    async fn tier_exhausted_is_never_no_model_support() {
        let s = sched(vec![acct("a-backup", 4, Some(100))]); // 档位内一个号都没有
        let e = s.acquire_tiered(Some("s"), |_| true, Some(&hi_tier())).await.err();
        assert_eq!(e, Some(AcquireError::TierExhausted));
        assert_ne!(e, Some(AcquireError::NoModelSupport), "档位空集不是订阅能力问题");

        // 反向:真的没号支持该模型时,语义不能被守卫改写。
        let s2 = sched(vec![acct_sub("a-free", 4, "KIRO FREE")]);
        assert_eq!(
            s2.acquire_tiered(Some("s"), opus_pred, None).await.err(),
            Some(AcquireError::NoModelSupport),
            "全 FREE 请求 opus 仍应是 NoModelSupport"
        );
    }

    /// 低价流量不得污染**兜底层**的 LRU 游标:GLOW 从不选中兜底号,所以在它跑过之后,
    /// 兜底层对普通请求必须仍是"全部未用过"的初始状态、按 id 依次轮转。
    ///
    /// 若守卫实现哪天退化成"先选中再拒绝",兜底号的 `last_selected_at` 会被写脏,
    /// 下面的轮转顺序就会乱 —— 这条测试正是为捕捉那种退化而写。
    #[tokio::test]
    async fn guard_blocked_account_keeps_lru_cursor() {
        let s = sched(vec![
            acct("c1-backup", 4, Some(100)),
            acct("c2-backup", 4, Some(100)),
            acct("m-main", 4, Some(0)),
        ]);
        for i in 0..10 {
            let l = s
                .acquire_tiered(Some(&format!("glow{i}")), |_| true, Some(&hi_tier()))
                .await
                .unwrap();
            assert_eq!(l.account_id(), "m-main", "低价档只能落主力号");
        }
        // 主力号退场,普通流量下探兜底层:两个兜底号都该是"从未被选中",
        // 于是平局按 id → 先 c1 后 c2。GLOW 若写脏了游标,这个顺序会变。
        s.report_failure("m-main", UpstreamErrorKind::RateLimited);
        let first = s.acquire(None).await.unwrap().account_id().to_string();
        let second = s.acquire(None).await.unwrap().account_id().to_string();
        assert_eq!(
            (first.as_str(), second.as_str()),
            ("c1-backup", "c2-backup"),
            "兜底层的 LRU 游标必须未被低价流量污染"
        );
    }

    /// **带守卫的请求绝不触发全灭自愈**:否则每个被挡下的低价请求都会把正常组刚合法
    /// 禁用的号复活并清零失败计数,连续失败保护对所有档位一起失效。
    #[tokio::test]
    async fn heal_ignores_shadow_tier() {
        let s = sched(vec![acct("a-main", 2, Some(0))]);
        for _ in 0..max_failures() {
            s.report_failure("a-main", UpstreamErrorKind::ServerError);
        }
        // 低价档请求:应直接 TierExhausted,且**不得**顺手把 a-main 复活。
        assert_eq!(
            s.acquire_tiered(Some("glow"), |_| true, Some(&hi_tier())).await.err(),
            Some(AcquireError::TierExhausted)
        );
        let snap = s.status_snapshot();
        let a = snap.iter().find(|x| x.account_id == "a-main").unwrap();
        assert!(a.disabled, "低价档请求不得触发全灭自愈复活账号");
        // 反向:普通组请求仍能自愈(保护只对影子层收紧,不改变现有行为)。
        assert!(s.acquire(Some("normal")).await.is_ok(), "正常组的全灭自愈必须照常工作");
    }

    /// 会话亲和在档位内正常工作(缓存热度不因为加了守卫就丢)。
    #[tokio::test]
    async fn tier_guard_preserves_affinity() {
        let s = sched(vec![acct("a", 4, Some(0)), acct("b", 4, Some(0)), acct("c", 4, Some(100))]);
        let first = s.acquire_tiered(Some("glow-1"), |_| true, Some(&hi_tier())).await.unwrap()
            .account_id().to_string();
        for _ in 0..6 {
            let l = s.acquire_tiered(Some("glow-1"), |_| true, Some(&hi_tier())).await.unwrap();
            assert_eq!(l.account_id(), first, "同会话必须钉同一个号,否则上游缓存全冷");
        }
    }

    /// 守卫对普通流量零副作用:两档交替跑,普通请求照常选中被守卫拒绝的号。
    #[tokio::test]
    async fn guard_has_no_side_effect_on_unguarded_traffic() {
        let s = sched(vec![acct("a-backup", 4, Some(100)), acct("b-main", 4, Some(0))]);
        for i in 0..5 {
            s.acquire_tiered(Some(&format!("glow{i}")), |_| true, Some(&hi_tier())).await.unwrap();
            let n = s.acquire(Some(&format!("norm{i}"))).await.unwrap();
            assert!(
                ["a-backup", "b-main"].contains(&n.account_id()),
                "普通请求可用全部账号"
            );
        }
        // 普通请求确实能选到被守卫拒绝的那个号(共享池、单份 entry)。
        s.report_failure("b-main", UpstreamErrorKind::RateLimited);
        assert_eq!(s.acquire(Some("norm-x")).await.unwrap().account_id(), "a-backup");
    }

    /// `TierExhausted` 的文案必须能与 `NoModelSupport` 区分(运维看日志要能分辨)。
    #[test]
    fn acquire_error_display_distinguishes_tier() {
        let t = AcquireError::TierExhausted.to_string();
        assert!(!t.is_empty());
        assert_ne!(t, AcquireError::NoModelSupport.to_string());
    }
}
