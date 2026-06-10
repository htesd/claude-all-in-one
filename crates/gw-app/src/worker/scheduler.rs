//! worker 组内账号调度 —— 🟢 移植旧 kiro.rs `MultiTokenManager` 的 v52 会话亲和。
//!
//! ## 为什么在 worker 层
//!
//! kiro.rs 是单进程,一个 `MultiTokenManager` 既做选号又做亲和。kiro-gw 拆成多进程:
//! - **router** 做 session→worker 亲和(同会话钉同 worker,已实现);
//! - **worker**(本模块)做 session→**组内账号**亲和:同会话钉同一上游账号。
//!
//! 这一层才是 Kiro prefix cache 命中的命门——**Kiro 缓存按上游账号隔离**,同会话每换
//! 一个账号≈冷启动。v52 实测:同 conversationId 19 秒内横跳 4 个账号 → 命中率塌到个位数。
//!
//! ## v52 亲和铁律:「落在哪个号就认哪个号」
//!
//! - 新会话:在合格账号里按**优先级分层 LRU** 选 primary(最高优先级层内 last_selected 最旧);
//! - 老会话且 primary 当下可用:一直用 primary(缓存热);
//! - primary 当下不可用(busy/冷却/禁用):**立即**改选 LRU 候选并**当场转正为新 primary**,
//!   此后钉死新号、**永不主动迁回**原号(消除旧版「空↔满」抖动导致的橡皮筋横跳)。
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
use gw_core::error::UpstreamErrorKind;
use parking_lot::Mutex;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// 会话亲和映射 TTL:超过此时长未访问的会话条目惰性淘汰(等于会话重开,自然再平衡)。
const AFFINITY_TTL: Duration = Duration::from_secs(30 * 60);
/// 429 限流冷却时长(到期自愈)。
const RATE_LIMIT_COOLDOWN: Duration = Duration::from_secs(60);
/// 空响应冷却时长(与 429 解耦,默认更短;v58 阈值冷却用)。
const EMPTY_RESPONSE_COOLDOWN: Duration = Duration::from_secs(20);
/// 空响应固定窗口:窗口内累计 empty 达阈值才冷却(避免误伤偶发 empty 的健康号)。
const EMPTY_WINDOW: Duration = Duration::from_secs(120);
const EMPTY_THRESHOLD: u32 = 3;
/// 连续 API 失败达此次数 → 自动禁用(TooManyFailures,可被全灭自愈)。
const MAX_FAILURES: u32 = 5;

/// 账号被禁用/冷却的原因(决定自愈策略)。🟢 对齐 kiro.rs DisabledReason。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DisabledReason {
    /// 连续 API 失败达阈值(全灭时可自愈重置)。
    TooManyFailures,
    /// 额度耗尽(QuotaExhausted,持久,不自动恢复)。
    QuotaExhausted,
    /// 429 限流,冷却到期自愈。
    RateLimited,
    /// 空响应达阈值,冷却到期自愈。
    EmptyResponse,
    /// refresh_token 永久失效(invalid_grant),持久。
    InvalidRefreshToken,
}

impl DisabledReason {
    /// 是否为可冷却自愈类(到期自动恢复)。其余为持久禁用(需人工/全灭自愈)。
    fn is_cooldown(self) -> bool {
        matches!(self, DisabledReason::RateLimited | DisabledReason::EmptyResponse)
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
    /// 连续 API 失败次数(成功清零;达 MAX_FAILURES 禁用)。
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

/// 会话亲和记录:session_key → 当前 primary 账号(带 TTL 淘汰)。
/// v52:只留 primary + last_access,删 alt/streak(「落在哪个号就认哪个号」)。
struct AffinityEntry {
    /// 当前主账号 id(迁移后即更新,不回弹)。
    primary: String,
    /// 最后访问时刻(TTL 淘汰用)。
    last_access: Instant,
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
}

impl std::fmt::Display for AcquireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AcquireError::AllDisabled => write!(f, "组内所有账号均已禁用"),
            AcquireError::AllBusy => write!(f, "组内所有账号并发已满"),
            AcquireError::Empty => write!(f, "组内无账号"),
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
}

impl AccountScheduler {
    /// 用一组账号构造调度器。
    pub fn new(accounts: Vec<Arc<Account>>) -> Self {
        let mut entries = HashMap::with_capacity(accounts.len());
        for acc in accounts {
            entries.insert(acc.account_id.clone(), CredentialState::new(acc));
        }
        Self {
            entries: Mutex::new(entries),
            affinity: Mutex::new(HashMap::new()),
        }
    }

