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
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::AllDisabled => write!(f, "组内所有账号均已禁用"),
            AcquireError::AllBusy => write!(f, "组内所有账号并发已满"),
            AcquireError::Empty => write!(f, "组内无账号"),
            AcquireError::NoModelSupport => {
                write!(f, "组内无支持该模型的账号(订阅等级不足,如 FREE 不支持 opus)")
            }
        }
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

    /// 合格账号 id 集:未禁用 + 不在 exclude(busy)内 + 支持本次模型。
    fn eligible_ids(
        entries: &HashMap<String, CredentialState>,
        exclude: &HashSet<String>,
        supports: &dyn Fn(&Account) -> bool,
    ) -> Vec<String> {
        entries
            .values()
            .filter(|e| {
                !e.disabled
                    && !exclude.contains(&e.account.account_id)
                    && supports(&e.account)
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
    fn select_id(
        &self,
        session_key: Option<&str>,
        exclude: &HashSet<String>,
        now: Instant,
        supports: &dyn Fn(&Account) -> bool,
    ) -> Option<String> {
        let mut entries = self.entries.lock();
        Self::heal_cooldowns(&mut entries, now);
        let eligible = Self::eligible_ids(&entries, exclude, supports);
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

            let Some(id) = self.select_id(session_key, &busy, now, &supports) else {
                // 无合格号:区分"没号支持该模型" vs "全 busy(有可用但占满)" vs "全禁用"。
                // 三类计数都只看**支持该模型**的号——不支持的号既救不了 busy 等待,
                // 也不该让错误从 NoModelSupport 误报成 AllDisabled。
                let (supported_any, avail_total, avail_not_busy) = {
                    let entries = self.entries.lock();
                    let mut any = false;
                    let mut avail = 0usize;
                    let mut not_busy = 0usize;
                    for e in entries.values() {
                        if !supports(&e.account) {
                            continue;
                        }
                        any = true;
                        if e.disabled {
                            continue;
                        }
                        avail += 1;
                        if !busy.contains(&e.account.account_id) {
                            not_busy += 1;
                        }
                    }
                    (any, avail, not_busy)
                };
                if !supported_any {
                    return Err(AcquireError::NoModelSupport);
                }
                if avail_total > 0 && avail_not_busy == 0 && !busy.is_empty() {
                    // 有可用号但全 busy → 等并发释放后重试。
                    busy.clear();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    attempts += 1;
                    continue;
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
            UpstreamErrorKind::BadRequest => {}
        }
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
}