    /// 组内账号总数。
    pub fn total(&self) -> usize {
        self.entries.lock().len()
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

    /// 合格账号 id 集:未禁用 + 不在 exclude(busy)内。
    fn eligible_ids(
        entries: &HashMap<String, CredentialState>,
        exclude: &HashSet<String>,
    ) -> Vec<String> {
        entries
            .values()
            .filter(|e| !e.disabled && !exclude.contains(&e.account.account_id))
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

    /// 按会话亲和选一个账号 id(v52「落在哪个号就认哪个号」),并更新亲和表 + last_selected。
    /// `session_key = None` 时退化为分层 LRU(无亲和记忆)。`exclude` = 本轮已 busy 的号。
    fn select_id(
        &self,
        session_key: Option<&str>,
        exclude: &HashSet<String>,
        now: Instant,
    ) -> Option<String> {
        let mut entries = self.entries.lock();
        Self::heal_cooldowns(&mut entries, now);
        let eligible = Self::eligible_ids(&entries, exclude);
        if eligible.is_empty() {
            return None;
        }

        let chosen = match session_key {
            None => Self::tiered_lru(&entries, &eligible),
            Some(key) => {
                let mut map = self.affinity.lock();
                map.retain(|_, v| now.duration_since(v.last_access) < AFFINITY_TTL);
                let eligible_set: HashSet<&String> = eligible.iter().collect();
                match map.get_mut(key) {
                    None => {
                        let id = Self::tiered_lru(&entries, &eligible);
                        map.insert(
                            key.to_string(),
                            AffinityEntry { primary: id.clone(), last_access: now },
                        );
                        id
                    }
                    Some(ent) => {
                        ent.last_access = now;
                        if eligible_set.contains(&ent.primary) {
                            // primary 当下可用 → 继续用(缓存热)。
                            ent.primary.clone()
                        } else {
                            // primary 不可用 → 立即改选并当场转正,永不迁回。
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

    /// 选号 + 取并发许可(v52 亲和)。返回租约;持有期间占并发槽,Drop 释放。
    ///
    /// 流程(对齐 kiro.rs acquire_context_with_session_and_group):
    /// 1. 冷却 sweep + 分层 LRU 亲和选号;
    /// 2. 取并发许可,满了把该号记 busy、清亲和重试标记,换下一个号;
    /// 3. 全 busy 但有可用号 → 短 sleep 等并发释放后重试;
    /// 4. 全禁用且有 TooManyFailures → 全灭自愈(重置失败计数)再试一轮;否则报错。
    pub async fn acquire(&self, session_key: Option<&str>) -> Result<AccountLease, AcquireError> {
        let total = self.total();
        if total == 0 {
            return Err(AcquireError::Empty);
        }
        let max_attempts = (total * MAX_FAILURES as usize).max(1);
        let mut attempts = 0;
        let mut busy: HashSet<String> = HashSet::new();
        let mut self_healed = false;

        loop {
            if attempts >= max_attempts {
                return Err(AcquireError::AllBusy);
            }
            let now = Instant::now();

            let Some(id) = self.select_id(session_key, &busy, now) else {
                // 无合格号:区分"全 busy(有可用但占满)" vs "全禁用"。
                let (avail_total, avail_not_busy) = {
                    let entries = self.entries.lock();
                    let total_avail = entries.values().filter(|e| !e.disabled).count();
                    let not_busy = entries
                        .values()
                        .filter(|e| !e.disabled && !busy.contains(&e.account.account_id))
                        .count();
                    (total_avail, not_busy)
                };
                if avail_total > 0 && avail_not_busy == 0 && !busy.is_empty() {
                    // 有可用号但全 busy → 等并发释放后重试。
                    busy.clear();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    attempts += 1;
                    continue;
                }
                // 全禁用:若有 TooManyFailures,做一次全灭自愈(等价重启)再试。
                if !self_healed && self.heal_too_many_failures() {
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
    /// 返回是否实际恢复了至少一个号。
    fn heal_too_many_failures(&self) -> bool {
        let mut entries = self.entries.lock();
        let mut healed = false;
        for e in entries.values_mut() {
            if e.disabled && e.disabled_reason == Some(DisabledReason::TooManyFailures) {
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
        let mut entries = self.entries.lock();
        let Some(e) = entries.get_mut(id) else { return };
        // 已禁用(含手动/额度)不覆盖原因,幂等。
        if e.disabled {
            return;
        }
        match kind {
            UpstreamErrorKind::RateLimited | UpstreamErrorKind::TemporarilyBlocked => {
                e.disabled = true;
                e.disabled_reason = Some(DisabledReason::RateLimited);
                e.disabled_until = Some(now + RATE_LIMIT_COOLDOWN);
                tracing::warn!(account = %id, "命中限流,冷却 {}s", RATE_LIMIT_COOLDOWN.as_secs());
            }
            UpstreamErrorKind::EmptyResponse => {
                // v58 固定窗口阈值:窗口内累计达阈值才冷却,避免误伤偶发 empty 的健康号。
                match e.empty_window_start {
                    Some(start) if now.duration_since(start) <= EMPTY_WINDOW => {
                        e.empty_count_in_window += 1;
                    }
                    _ => {
                        e.empty_window_start = Some(now);
                        e.empty_count_in_window = 1;
                    }
                }
                if e.empty_count_in_window >= EMPTY_THRESHOLD {
                    e.disabled = true;
                    e.disabled_reason = Some(DisabledReason::EmptyResponse);
                    e.disabled_until = Some(now + EMPTY_RESPONSE_COOLDOWN);
                    e.empty_window_start = None;
                    e.empty_count_in_window = 0;
                    tracing::warn!(account = %id, "空响应达阈值,冷却 {}s", EMPTY_RESPONSE_COOLDOWN.as_secs());
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
                if e.failure_count >= MAX_FAILURES {
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
        AccountScheduler::new(accounts)
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
        for _ in 0..MAX_FAILURES {
            s.report_failure("a", UpstreamErrorKind::ServerError);
        }
        assert!(s.acquire(Some("s")).await.is_ok(), "全灭自愈后应恢复");
    }

    #[tokio::test]
    async fn success_resets_failure_count() {
        let s = sched(vec![acct("a", 4, None)]);
        for _ in 0..(MAX_FAILURES - 1) {
            s.report_failure("a", UpstreamErrorKind::ServerError);
        }
        s.report_success("a");
        for _ in 0..(MAX_FAILURES - 1) {
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
        for _ in 0..(MAX_FAILURES * 2) {
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
        assert!(a.cooldown_remaining_secs > 0 && a.cooldown_remaining_secs <= 60);
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

