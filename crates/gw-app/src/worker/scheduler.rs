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
    /// 「全禁用」时排队等冷却到期的最长时长。`0` = 关闭(与本开关引入前逐字节等价)。
    /// 详见 [`gw_core::config::SchedulerConfig::queue_wait_ms`]。
    queue_wait: Duration,
    /// 限流节流间隔(仅对开了排队的号生效;`0` = 关,走二值冷却)。
    rate_limit_pace: Duration,
    /// 节流熔断阈值;`0` = **不熔断**(默认),开了排队的号永远只节流不下线。
    rate_limit_pace_max_strikes: u32,
    /// 降层前为「仅因 429 节流而暂不可选的更高优先层」等待的上限;`0` = 关(立即降层)。
    /// 详见 [`gw_core::config::SchedulerConfig::tier_hold_ms`]。
    tier_hold: Duration,
    /// 请求级窗口:请求开始后多久内其取号还允许触发上面的等待;`0` = 关。
    tier_hold_window: Duration,
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
            queue_wait: Duration::from_millis(c.queue_wait_ms),
            rate_limit_pace: Duration::from_millis(c.rate_limit_pace_ms),
            // 0 = 不熔断(默认):开了排队的号永远只节流,唯一能让它下线的是真被 suspend。
            rate_limit_pace_max_strikes: c.rate_limit_pace_max_strikes,
            tier_hold: Duration::from_millis(c.tier_hold_ms),
            tier_hold_window: Duration::from_millis(c.tier_hold_window_ms),
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

/// 排队位守卫:持有期间计入在途等待数,**Drop 即释放**。
/// 必须是 RAII —— 客户端中途断开会让整个 acquire future 被 drop,
/// 手写 decrement 在那条路径上不会执行,计数只涨不落,几分钟后队列就永久“满”了。
struct QueueSlot<'a>(&'a std::sync::atomic::AtomicUsize);

impl Drop for QueueSlot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// 该账号是否开启「排队等冷却」(`extra.queue_enabled == true`)。
///
/// **逐账号**而非全局:企业号的上游并发是跨租户共享的,429 是跟别人抢、等一下就有;
/// 而社交号的 429 往往伴随额度见底,等待只是把客户多挂几秒后照样报错。所以开关必须
/// 落到账号粒度,由运维按凭据类型决定,别一刀切。
fn queue_enabled(a: &Account) -> bool {
    a.extra
        .get("queue_enabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
}

/// 该账号的**每分钟调用上限**(`extra.rpm_limit`);`None` = 不限(默认,行为与本开关
/// 引入前逐字节相同)。`<= 0` 视为不限 —— 0 若当成"一次都不许"会让运维一个笔误
/// 静默停掉一个号,而"不限"是这个字段缺席时的既有语义。
///
/// **为什么必须有这个闸,而 `max_concurrency` 顶不上**:并发限的是"同时几个",
/// 不是"一分钟几个"。一个 `max_concurrency=2` 的号,每请求 ~14s,一分钟能轮转 8 轮 ——
/// 2026-08-09 线上实测单个付费号一分钟被打 16 次,全程并发从未越过 2。
///
/// **为什么不能等上游信号**:实测近 3 天 social(付费)号收到的 `RateLimited` = **0 次**、
/// `EmptyResponse` = **0 次**,却有 17 次 403 直接封禁(7 个号全灭)。Kiro 对这类号
/// 不发限流预警,第一个信号就是封号 —— 所有挂在"上游先给软信号"前提上的冷却参数
/// (`rate_limit_cooldown_secs`/`empty_response_*`)在它们身上**从不执行**。
/// 唯一能救的办法是我方主动定频。
///
/// 放 `extra` 而**不是** `SystemSettings`:后者在 2026-07-31 之前的镜像里带
/// `deny_unknown_fields`,加字段会让回滚变成全量 503(见 `gw_core::config` 的硬警告)。
/// 且 RPM 本就该按凭据类型分别设:付费 social 号保守,企业号可以不设。
fn rpm_limit(a: &Account) -> Option<u32> {
    a.extra
        .get("rpm_limit")
        .and_then(|v| v.as_i64())
        .filter(|n| *n > 0)
        .map(|n| n.min(u32::MAX as i64) as u32)
}

/// 单账号运行态(并发槽 + 禁用/冷却 + LRU + 失败计数)。🟢 对齐 kiro.rs CredentialEntry。
/// 429 节流用的两个字段见 `paced_until` / `rate_limit_strikes`。
struct CredentialState {
    /// 账号配置(含 extra 凭证字段)。刷新后由调度器整体替换为带新 token 的副本。
    account: Arc<Account>,
    /// 单号并发上限信号量(容量 = account.max_concurrency)。
    semaphore: Arc<Semaphore>,
    /// **无视图时**的兜底排序(数值越小越优先);来自 account.extra.priority,缺省 100。
    ///
    /// ⚠️ 组内优先级的事实源是**成员边**(`account_groups.priority`),经 [`GroupView`]
    /// 按请求传入 —— 同一个号在 A 组可以是主力、在 B 组是兜底,这是本字段表达不了的。
    /// 生产路径每个请求都带分组,恒有视图;本字段只服务于未分组 key 与单元测试。
    default_rank: i64,
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
    /// **429 节流闸**(仅开了排队的号会被设置):在此时刻前不被选中,但**账号不下线**
    /// —— 与 `disabled` 的本质区别是它不进 `/health` 的禁用统计、不影响面板"正常"状态,
    /// 也不会让整组被判 `AllDisabled`。到点自动可选,即"保持一个频率访问"。
    paced_until: Option<Instant>,
    /// 连续 429 次数(成功一次即清零)。撞到熔断阈值就放弃节流、退回二值冷却,
    /// 防止上游真把这个号限死时我们无限定频硬撞(22 分钟送走 5 个号就是这么来的)。
    rate_limit_strikes: u32,
    empty_window_start: Option<Instant>,
    /// 当前窗口内 empty 次数。
    empty_count_in_window: u32,
    /// RPM 滑动窗口:最近若干次**选中**时刻(升序)。见 [`rpm_limit`]。
    ///
    /// 用滑动窗口而非固定窗口:固定窗口允许"窗口末尾打满 + 下窗口开头再打满"的
    /// 双倍突发(RPM=6 能在 2 秒内过 12 个),而封号看的正是这种瞬时节奏。
    /// 长度天然被上限截断(超过 limit 条就 pop 掉最老的),所以不会无界增长。
    ///
    /// 记的是**选中**(select)而不是完成:一个请求跑 20 秒,按完成计会让 20 秒内
    /// 选中的号全部漏过闸门。
    rpm_hits: std::collections::VecDeque<Instant>,
    /// upstream_cut 滑窗(软冷却信号,见 [`AccountScheduler::report_upstream_cut`])。
    /// 有界:只保留窗口内的戳,且条数封顶 [`CUTS_CAP`]。懒清理,无后台任务。
    cuts: std::collections::VecDeque<Instant>,
    /// 软冷却截止时刻:期间该号**不接新会话**(已有亲和仍粘着;normal 全空时
    /// fail-open 仍可用,见 select_id 的 normal/draining 拆分)。
    drain_until: Option<Instant>,
    /// 软冷却代际:reset / 配置重启用 / 移除后重新加入时换发新号并清空
    /// cuts/drain_until。lease 选号时快照;迟到的旧代际上报一律丢弃,
    /// 防止老流的 Drop 把前兆记到「救回来」的新运行态上(codex 对抗评审#7)。
    epoch: u64,
}

impl CredentialState {
    fn new(account: Arc<Account>) -> Self {
        let concurrency = account.max_concurrency.max(1) as usize;
        let default_rank = account
            .extra
            .get("priority")
            .and_then(|v| v.as_i64())
            .unwrap_or(100);
        Self {
            semaphore: Arc::new(Semaphore::new(concurrency)),
            default_rank,
            disabled: account.disabled,
            config_disabled: account.disabled,
            extra_dirty: false,
            disabled_reason: None,
            disabled_until: None,
            failure_count: 0,
            last_selected_at: None,
            paced_until: None,
            rate_limit_strikes: 0,
            empty_window_start: None,
            empty_count_in_window: 0,
            rpm_hits: std::collections::VecDeque::new(),
            cuts: std::collections::VecDeque::new(),
            drain_until: None,
            epoch: next_cut_epoch(),
            account,
        }
    }

    /// RPM 窗口内已用次数;顺带丢弃过期条目(懒清理,不需要后台任务)。
    /// 未设上限的号恒返回 0,不产生任何簿记开销。
    fn rpm_used(&mut self, now: Instant) -> u32 {
        if rpm_limit(&self.account).is_none() {
            return 0;
        }
        while let Some(front) = self.rpm_hits.front() {
            if now.duration_since(*front) >= RPM_WINDOW {
                self.rpm_hits.pop_front();
            } else {
                break;
            }
        }
        self.rpm_hits.len() as u32
    }

    /// 该号此刻是否已打满 RPM 配额。
    fn rpm_exhausted(&mut self, now: Instant) -> bool {
        match rpm_limit(&self.account) {
            None => false,
            Some(limit) => self.rpm_used(now) >= limit,
        }
    }

    /// 配额恢复一格的剩余时间(最老那条滑出窗口的时刻);没打满则 `None`。
    fn rpm_wait(&mut self, now: Instant) -> Option<Duration> {
        if !self.rpm_exhausted(now) {
            return None;
        }
        let front = *self.rpm_hits.front()?;
        Some(RPM_WINDOW.saturating_sub(now.duration_since(front)))
    }

    /// 记一次调用。只对设了上限的号记账,避免给不限速的号(企业号)白加内存。
    fn rpm_note_selected(&mut self, now: Instant) {
        if rpm_limit(&self.account).is_none() {
            return;
        }
        self.rpm_hits.push_back(now);
    }

    /// 退还最近一次预留(选号成功但 `try_lease` 没拿到 permit → 没发出上游调用)。
    /// 退最新那条而不是最老的:最老的是别人的真实调用,退错会让配额虚高。
    fn rpm_refund_latest(&mut self) {
        if rpm_limit(&self.account).is_none() {
            return;
        }
        self.rpm_hits.pop_back();
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
    /// 该号是否开了「排队等冷却」(`extra.queue_enabled`)。面板逐号展示 + 可开关。
    pub queue_enabled: bool,
    /// 该号的每分钟上限(`extra.rpm_limit`);`None` = 不限。
    pub rpm_limit: Option<u32>,
    /// 滑动窗口内已用次数。运维据此判断「号是不是被自己的 RPM 闸卡住」——
    /// 否则面板上它显示"正常"却不吃流量,查不出原因。
    pub rpm_used: u32,
}

/// 全组的排队实况(面板展示用)。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct QueueStats {
    /// 此刻正在排队等冷却的请求数。
    pub waiting: usize,
    /// 队列容量 = 已开排队**且当前可服务**的号的并发之和。跑干/禁用的不计。
    /// 这就是准入阈值:`waiting` 触到它,新请求立刻 503 而不是排进来陪跑。
    pub capacity: usize,
    /// 开了排队开关的号数(不论其当前是否可用)。
    pub enabled_accounts: usize,
    /// **累计**进过排队的请求数(worker 启动以来)。
    ///
    /// 为什么要它:`waiting` 是瞬时值,而排队只在「全组都不可用」时触发,面板上几乎恒为 0
    /// —— 准确但看不出机制到底有没有在工作。累计值才能回答"这个开关有没有救到人"。
    pub queued_total: u64,
    /// **累计**被节流吸收的 429 次数(worker 启动以来)。
    ///
    /// 节流那行日志是 `debug!`,线上 `RUST_LOG=info` 看不到 —— 这个计数是它唯一的可观测面。
    pub paced_total: u64,
    /// **累计**「降层前为高优先层等待」的次数(worker 启动以来)。
    ///
    /// 口径是**每次内部睡眠 +1**,不是每个请求 +1:一次 429 可以让几十个并发请求各自
    /// 等一轮,同一个请求也可能在预算内等多轮。所以它只能回答「这个开关有没有在工作、
    /// 强度如何」,**不能**和 `paced_total` 相减去推算"漏了多少量" —— 两者口径不同,
    /// 差值正负都没有意义。真要量化漏出,看 `request_logs` 里各优先层的请求占比。
    pub tier_held_total: u64,
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

/// 「降层前先等」的单次睡眠下限,同时也是**是否还值得等**的门槛:剩余预算不足它就
/// 直接降层。有下限是为了保证循环推进(不空转);把它同时当门槛,是为了让实际等待
/// 永远不越过配置的 `tier_hold_ms` —— 否则「先裁到剩余预算、再取 max(下限)」会在
/// 预算尾部系统性超发。
const MIN_TIER_HOLD: Duration = Duration::from_millis(10);

/// RPM 滑动窗口长度。见 [`rpm_limit`] 与 [`CredentialState::rpm_hits`]。
const RPM_WINDOW: Duration = Duration::from_secs(60);

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

/// upstream_cut 滑窗条数硬上限(个位数):窗口剪枝之外的第二道有界护栏。
const CUTS_CAP: usize = 8;

/// 软冷却代际发号器(进程内单调递增)。新 entry、reset、配置重启用都取新号:
/// 「移除再加入」会得到全新 entry,若代际从 0 重启,旧 lease 的快照就可能撞上
/// 新 entry 的代际,把老流迟到的 upstream_cut 记到新运行态(codex 对抗评审#7)。
static CUT_EPOCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

fn next_cut_epoch() -> u64 {
    CUT_EPOCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// draining 软冷却参数(env OnceLock,对齐 KIRO_MIN_THINKING_BUDGET 模式)。
///
/// **不进 SystemSettings**:旧镜像对设置项 deny_unknown_fields,加字段会让回滚
/// 旧镜像时全量 503(仓库既有硬警告)。非法/0/负值 → warn + 默认值(fail-open,
/// 绝不能把坏配置解释成"立即冷却",codex 对抗评审#6 边界)。
#[derive(Clone, Copy)]
struct DrainTuning {
    /// upstream_cut 滑窗长度(默认 600s)。
    window: Duration,
    /// 窗口内达此次数进 draining(默认 2)。
    threshold: u32,
    /// 单次 draining 时长(默认 1500s;使用时 clamp 到 ≤ 亲和 TTL)。
    secs: Duration,
}

fn drain_tuning() -> DrainTuning {
    static V: std::sync::OnceLock<DrainTuning> = std::sync::OnceLock::new();
    *V.get_or_init(|| {
        fn env_secs(name: &str, default: u64) -> u64 {
            match std::env::var(name).ok().map(|s| s.trim().parse::<u64>()) {
                Some(Ok(v)) if v > 0 => v,
                Some(_) => {
                    tracing::warn!("{name} 非法/非正,回退默认值 {default}");
                    default
                }
                None => default,
            }
        }
        DrainTuning {
            window: Duration::from_secs(env_secs("KIRO_DRAIN_WINDOW_SECS", 600)),
            threshold: env_secs("KIRO_DRAIN_THRESHOLD", 2) as u32,
            secs: Duration::from_secs(env_secs("KIRO_DRAIN_SECS", 1500)),
        }
    })
}

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
    /// 选号时刻的软冷却代际快照。upstream_cut 上报随身携带,代际不符即丢弃
    /// (见 [`CredentialState::epoch`] 与 [`AccountScheduler::report_upstream_cut`])。
    pub cut_epoch: u64,
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
    /// 本组没有任何能服务该模型的**成员**(组里一条成员边都没配,或配的号都不支持该模型)。
    ///
    /// 必须与 [`AcquireError::NoModelSupport`] 严格区分:后者被映射成 **400**
    /// (客户侧可解:换模型/升级订阅),而本变体是**配置态**、运维加条成员边即恢复,
    /// 必须是 **503**。若把成员过滤混进 `supports` 谓词就会退化成 NoModelSupport→400,
    /// 客户端(SDK/NewAPI 对 400 不重试)会当成自己请求非法而放弃。
    GroupEmpty,
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
            AcquireError::GroupEmpty => {
                write!(f, "本分组未配置任何可服务该模型的账号,请检查分组成员")
            }
        }
    }
}

impl AcquireError {
    /// **对外**中性文案。上面的 `Display` 是给运维看的(说的是账号池/分组/订阅档,
    /// 等于把渠道形态告诉客户),只进日志;客户端看这一份。
    ///
    /// 与 [`gw_core::error::UpstreamErrorKind::client_message`] 同口径:只区分
    /// 「客户能做什么」——换模型(400) vs 等一等(503)。
    pub fn client_message(&self) -> &'static str {
        match self {
            // 400:客户侧可解 —— 换个模型就行。
            AcquireError::NoModelSupport => "当前模型不可用,请更换模型后重试",
            // 503:我方容量/配置态,客户只能等。
            AcquireError::AllDisabled
            | AcquireError::AllBusy
            | AcquireError::Empty
            | AcquireError::GroupEmpty => "服务暂时不可用,请稍后重试",
        }
    }
}

/// 一次请求所属分组的**成员视图**:哪些号可见 + 它们在这个组里排第几。
///
/// 来自成员边表 `account_groups`(worker 每轮同步快照)。**不在视图里的号,对本次请求
/// 根本不存在** —— 不是"被过滤掉的可用号",而是压根不属于这个组。
///
/// 为什么排序要挂在视图上而不是账号上:同一个号可以在正常组当主力(rank 0)、在低价组
/// 当兜底(rank 100)。旧模型把 priority 挂在账号上,所有组被迫共用一套排序,于是低价组
/// 要么抢主力号、要么完全够不着主力号(压满了只能硬报错),没有中间态。
///
/// 设计约束(改动前先读):
/// 1. **必须独立于 `supports` 谓词**。`supports` 表达"账号能否服务该模型"(近乎静态、
///    客户侧可解),视图表达"这个号属不属于本组"(配置态)。合并二者会让视图过滤后的
///    空集退化成 `NoModelSupport` → 400,语义完全错误。
/// 2. **rank 必须是稳定量**。成员边只在 admin 改配置时变,所以会话亲和不会因它反复重钉。
///    **不要**把 `available_permits()` 这类抖动量塞进来:primary 一失格就会走"改选并当场
///    转正、永不迁回"(见 `select_id`),抖动量会让会话反复换号、缓存冷启动,反而放大额度消耗。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupView {
    /// account_id → 组内优先级(数值越小越优先)。键集即成员集。
    rank: std::collections::HashMap<String, i64>,
}

impl GroupView {
    pub fn new(rank: std::collections::HashMap<String, i64>) -> Self {
        Self { rank }
    }
}

/// 本次请求下该账号的排序值;`None` = **不属于本组**,对本请求不存在。
///
/// 无视图(未分组 key / 单元测试)时回落到账号自带的 `default_rank`,行为与本次重构
/// 之前逐字节相同 —— 这是「既有 49 条调度测试零改动通过」的依据。
fn rank_of(view: Option<&GroupView>, e: &CredentialState) -> Option<i64> {
    match view {
        None => Some(e.default_rank),
        Some(v) => v.rank.get(&e.account.account_id).copied(),
    }
}

/// 同上,但按 id 从 entries 里取;取不到 entry 视为不可见。
fn rank_by_id(
    view: Option<&GroupView>,
    entries: &HashMap<String, CredentialState>,
    id: &str,
) -> Option<i64> {
    entries.get(id).and_then(|e| rank_of(view, e))
}

/// worker 组内账号调度器:会话亲和选号 + 并发控制 + 冷却/禁用生命周期。
///
/// 线程安全(内部 Mutex),被 `Arc` 多请求共享。**一个 worker 一个实例**,只管本组账号。
pub struct AccountScheduler {
    /// account_id → 运行态。HashMap 以 id 索引(选号在锁内遍历,组规模小,O(n) 可接受)。
    entries: Mutex<HashMap<String, CredentialState>>,
    /// 当前正在「排队等冷却」的请求数。上限按**可排队账号的并发之和**动态算(见
    /// [`Self::queue_probe`]):不设界的话,一波流量会全部堆在同一个号上等,
    /// 最后集体等到超时 —— 比直接快速失败更糟(客户等更久、结果一样)。
    /// 用 `Relaxed`:它只是个准入近似阈值,不参与任何同步关系;短暂过冲无害。
    waiting: std::sync::atomic::AtomicUsize,
    /// 累计进过排队的请求数 / 累计被节流吸收的 429 数(仅观测,不参与任何判定)。
    queued_total: std::sync::atomic::AtomicU64,
    paced_total: std::sync::atomic::AtomicU64,
    /// 累计「降层前为高优先层等待」的睡眠次数(仅观测,口径见 [`QueueStats`] 同名字段)。
    /// 它还兼任**确定性抖动源**:同一个 paced 号上挂着的多个等待者若同时醒来会一起打
    /// 上游、再撞一片 429,取它的低位散一点开(仓库无 rand 依赖,不为此引一个)。
    tier_held_total: std::sync::atomic::AtomicU64,
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
            waiting: std::sync::atomic::AtomicUsize::new(0),
            queued_total: std::sync::atomic::AtomicU64::new(0),
            paced_total: std::sync::atomic::AtomicU64::new(0),
            tier_held_total: std::sync::atomic::AtomicU64::new(0),
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

    /// 请求级等待窗口:请求开始后多久内,其取号还允许触发「降层前先等」。
    /// `0` = 关(每轮都直接降层)。供 worker 的换号重试循环拿它与 `retry_started.elapsed()`
    /// 比,得出本轮的 `allow_tier_hold`。
    pub fn tier_hold_window(&self) -> Duration {
        self.tuning.read().tier_hold_window
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

    /// 合格账号 id 集:未禁用 + 不在 exclude(busy)内 + 支持本次模型 + **是本组成员**
    /// + **本分钟 RPM 配额未打满**。`view = None`(未分组请求)时与重构之前逐字节等价。
    ///
    /// 取 `&mut` 是因为 RPM 窗口做懒清理(见 [`CredentialState::rpm_used`]):不设上限的号
    /// 直接短路返回,不产生任何簿记开销,所以对既有部署零影响。
    fn eligible_ids(
        entries: &mut HashMap<String, CredentialState>,
        exclude: &HashSet<String>,
        now: Instant,
        supports: &dyn Fn(&Account) -> bool,
        view: Option<&GroupView>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for e in entries.values_mut() {
            let basic = !e.disabled
                // 429 节流窗口内不选它,但它**没下线** —— 到点即恢复,不需要 sweep。
                && e.paced_until.map(|t| now >= t).unwrap_or(true)
                && !exclude.contains(&e.account.account_id)
                && supports(&e.account)
                && rank_of(view, e).is_some();
            if !basic {
                continue;
            }
            // RPM 打满的号本轮不可选。**必须排在上面的判定之后**:先排除掉压根选不中的号,
            // 免得为它们做窗口清理。与 `paced_until` 一样是"没下线、到点自己回来"的软状态,
            // 不进 `/health` 禁用统计、不让整组被判 AllDisabled。
            if e.rpm_exhausted(now) {
                continue;
            }
            out.push(e.account.account_id.clone());
        }
        out
    }

    /// 「降层前先等」的判定:若**更高优先层**存在仅因 429 节流而暂不可选的号,返回其中
    /// 最早到期的剩余时间;否则 `None`(照常降层)。不改任何状态,一次持锁扫完。
    ///
    /// 判据与 [`Self::eligible_ids`] 的谓词逐条对齐,**只放开 pace 那一条** —— 否则会为一个
    /// 根本选不中的号(不支持本模型 / 非本组成员 / 已禁用 / permit 被占满)干等,把省钱
    /// 变成纯延迟。`exclude`(busy)也照剔:高层号并发满是容量问题,等不出来。
    ///
    /// 一个号都选不出时返回 `None`,**不拦截**:那条路由 [`Self::acquire_in_group`] 里既有的
    /// paced / queue 分支负责,重复拦截只会让两套预算叠加。
    ///
    /// ⚠️ 本方法跑在 `heal_cooldowns` **之前**(后者在 `select_id` 里)。于是「冷却刚到期、
    /// 还没被 sweep 掉」的号在这里仍读作 `disabled`,不计入基线。后果是极小概率多等一个
    /// 节流窗口(≤ 单轮预算),不会漏等也不会误降层 —— 换取的是不额外拿一次写锁。
    /// 锁只在本方法内持有,**不跨 `await`**:调用方拿到时长后才睡。
    fn tier_hold_wait(
        &self,
        exclude: &HashSet<String>,
        now: Instant,
        supports: &dyn Fn(&Account) -> bool,
        view: Option<&GroupView>,
    ) -> Option<Duration> {
        let mut entries = self.entries.lock();
        // 此刻就能选中的最高优先层 = 「不等的话会落到哪一层」的基线。
        let mut best_selectable: Option<i64> = None;
        // 仅因节流不可选的最高优先层 + 该层最早到期时间 = 「等一下能拿到什么」。
        let mut best_paced: Option<(i64, Duration)> = None;
        for e in entries.values_mut() {
            if e.disabled || exclude.contains(&e.account.account_id) || !supports(&e.account) {
                continue;
            }
            let Some(rank) = rank_of(view, e) else { continue };
            let paced_left = e
                .paced_until
                .map(|t| t.saturating_duration_since(now))
                .filter(|d| *d > Duration::ZERO);
            match paced_left {
                // 等一个**没有空闲 permit** 的号是白等:窗口过去了照样租不到,只是把延迟
                // 加给客户。与 `best_available_higher` 用 `available_permits()` 的理由相同
                // (那里也是"高层饱和就别迁")。注意只筛候选、不筛下面的基线 ——
                // 基线要与 `select_id` 的口径逐字节一致,否则会误判"不等就会掉到哪层"。
                Some(_) if e.semaphore.available_permits() == 0 => continue,
                Some(d) => {
                    // 节流窗口过去后 RPM 仍打满 → 照样选不中,等它只是徒增延迟
                    // (对抗审查 [中]:候选侧漏了这一条,而基线侧已经检查了)。
                    if e.rpm_wait(now).is_some_and(|r| r > d) {
                        continue;
                    }
                    let better = match best_paced {
                        None => true,
                        Some((r, m)) => rank < r || (rank == r && d < m),
                    };
                    if better {
                        best_paced = Some((rank, d));
                    }
                }
                None => {
                    // RPM 打满的号此刻**选不中**,不能算进基线 —— 否则会误判"不等也不降层"。
                    // 这是基线与 `select_id`/`eligible_ids` 口径一致的一部分。
                    if e.rpm_exhausted(now) {
                        continue;
                    }
                    if best_selectable.map(|r| rank < r).unwrap_or(true) {
                        best_selectable = Some(rank);
                    }
                }
            }
        }
        let (paced_rank, wait) = best_paced?;
        // 同层还有号能立刻服务时不等 —— 等的意义只在于「不降层」。
        (paced_rank < best_selectable?).then_some(wait)
    }

    /// 「降层前先等**排队号**」的判定:更高优先层若有 `queue_enabled` 的号正在冷却、
    /// 且能在 `budget` 内自愈,返回其中最早到期的剩余时间;否则 `None`(照常降层)。
    ///
    /// 为什么需要它、而 [`Self::tier_hold_wait`] 不够:后者只认 `paced_until`(429 节流
    /// 窗口)。但排队号撞 429 走**二值冷却**的情形很常见 —— 关了 `rate_limit_pace_ms`、
    /// 或撞上 `rate_limit_pace_max_strikes` 熔断退回冷却时都是。生产 G0 高优层 29 个号
    /// 全部 `queue_enabled`、冷却 2s,于是每次 429 都把一批量漏给只有 31 个 permit 的
    /// 低优兜底池,把本已半死的兜底号打到封禁(2026-08-09 用户报障)。
    ///
    /// 口径(用户原话):"只要我有排队模式的号,低优先就不应该吃流量;如果没有,那高优先
    /// 的正常掉到低优先"。所以:
    /// - 只保护 `queue_enabled` 的号 —— 普通高优号冷却仍立即降层(既有语义,见
    ///   `tier_hold_ignores_cooled_down_top_tier`)。
    /// - 只等**预算内会回来**的冷却。1h 封禁(`suspended_cooldown`)远超预算 → 降层,
    ///   这正是"如果没有"的那一半。
    /// - 同层已有号能立刻服务时不等:等的意义只在于不降层。
    /// - 谓词与 [`Self::eligible_ids`] 逐条对齐,**只放开 disabled 那一条**,否则会为一个
    ///   根本选不中的号(不支持本模型/非本组/permit 满)干等,把省钱变成客户干等。
    ///
    /// 锁只在本方法内持有,**不跨 `await`**:调用方拿到时长后才睡。
    fn queue_tier_hold_wait(
        &self,
        exclude: &HashSet<String>,
        now: Instant,
        supports: &dyn Fn(&Account) -> bool,
        view: Option<&GroupView>,
        budget: Duration,
    ) -> Option<(Duration, usize)> {
        let mut entries = self.entries.lock();
        // 此刻就能选中的最高优先层 = 「不等的话会落到哪一层」的基线。口径必须与
        // `select_id`/`eligible_ids` 一致,否则会误判"不等就会掉到哪层"。
        let mut best_selectable: Option<i64> = None;
        // 仅因冷却不可选的排队号里,最高优先层 + 该层最早到期时间。
        let mut best_cooling: Option<(i64, Duration)> = None;
        // 队列准入容量。口径与 `queue_probe` 一致:只算**开了排队且预算内会回来**的号 ——
        // 撑大 cap 会让等待者远超真实吞吐、全部排到超时。
        let mut cap: usize = 0;
        for e in entries.values_mut() {
            if exclude.contains(&e.account.account_id) || !supports(&e.account) {
                continue;
            }
            let Some(rank) = rank_of(view, e) else { continue };
            if e.disabled {
                // 只有开了排队、且是**冷却类**(会自愈)的禁用才构成等待理由。
                // 额度跑干 / config 禁用不会自己回来,等它等于干等到超时。
                if !queue_enabled(&e.account)
                    || !e.disabled_reason.map(|r| r.is_cooldown()).unwrap_or(false)
                {
                    continue;
                }
                let Some(until) = e.disabled_until else { continue };
                let d = until.saturating_duration_since(now);
                // 已到期(sweep 还没跑到)不算冷却;超预算的(如 1h 封禁)直接放弃。
                if d.is_zero() || d > budget {
                    continue;
                }
                // 等一个 permit 已满的号是白等:冷却过去了照样租不到。理由同
                // `tier_hold_wait` / `best_available_higher` 用 `available_permits()`。
                if e.semaphore.available_permits() == 0 {
                    continue;
                }
                // 冷却到点时 RPM 仍打满 → 照样选不中,等它是白等(对抗审查 [中])。
                // 比较的是**冷却结束那一刻**的配额:rpm_wait 是从现在起的剩余,
                // 若它比冷却更久,说明冷却结束时配额还没滑出窗口。
                if e.rpm_wait(now).is_some_and(|r| r > d) {
                    continue;
                }
                // 这个号预算内会回来 → 它的并发计入队列准入容量(口径同 queue_probe)。
                cap = cap.saturating_add(e.account.max_concurrency as usize);
                let better = match best_cooling {
                    None => true,
                    Some((r, m)) => rank < r || (rank == r && d < m),
                };
                if better {
                    best_cooling = Some((rank, d));
                }
            } else if e.paced_until.map(|t| now >= t).unwrap_or(true) && !e.rpm_exhausted(now) {
                // 与 eligible_ids 同口径:未禁用 + 不在节流窗口内 + RPM 未打满 = 此刻可选。
                // RPM 那一条必须带上,否则一个被 RPM 卡住的低层号会被当成"基线可选",
                // 让闸门误判"不等也不会降层"从而放行降层 —— 正好放跑要防的那个场景。
                if best_selectable.map(|r| rank < r).unwrap_or(true) {
                    best_selectable = Some(rank);
                }
            }
        }
        let (cooling_rank, wait) = best_cooling?;
        match best_selectable {
            // 此刻一个号都选不出:交给 `select_id` 的 else 分支(既有 queue/paced/rpm 等待
            // 逻辑),这里不拦 —— 重复拦截会让两套预算叠加。
            None => None,
            // 只有当「不等就会降到更低层」时才等。
            Some(sel) => (cooling_rank < sel).then_some((wait, cap)),
        }
    }

    /// 分层 LRU:在合格集合里取**最高优先级层**(组内 rank 最小),层内选 last_selected_at
    /// 最旧者(None 视为最久未用,平局按 id)。返回选中 id。调用方保证 ids 非空。
    fn tiered_lru(
        entries: &HashMap<String, CredentialState>,
        ids: &[String],
        view: Option<&GroupView>,
    ) -> String {
        let top_priority = ids
            .iter()
            .filter_map(|id| rank_by_id(view, entries, id))
            .min()
            .unwrap_or(i64::MAX);
        ids.iter()
            .filter(|id| rank_by_id(view, entries, id) == Some(top_priority))
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
        view: Option<&GroupView>,
    ) -> Option<String> {
        let cands: Vec<String> = ids
            .iter()
            .filter(|id| {
                rank_by_id(view, entries, id).is_some_and(|r| r < worse_than)
                    && entries
                        .get(*id)
                        .is_some_and(|e| e.semaphore.available_permits() > 0)
            })
            .cloned()
            .collect();
        if cands.is_empty() {
            None
        } else {
            Some(Self::tiered_lru(entries, &cands, view))
        }
    }

    /// 按会话亲和选一个账号 id(v52「落在哪个号就认哪个号」),并更新亲和表 + last_selected。
    /// `session_key = None` 时退化为分层 LRU(无亲和记忆)。`exclude` = 本轮已 busy 的号。
    /// `supports` = 模型能力过滤:primary 不支持本次模型时同样走「改选 + 当场转正」。
    /// `view` = 本次请求所属分组的成员视图(`None` = 未分组请求,行为不变)。
    fn select_id(
        &self,
        session_key: Option<&str>,
        exclude: &HashSet<String>,
        now: Instant,
        supports: &dyn Fn(&Account) -> bool,
        view: Option<&GroupView>,
    ) -> Option<String> {
        let mut entries = self.entries.lock();
        Self::heal_cooldowns(&mut entries, now);
        let eligible = Self::eligible_ids(&mut entries, exclude, now, supports, view);
        if eligible.is_empty() {
            return None;
        }

        // ── draining 软冷却拆分(codex 对抗评审#5)──
        // 既有谓词(RPM/排除/模型能力)已在 eligible_ids 全部跑完,
        // 这里只把合格集**分组**不过滤:draining 是偏好不是封禁。
        // 顺带对到期的 drain_until 做懒清理(软冷却无后台任务,只在选号/上报时观察)。
        let mut normal: Vec<String> = Vec::with_capacity(eligible.len());
        let mut draining: Vec<String> = Vec::new();
        for id in &eligible {
            match entries.get_mut(id).and_then(|e| {
                let d = e.drain_until?;
                if now < d {
                    Some(true)
                } else {
                    e.drain_until = None;
                    e.cuts.clear();
                    tracing::info!(account = %id, "软冷却到期,退出 draining");
                    Some(false)
                }
            }) {
                Some(true) => draining.push(id.clone()),
                _ => normal.push(id.clone()),
            }
        }
        // 无会话亲和的新请求优先 normal;normal 全空才 fail-open 用 draining
        // (两组都来自 eligible,不绕过任何既有闸门——宁可服务也不全池 503)。
        let pool = if normal.is_empty() {
            if !draining.is_empty() {
                tracing::debug!(
                    draining = draining.len(),
                    "合格集全部处于软冷却,fail-open 仍从 draining 选号"
                );
            }
            &eligible
        } else {
            &normal
        };

        let chosen = match session_key {
            None => Self::tiered_lru(&entries, pool, view),
            Some(key) => {
                let affinity_ttl = self.tuning.read().affinity_ttl;
                let mut map = self.affinity.lock();
                map.retain(|_, v| now.duration_since(v.last_access) < affinity_ttl);
                let eligible_set: HashSet<&String> = eligible.iter().collect();
                match map.get_mut(key) {
                    None => {
                        let id = Self::tiered_lru(&entries, pool, view);
                        map.insert(
                            key.to_string(),
                            AffinityEntry { primary: id.clone(), last_access: now, last_upgrade: None },
                        );
                        id
                    }
                    Some(ent) => {
                        ent.last_access = now;
                        if eligible_set.contains(&ent.primary) {
                            // primary 当下可用:**即使 primary 在 draining 也保持粘着**
                            // (冷却期内会话不挪窝,上游看到的是同会话重试,正常行为)。
                            // 同层保持 v52 粘着(缓存热);但若 primary 落在
                            // 低优先级层、而此刻更高层**有空闲 permit**,则向上迁移一次并转正
                            // ——让高优先级号被积极使用。高层饱和 / 已在最高层 → 维持粘着。
                            // 向上迁移目标从 **normal** 里挑(排除 draining,评审#5):
                            // 不能把会话迁到一个正在软冷却的号上。
                            // 去抖:距上次向上迁移不足 MIGRATE_UP_DEBOUNCE 则**不再迁**(cooled=false
                            // 时连 best_available_higher 扫描都跳过),把跨层横跳频率硬上限到
                            // 1 次/窗口/会话,防高层饱和线附近的橡皮筋抖动(见该常量注释)。
                            let primary_priority =
                                rank_by_id(view, &entries, &ent.primary).unwrap_or(i64::MAX);
                            let cooled = ent
                                .last_upgrade
                                .map_or(true, |t| now.duration_since(t) >= MIGRATE_UP_DEBOUNCE);
                            let target = if cooled {
                                Self::best_available_higher(
                                    &entries,
                                    &normal,
                                    primary_priority,
                                    view,
                                )
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
                            // 改选同样优先 normal,normal 全空才 fail-open 到 draining。
                            let id = Self::tiered_lru(&entries, pool, view);
                            ent.primary = id.clone();
                            id
                        }
                    }
                }
            }
        };

        // 选中即更新 last_selected_at(让 LRU 反映实时负载,新会话才轮转),
        // 并**在同一把锁内预留一个 RPM 名额**。
        //
        // 为什么必须在这里预留(对抗审查第二轮 [高]):`note_upstream_call` 只累加、
        // 不拒绝。若只在真正发调用时才记,账号仅剩 1 个名额时**多个并发请求**会在
        // 第一个记账之前全部通过 `eligible_ids` 的检查 → 实际调用突破 rpm_limit,
        // 突发幅度可达 `max_concurrency`。检查与占位必须原子。
        //
        // 预留可能被退还:`try_lease` 拿不到 permit 时没发出任何上游调用,
        // 由调用方 `rpm_refund_latest` 退回(见 `acquire_in_group`)。
        // 同一 lease 上的**重试**则额外各记一次,见 [`Self::note_upstream_call`]。
        if let Some(e) = entries.get_mut(&chosen) {
            e.last_selected_at = Some(now);
            e.rpm_note_selected(now);
        }
        Some(chosen)
    }

    /// 退还一次 RPM 预留(选号成功但没拿到 permit,故未发出上游调用)。
    fn rpm_refund(&self, account_id: &str) {
        let mut entries = self.entries.lock();
        if let Some(e) = entries.get_mut(account_id) {
            e.rpm_refund_latest();
        }
    }

    /// 记一次**额外的**上游调用(同一个 lease 上的第 2、3… 次)。
    ///
    /// 语义要点:选号时已经**预留**了第一次调用的名额(见 `select_id`),所以主链路的
    /// 首发调用**不该**再调本方法 —— 否则一次调用扣两格。本方法只用于同一 lease 上的
    /// 追加调用:token 刷新后重试、profileArn 修复后重试、Overloaded 退避的每一次重试、
    /// web search 的每一次续轮、人工探针(它不经过 select_id,故自己占一格)。
    ///
    /// 漏调一处 = 该路径的调用不计入 RPM = 号可能因超频被封。
    pub fn note_upstream_call(&self, account_id: &str) {
        let now = Instant::now();
        let mut entries = self.entries.lock();
        if let Some(e) = entries.get_mut(account_id) {
            e.rpm_note_selected(now);
        }
    }

    /// 取选中账号的并发许可 + 账号副本。permit 满返回 `Ok(None)`(调用方标 busy 重试);
    /// 账号不存在返回 `Ok(None)`。
    fn try_lease(&self, id: &str) -> Option<AccountLease> {
        let (sem, account, cut_epoch) = {
            let entries = self.entries.lock();
            let e = entries.get(id)?;
            (e.semaphore.clone(), e.account.clone(), e.epoch)
        };
        match sem.try_acquire_owned() {
            Ok(permit) => Some(AccountLease { account, _permit: permit, cut_epoch }),
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
        self.acquire_in_group(session_key, supports, None, true).await
    }

    /// [`Self::acquire_where`] + 按分组的成员视图。`view = None` 时与前者**逐字节等价**
    /// (这是"重构不改变现有分组行为"的机械保证,见测试
    /// `view_none_matches_acquire_where_exactly`)。
    ///
    /// 视图与 `supports` 的两点关键差异,别合并二者(见 [`GroupView`] 文档):
    /// - 视图过滤后的空集报 [`AcquireError::GroupEmpty`](503,可重试),
    ///   而非 `NoModelSupport`(400,客户端不重试);
    /// - **自愈按视图收窄**:全灭自愈只复活本组成员,绝不把别的组刚合法禁用的号一起
    ///   复活并清零失败计数(否则连续失败保护对所有组一起失效)。
    /// `allow_tier_hold` = 本轮取号是否允许「降层前先等高优先层的节流窗口」(见
    /// [`Self::tier_hold_wait`])。由调用方按**请求级墙上时钟**决定:请求开头的一小段
    /// 窗口内为便宜号等,窗口外照常降层兜底 —— 封顶的理由见
    /// [`gw_core::config::SchedulerConfig::tier_hold_window_ms`]。
    pub async fn acquire_in_group<F>(
        &self,
        session_key: Option<&str>,
        supports: F,
        view: Option<&GroupView>,
        allow_tier_hold: bool,
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
        // 排队等冷却的预算(热值)。`started` 是**唯一**的循环边界 —— 等待分支刻意不消耗
        // `attempts`(那是给 busy 换号用的预算),否则号一多就会在等到冷却前先被判 AllBusy。
        let (queue_wait, tier_hold) = {
            let t = self.tuning.read();
            (t.queue_wait, t.tier_hold)
        };
        let started = Instant::now();
        // 排队位在**首次**进入等待时取,跨轮持有到 acquire 返回(或 future 被 drop)。
        let mut queue_slot: Option<QueueSlot> = None;
        let mut attempts = 0;
        let mut busy: HashSet<String> = HashSet::new();
        let mut self_healed = false;

        loop {
            if attempts >= max_attempts {
                return Err(AcquireError::AllBusy);
            }
            let now = Instant::now();

            // 降层前先等:高优先层只是被 429 节流(几百毫秒)时,等它回来而不是把这个请求
            // 送给低优先级兜底池。必须在 `select_id` **之前** —— 后者会把会话亲和的 primary
            // 当场改钉到低层号上(「primary 不可用 → 立即改选并当场转正」),先选后等就晚了,
            // 还白烧一次 `MIGRATE_UP_DEBOUNCE` 的去抖额度。
            //
            // 刻意**不消耗 `attempts`**:那是给 busy 换号用的预算(`total * 5`),小组会被
            // 等待循环几轮耗光,把成功变成 `AllBusy`(503)。循环终止只由 `started.elapsed()
            // < tier_hold` 保证(单调 + 睡眠下限 10ms)。理由同上面排队分支的注释。
            // 也**不 `busy.clear()`**:busy 里是 permit 满的号,与节流无关,清掉只会让下一轮
            // 重选到同一个满号并真的烧掉 attempts。
            //
            // 剩余预算不足 `MIN_TIER_HOLD` 就**不再等**:睡不满一个有意义的片刻,却会让
            // 实际等待越过配置声明的上限(审查抓到的边界越界)。预算裁剪必须是**最后**
            // 一步,`.max()` 不能排在它后面。
            if allow_tier_hold && !tier_hold.is_zero() {
                let remaining = tier_hold.saturating_sub(started.elapsed());
                if remaining >= MIN_TIER_HOLD {
                    if let Some(d) = self.tier_hold_wait(&busy, now, &supports, view) {
                        let n = self
                            .tier_held_total
                            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        // 抖动:同一个号上的多个等待者同时醒来会一起打上游、再撞一片 429。
                        let jitter = Duration::from_millis((n % 7) * 8);
                        let wait = (d + jitter).max(MIN_TIER_HOLD).min(remaining);
                        tokio::time::sleep(wait).await;
                        continue;
                    }
                }
            }

            // 降层前先等**排队号的冷却**:排队号(自购速刷号)撞 429 走二值冷却时,
            // 上面的 `tier_hold_wait` 管不到(它只认节流窗口),于是量会漏给低优兜底池。
            // 预算用 `queue_wait`(排队开关本身的预算)而非 `tier_hold` —— 冷却是秒级,
            // 而 `tier_hold_ms` 是为几百毫秒的节流窗口设的,拿它当预算等于永不生效。
            //
            // 与下方 `select_id` else 分支里的排队等待**不重复**:那条只在"一个号都选不出"
            // 时触发,而本闸门治的正是"低优号可选、于是高优号的冷却根本没人等"。
            // 同样刻意**不消耗 `attempts`**(那是 busy 换号的预算)、**不 `busy.clear()`**
            // (busy 里是 permit 满的号,与冷却无关),理由同上面的 tier_hold 分支。
            if !queue_wait.is_zero() {
                let remaining = queue_wait.saturating_sub(started.elapsed());
                if remaining >= MIN_TIER_HOLD {
                    if let Some((d, cap)) =
                        self.queue_tier_hold_wait(&busy, now, &supports, view, remaining)
                    {
                        // **必须走队列准入**(对抗审查 [高]):否则任意数量的请求都能在这里
                        // 无界睡眠 —— sleeping future / 连接堆积,冷却结束时惊群一起打上游
                        // 再撞一片 429,而面板的 waiting/queued_total 全程显示 0(失真)。
                        // 队列已满就**不等、直接降层**:这个闸门是"省钱优化",不该为它
                        // 把请求拖到超时。降层本身是用户认可的兜底路径。
                        // 队列已满 → **不等**,直接跌落到下面的 select_id 正常降层。
                        // 这个闸门是"省钱优化",不该为它把请求拖到超时;降层本身就是
                        // 用户认可的兜底路径。不 return AllBusy —— 低优号很可能还能服务。
                        let admitted = queue_slot.is_some()
                            || match self.try_enter_queue(cap) {
                                Some(slot) => {
                                    queue_slot = Some(slot);
                                    true
                                }
                                None => false,
                            };
                        // 队列满就不等,落到下面的 select_id 常规降层。
                        // 不消耗 `attempts`:什么都没尝试过,扣预算会凭空缩短重试机会。
                        if admitted {
                            let n = self
                                .tier_held_total
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            // 抖动:一批等待者同时醒来会一起打上游、再撞一片 429。
                            let jitter = Duration::from_millis((n % 7) * 8);
                            // 上限 200ms 后回来重选:期间可能有同层号先恢复或 permit 释放,
                            // 早点回来比睡满整段更快;冷却未到则下一轮继续等(受 remaining 收敛)。
                            let wait = (d + jitter)
                                .clamp(MIN_TIER_HOLD, Duration::from_millis(200))
                                .min(remaining);
                            tokio::time::sleep(wait).await;
                            continue;
                        }
                        tracing::debug!("队列已满,放弃等高优排队号,按常规降层");
                    }
                }
            }

            let Some(id) = self.select_id(session_key, &busy, now, &supports, view) else {
                // 无合格号:区分"没号支持该模型" vs "本组无成员" vs "全 busy(有可用但
                // 占满)" vs "全禁用"。计数都只看**支持该模型**的号——不支持的号既救不了
                // busy 等待,也不该让错误从 NoModelSupport 误报成 AllDisabled。
                // `member_any`/`avail_*` 额外过一遍视图:非成员对本组不存在。
                let (
                    supported_any,
                    member_any,
                    avail_total,
                    avail_not_busy,
                    paced_soonest,
                    rpm_soonest,
                ) = {
                    let mut entries = self.entries.lock();
                    let mut any = false;
                    let mut member_any = false;
                    let mut avail = 0usize;
                    let mut not_busy = 0usize;
                    // 处于 429 节流窗口内的号:**没下线**,到点自己就回来。必须单独统计 ——
                    // 否则它既被 select_id 跳过、又被算进 avail_total,两边都不认领,
                    // 最后掉进 AllDisabled 分支把 503 抛给客户(本用例抓到的真实缺陷)。
                    let mut paced: Option<Duration> = None;
                    // RPM 等待**单独统计**,不与 paced 合并(对抗审查 gpt-5.6-sol [高]):
                    // paced 分支每轮只睡 ≤50ms 且消耗一次 `attempts`,那对几百毫秒的 429
                    // 窗口合适;但 RPM 等待是**秒级**(最长 60s),混进去会在恢复前把
                    // `attempts`(total×5)烧光 → 误报 AllBusy,还绕过队列容量控制。
                    let mut rpm_soonest: Option<Duration> = None;
                    for e in entries.values_mut() {
                        if !supports(&e.account) {
                            continue;
                        }
                        any = true;
                        if rank_of(view, e).is_none() {
                            continue;
                        }
                        member_any = true;
                        if e.disabled {
                            continue;
                        }
                        avail += 1;
                        // RPM 打满:号没下线、到点自己回来(与 paced 同属软状态),必须被
                        // 某个分支认领 —— 否则它既被 select_id 跳过、又被算进 avail_total,
                        // 两边都不认领 → 掉进 AllDisabled 把 503 抛给客户。
                        if let Some(d) = e.rpm_wait(now) {
                            if d > Duration::ZERO
                                && rpm_soonest.map(|m| d < m).unwrap_or(true)
                            {
                                rpm_soonest = Some(d);
                            }
                            continue;
                        }
                        if let Some(until) = e.paced_until {
                            let d = until.saturating_duration_since(now);
                            if d > Duration::ZERO {
                                if paced.map(|m| d < m).unwrap_or(true) {
                                    paced = Some(d);
                                }
                                // 节流中的号本轮不可选,不计入"未 busy 的可用号"。
                                continue;
                            }
                        }
                        if !busy.contains(&e.account.account_id) {
                            not_busy += 1;
                        }
                    }
                    (any, member_any, avail, not_busy, paced, rpm_soonest)
                };
                if !supported_any {
                    return Err(AcquireError::NoModelSupport);
                }
                // 本组一个能服务该模型的成员都没有(组没配号/配的号都不支持该模型):
                // 是配置态而非瞬时故障,但仍报可重试的 GroupEmpty —— 运维加条成员边就
                // 恢复,不该让客户端拿到"换模型才能解决"的 400。
                if !member_any {
                    return Err(AcquireError::GroupEmpty);
                }
                if avail_total > 0 && avail_not_busy == 0 && !busy.is_empty() {
                    // 有可用号但全 busy → 等并发释放后重试。
                    busy.clear();
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    attempts += 1;
                    continue;
                }
                // 号只是在 429 节流窗口内(未下线)→ 等窗口过去再选,**与队列开关无关**:
                // 节流窗口是我方自己设的、几百毫秒量级,不该让它变成客户的 503。
                if let Some(d) = paced_soonest {
                    busy.clear();
                    tokio::time::sleep(d.min(Duration::from_millis(50))).await;
                    attempts += 1;
                    continue;
                }
                // 全禁用:若有 TooManyFailures,做一次全灭自愈(等价重启)再试。
                // 只复活**支持本次模型、且属于本组**的号:opus 请求不该顺手复活无关 FREE
                // 失败号(审查②R Skeptic#4);低价组的请求也不该把正常组刚合法禁用的号
                // 一起复活并清零失败计数——那会让连续失败保护对所有组一起失效。
                //
                // ⚠️ 自愈是**最后手段**,触发条件必须是"整个 worker 池也没救了",不能只看
                // 本组视图(对抗审查 Skeptic#4)。否则一个只含单个坏号的小组,即使 worker
                // 里还有大量健康号,也能每个请求触发一次"全灭"自愈 → 该号被反复复活、
                // 连续失败保护对它彻底失效;而它可能同时属于别的组,污染面不止本组。
                if !self_healed
                    && self.whole_pool_exhausted(&supports)
                    && self.heal_too_many_failures(&supports, view)
                {
                    self_healed = true;
                    attempts += 1;
                    continue;
                }
                // 全禁用:在报错前先看有没有号只是**冷却中**、且会在预算内到期 ——
                // 有就等它自愈,把上游限速在网关内部消化掉,客户端只是变慢而不是拿到 503。
                // 这段等待发生在**响应开始之前**,不涉及协议改动、也不会产生半截流。
                if !queue_wait.is_zero() {
                    let elapsed = started.elapsed();
                    if elapsed < queue_wait {
                        if let Some((d, cap)) =
                            self.queue_probe(&supports, view, now, queue_wait - elapsed)
                        {
                            // 队列已满则不排,立刻报错 —— 再挤进来只是陪跑到超时,
                            // 客户等更久、结果一样。容量随可排队号的并发动态变化。
                            // 队列满(含 cap==0)则**不在这里报错**:等待理由可能纯粹是
                            // RPM ——`queue_probe` 刻意不把 RPM 满的号计入 cap(那容量吃不下),
                            // 于是"全部号都开排队且只是 RPM 满"会得到 cap==0 →
                            // try_enter_queue 失败。旧代码在这里 `return AllBusy`,让下面的
                            // rpm_soonest 分支**永远到不了**,请求立刻 503(对抗审查第二轮 [高])。
                            // 改为跌落:若确有 RPM 等待理由,由下面那条分支接管;否则才报错。
                            let admitted = queue_slot.is_some()
                                || match self.try_enter_queue(cap) {
                                    Some(slot) => {
                                        queue_slot = Some(slot);
                                        true
                                    }
                                    None => false,
                                };
                            if admitted {
                                // 上限 200ms:期间可能有并发槽先释放,早点回来重选;
                                // 下限 10ms:d 接近 0 时不空转。
                                tokio::time::sleep(d.clamp(
                                    Duration::from_millis(10),
                                    Duration::from_millis(200),
                                ))
                                .await;
                                busy.clear();
                                continue;
                            }
                            // 没排上且没有 RPM 理由 → 真的满了,报可重试错误。
                            if rpm_soonest.is_none() {
                                return Err(AcquireError::AllBusy);
                            }
                        }
                    }
                }
                // 全组都被自己的 RPM 闸卡住:配额一定会滑出窗口,等它,别把 503 抛给客户。
                //
                // 位置在 queue 分支**之后**:那条分支只认 `queue_enabled` 的号,而 RPM 可以
                // 设在任何号上(付费号恰恰不开排队),所以必须自己兜底。
                //
                // 与上面 paced 分支的关键差别 —— **不消耗 `attempts`**:RPM 等待是秒级,
                // 消耗预算会在配额恢复前先被判 AllBusy(对抗审查 [高])。循环终止改由
                // `queue_wait` 预算保证:超出即落到下面的 AllDisabled。
                // 分片 200ms 回来重选:期间可能有别的号恢复或 permit 释放,比睡满更快。
                if !queue_wait.is_zero() {
                    if let Some(d) = rpm_soonest {
                        let elapsed = started.elapsed();
                        if elapsed < queue_wait {
                            let remaining = queue_wait - elapsed;
                            // 配额恢复需要的时间超过整个预算 → 白等,直接降级报错。
                            if d <= remaining {
                                tokio::time::sleep(d.clamp(
                                    Duration::from_millis(10),
                                    Duration::from_millis(200),
                                ))
                                .await;
                                busy.clear();
                                continue;
                            }
                        }
                    }
                }
                return Err(AcquireError::AllDisabled);
            };

            match self.try_lease(&id) {
                Some(lease) => return Ok(lease),
                None => {
                    // 并发满:标 busy,换号重试。选号时预留的 RPM 名额要**退还** ——
                    // 这一轮没有发出任何上游调用,不退会让健康号被虚假限流近 60 秒。
                    self.rpm_refund(&id);
                    busy.insert(id);
                    attempts += 1;
                }
            }
        }
    }

    /// 全组排队实况(面板用)。容量口径与 [`Self::queue_probe`] 一致:
    /// 只算**开了排队且当前可服务**的号,跑干/禁用的不计入 —— 否则面板会显示一个
    /// 根本吃不下的容量,运维照它判断就会误以为还有余量。
    pub fn queue_stats(&self) -> QueueStats {
        let mut entries = self.entries.lock();
        let mut st = QueueStats {
            waiting: self.waiting.load(std::sync::atomic::Ordering::Relaxed),
            queued_total: self.queued_total.load(std::sync::atomic::Ordering::Relaxed),
            paced_total: self.paced_total.load(std::sync::atomic::Ordering::Relaxed),
            tier_held_total: self.tier_held_total.load(std::sync::atomic::Ordering::Relaxed),
            ..Default::default()
        };
        let now = Instant::now();
        for e in entries.values_mut() {
            if !queue_enabled(&e.account) {
                continue;
            }
            st.enabled_accounts += 1;
            // 口径必须与 `queue_probe` 一致(对抗审查 [低]):RPM 打满的号此刻**吃不下**,
            // 按满并发计入会让 /health 报出并不存在的可服务容量,误导容量与事故判断。
            if !e.disabled && !e.rpm_exhausted(now) {
                st.capacity = st.capacity.saturating_add(e.account.max_concurrency as usize);
            }
        }
        st
    }

    /// 取一个排队位;已达容量则 `None`(调用方据此**快速失败**,不再堆积)。
    fn try_enter_queue(&self, cap: usize) -> Option<QueueSlot<'_>> {
        use std::sync::atomic::Ordering::Relaxed;
        if cap == 0 {
            return None;
        }
        // fetch_add 后回滚的写法在过冲时可能短暂超一点,但不会漏放守卫;
        // 阈值本身是近似的,不值得为它上 CAS 循环。
        if self.waiting.fetch_add(1, Relaxed) >= cap {
            self.waiting.fetch_sub(1, Relaxed);
            return None;
        }
        self.queued_total.fetch_add(1, Relaxed);
        Some(QueueSlot(&self.waiting))
    }

    /// 排队准入探测:一把锁里同时算出「还要等多久」和「队列容量」。
    ///
    /// 返回 `None` = **没有等得到的号**,此时排队毫无意义,应当立刻报错而不是把客户挂满
    /// 整个预算(那会把容量问题放大成全站卡死):
    /// - 账号没开 `extra.queue_enabled` → 不为它等(见 [`queue_enabled`]);
    /// - **额度跑干**(`QuotaExhausted`)的 `disabled_until` 是 `None`,不构成等待理由;
    /// - `config` 禁用(DB `disabled=1`)同理;
    /// - 到期时刻在 `budget` 之外的(如 1h 的 `TemporarilySuspended`)也算等不到。
    ///
    /// `Some((d, cap))` 的 `cap` = **本组内已开排队的号的并发之和**。用它当在途等待数的
    /// 上限:等待者再多也吃不下更多并发,超出部分只是排在后面陪跑到超时。取 1× 而非
    /// 更大的倍数,是让最坏等待≈一次请求的周转时间,而不是几轮。
    fn queue_probe(
        &self,
        supports: &dyn Fn(&Account) -> bool,
        view: Option<&GroupView>,
        now: Instant,
        budget: Duration,
    ) -> Option<(Duration, usize)> {
        let mut entries = self.entries.lock();
        let mut soonest: Option<Duration> = None;
        let mut cap: usize = 0;
        for e in entries.values_mut() {
            if !supports(&e.account) || rank_of(view, e).is_none() || !queue_enabled(&e.account) {
                continue;
            }
            if !e.disabled {
                // RPM 打满:号还活着,但本窗口内**吃不下**了。既是等待理由(配额会滑出窗口),
                // 也**不得按满并发计入容量** —— 否则一个被 RPM 卡住的号会把 cap 撑大,
                // 等待者远超真实吞吐,全部排到超时(正是排队开关本要防的堆积)。
                if let Some(d) = e.rpm_wait(now) {
                    if d > Duration::ZERO && d <= budget && soonest.map(|m| d < m).unwrap_or(true) {
                        soonest = Some(d);
                    }
                    continue;
                }
                // 正在服务的号:并发全额计入容量。
                cap = cap.saturating_add(e.account.max_concurrency as usize);
                // 号没下线但在 429 节流窗口内:这是**最常见**的"等一下就有"情形,
                // 必须算作等待理由 —— 否则节流一开,全组恰好都在节流窗口时会误报 503。
                if let Some(until) = e.paced_until {
                    let d = until.saturating_duration_since(now);
                    if d > Duration::ZERO && d <= budget && soonest.map(|m| d < m).unwrap_or(true) {
                        soonest = Some(d);
                    }
                }
                continue;
            }
            // 禁用的号只有在**预算内会自愈**时才算数 —— 额度跑干/config 禁用/1h 封禁
            // 既不产生等待理由,也**不得计入容量**。否则一堆跑干的号会把 cap 撑大,
            // 等待者远超真实吞吐,全部排到超时(正是本开关要防的堆积)。
            if !e.disabled_reason.map(|r| r.is_cooldown()).unwrap_or(false) {
                continue;
            }
            let Some(until) = e.disabled_until else { continue };
            let d = until.saturating_duration_since(now);
            if d > budget {
                continue;
            }
            cap = cap.saturating_add(e.account.max_concurrency as usize);
            if soonest.map(|m| d < m).unwrap_or(true) {
                soonest = Some(d);
            }
        }
        soonest.map(|d| (d, cap))
    }

    /// 整个 worker 池(**不看分组视图**)是否已无任何可服务本模型的启用号。
    ///
    /// 这是自愈的前置闸门:自愈语义是"等价重启的最后手段",必须由**全池**告罄触发。
    /// 只看本组视图会让小组把它变成常规操作,见 `acquire_in_group` 里的说明。
    fn whole_pool_exhausted(&self, supports: &dyn Fn(&Account) -> bool) -> bool {
        let entries = self.entries.lock();
        !entries.values().any(|e| !e.disabled && supports(&e.account))
    }

    /// 全灭自愈:若存在 TooManyFailures 禁用的号,清其禁用 + 失败计数(等价重启)。
    /// 只动 `supports` 通过(支持本次模型)**且属于本组**(在 `view` 内)的号。
    /// 返回是否实际恢复了至少一个号。
    ///
    /// 按视图收窄是必须的:自愈由"本组全灭"触发,若复活范围是全局,低价组一次全灭就会
    /// 把正常组刚合法禁用的号一起复活并清零失败计数,连续失败保护对所有组一起失效。
    fn heal_too_many_failures(
        &self,
        supports: &dyn Fn(&Account) -> bool,
        view: Option<&GroupView>,
    ) -> bool {
        let mut entries = self.entries.lock();
        let mut healed = false;
        for e in entries.values_mut() {
            if rank_of(view, e).is_none() {
                continue;
            }
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
                            // 软冷却同理:重启用 = 新运行态,换发代际使在途旧请求的
                            // upstream_cut 上报失效,冷却状态清零(评审#7)。
                            e.epoch = next_cut_epoch();
                            e.cuts.clear();
                            e.drain_until = None;
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
                    e.default_rank = acc
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
        let mut snap: Vec<AccountStatusSnapshot> = entries
            .values_mut()
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
                    priority: e.default_rank,
                    disabled: e.disabled,
                    reason: reason.to_string(),
                    cooldown_remaining_secs: e
                        .disabled_until
                        .map(|t| t.saturating_duration_since(now).as_secs())
                        .unwrap_or(0),
                    failure_count: e.failure_count,
                    available_permits: e.semaphore.available_permits(),
                    max_concurrency: e.account.max_concurrency,
                    queue_enabled: queue_enabled(&e.account),
                    rpm_limit: rpm_limit(&e.account),
                    rpm_used: e.rpm_used(now),
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
        // 429 节流窗口与 strikes 也要清 —— "救号"就该让它立刻可用。
        e.paced_until = None;
        e.rate_limit_strikes = 0;
        // RPM 滑动窗口同样清空(对抗审查 [中]):不清的话 admin reset 返回成功、
        // 面板显示已启用,号却仍被自己的 RPM 闸挡在选号池外直到旧 hit 自然过期,
        // 运维完全看不出原因。
        e.rpm_hits.clear();
        // 软冷却同样换发新代际并清空:reset 后的号是「新运行态」,老流迟到的
        // upstream_cut 上报按代际不符丢弃(见 report_upstream_cut,评审#7)。
        e.epoch = next_cut_epoch();
        e.cuts.clear();
        e.drain_until = None;
        tracing::info!(account = %id, "admin reset:清运行时禁用与计数(含节流/RPM 窗口)");
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
            // 成功即清零 429 连击并解除节流闸:熔断只该对**持续**限流生效,
            // 竞争期间偶发的 429 不该累积到把号打回二值冷却。
            e.rate_limit_strikes = 0;
            e.paced_until = None;
        }
    }

    /// 上报一次「上游静默掐流」前兆(provider 显式信号,见 `StreamItem::UpstreamCut`)。
    ///
    /// 软冷却:滑窗内累计达阈值 → 该号进入 draining(**不接新会话**;已有亲和仍粘着;
    /// normal 全空时 fail-open 仍服务)。与既有健康/禁用体系**完全隔离**:不碰
    /// failure_count / disabled / 429 节流,失败方向永远安全
    /// (2026-07-25 激进信号接入健康上报、35 秒禁光 7 个号的事故教训)。
    ///
    /// `epoch` = lease 选号时的软冷却代际快照;reset / 配置重启用 / 移除后重新加入
    /// 都会换发新代际并清空 cuts/drain_until,旧代际的迟到上报一律丢弃
    /// (老流的 Drop 不得把前兆记到「救回来」的新运行态上,codex 对抗评审#7)。
    pub fn report_upstream_cut(&self, id: &str, epoch: u64) {
        let t = drain_tuning();
        // drain 时长 clamp 到 ≤ 亲和 TTL:冷却期若比亲和 TTL 还长,指向该号的会话
        // 会先掉出亲和、被 perceived 成新会话而遭排除,「同会话重试仍路由回它」的
        // 反检测设计就失效了(codex 对抗评审#6)。
        let affinity_ttl = self.tuning.read().affinity_ttl;
        let secs = if t.secs > affinity_ttl {
            tracing::warn!(
                drain_secs = t.secs.as_secs(),
                affinity_ttl_secs = affinity_ttl.as_secs(),
                "KIRO_DRAIN_SECS 超过亲和 TTL,已 clamp(过长会让会话在冷却期内掉出亲和)"
            );
            affinity_ttl
        } else {
            t.secs
        };
        self.report_upstream_cut_at(id, epoch, Instant::now(), t.window, t.threshold, secs);
    }

    /// [`Self::report_upstream_cut`] 的参数注入内核(测试用固定时钟/参数驱动)。
    fn report_upstream_cut_at(
        &self,
        id: &str,
        epoch: u64,
        now: Instant,
        window: Duration,
        threshold: u32,
        secs: Duration,
    ) {
        let mut entries = self.entries.lock();
        let Some(e) = entries.get_mut(id) else { return };
        if epoch != e.epoch {
            tracing::debug!(account = %id, "丢弃旧代际的 upstream_cut 上报(reset/重启用后的迟到回声)");
            return;
        }
        // 滑窗剪枝 + 有界 push(双保险:窗口外的丢弃,条数再封顶 CUTS_CAP)。
        while e.cuts.front().is_some_and(|t| now.duration_since(*t) >= window) {
            e.cuts.pop_front();
        }
        e.cuts.push_back(now);
        while e.cuts.len() > CUTS_CAP {
            e.cuts.pop_front();
        }
        if e.cuts.len() as u32 >= threshold {
            // 每次命中都续期(前兆仍在出现 = 上游还在掐,冷却跟着延续)。
            e.drain_until = Some(now + secs);
            tracing::info!(
                account = %id,
                cuts = e.cuts.len(),
                drain_secs = secs.as_secs(),
                "upstream_cut 滑窗达阈值,进入软冷却(draining:不接新会话,已有亲和保持)"
            );
        }
    }

    /// 该号此刻是否处于软冷却(draining)。仅观测/测试用;选号路径在
    /// `select_id` 内联判定(同一把锁里分组,不额外加锁)。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn is_draining(&self, id: &str) -> bool {
        let now = Instant::now();
        self.entries
            .lock()
            .get(id)
            .is_some_and(|e| e.drain_until.is_some_and(|t| now < t))
    }

    /// 读账号当前软冷却代际(测试与无 lease 路径快照用)。
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn current_cut_epoch(&self, id: &str) -> u64 {
        self.entries.lock().get(id).map(|e| e.epoch).unwrap_or(0)
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
                e.rate_limit_strikes = e.rate_limit_strikes.saturating_add(1);
                // 开了排队的号:429 是**跨租户竞争**,不是我方过载 —— 把号整个拉出轮转
                // 只会让请求转去烧别的号(而企业号往往正握着大部分剩余额度)。
                // 改成节流:号不下线,只是 pace 毫秒内不被选中,到点继续抢。
                // 熔断:连续撞太多次说明上游是真把它限死了,退回二值冷却别硬撞。
                // 熔断阈值 0 = 永不退回冷却。429 只是竞争,真正的下线信号是
                // 403 TEMPORARILY_SUSPENDED(走 TemporarilyBlocked 分支,1h 冷却)。
                let paced = queue_enabled(&e.account)
                    && !tuning.rate_limit_pace.is_zero()
                    && (tuning.rate_limit_pace_max_strikes == 0
                        || e.rate_limit_strikes <= tuning.rate_limit_pace_max_strikes);
                if paced {
                    self.paced_total
                        .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    e.paced_until = Some(now + tuning.rate_limit_pace);
                    tracing::debug!(
                        account = %id, strikes = e.rate_limit_strikes,
                        "命中限流,节流 {}ms(号不下线)", tuning.rate_limit_pace.as_millis(),
                    );
                } else {
                    e.disabled = true;
                    e.disabled_reason = Some(DisabledReason::RateLimited);
                    e.disabled_until = Some(now + tuning.rate_limit_cooldown);
                    e.paced_until = None;
                    tracing::warn!(account = %id, strikes = e.rate_limit_strikes,
                        "命中限流,冷却 {}s", tuning.rate_limit_cooldown.as_secs());
                }
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

    /// 开了 `extra.queue_enabled` 的账号(企业号形态)。
    fn qacct(id: &str, concurrency: u32) -> Arc<Account> {
        let mut extra = BTreeMap::new();
        extra.insert("queue_enabled".to_string(), serde_json::json!(true));
        Arc::new(Account {
            account_id: id.to_string(),
            provider: "kiro".into(),
            max_concurrency: concurrency,
            disabled: false,
            extra,
        })
    }

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

    /// 开了排队**且**带优先级的号(自购速刷号形态:挂最高层 + 允许节流)。
    fn qacct_p(id: &str, concurrency: u32, priority: i64) -> Arc<Account> {
        let mut extra = BTreeMap::new();
        extra.insert("queue_enabled".to_string(), serde_json::json!(true));
        extra.insert("priority".to_string(), serde_json::json!(priority));
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

    /// 带 RPM 上限的号(付费号形态:低并发 + 定频)。
    fn racct(id: &str, concurrency: u32, priority: i64, rpm: i64) -> Arc<Account> {
        let mut extra = BTreeMap::new();
        extra.insert("priority".to_string(), serde_json::json!(priority));
        extra.insert("rpm_limit".to_string(), serde_json::json!(rpm));
        Arc::new(Account {
            account_id: id.to_string(),
            provider: "kiro".into(),
            max_concurrency: concurrency,
            disabled: false,
            extra,
        })
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

    /// **账号级开关**:没开 `queue_enabled` 的号(社交号形态)即使在冷却里也不为它等 ——
    /// 那类 429 常伴随额度见底,等待只是把客户多挂几秒后照样报错。
    #[tokio::test]
    async fn queue_ignores_accounts_without_the_per_account_flag() {
        let cfg = SchedulerConfig {
            rate_limit_cooldown_secs: 1,
            queue_wait_ms: 5_000,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![acct("plain", 2, None)], &cfg);
        s.report_failure("plain", UpstreamErrorKind::RateLimited);
        let t0 = Instant::now();
        let err = match s.acquire(Some("s")).await {
            Ok(_) => panic!("本用例期望取号失败"),
            Err(e) => e,
        };
        assert!(matches!(err, AcquireError::AllDisabled), "实际={err:?}");
        assert!(t0.elapsed() < Duration::from_millis(200), "不该为没开开关的号等待: {:?}", t0.elapsed());
    }

    /// **队列容量按并发动态定**:容量 = 可排队号的并发之和。超出的请求立刻失败,
    /// 不堆在同一个号上陪跑到超时。conc=2 的单号 → 最多 2 个在途等待,第 3 个即刻被拒。
    #[tokio::test]
    async fn queue_depth_is_capped_by_total_concurrency() {
        let cfg = SchedulerConfig {
            rate_limit_cooldown_secs: 3,
            queue_wait_ms: 5_000,
            ..SchedulerConfig::default()
        };
        let s = Arc::new(AccountScheduler::new(vec![qacct("a", 2)], &cfg));
        s.report_failure("a", UpstreamErrorKind::RateLimited);

        // 先占满 2 个排队位(它们会一直等到冷却到期)。
        let mut waiters = Vec::new();
        for _ in 0..2 {
            let sc = s.clone();
            waiters.push(tokio::spawn(async move { sc.acquire(Some("s")).await.is_ok() }));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;

        // 第 3 个:队列满 → 立刻 AllBusy,而不是陪等 3 秒。
        let t0 = Instant::now();
        let err = match s.acquire(Some("s")).await {
            Ok(_) => panic!("队列已满时不该拿到租约"),
            Err(e) => e,
        };
        assert!(matches!(err, AcquireError::AllBusy), "队列满应报 AllBusy,实际={err:?}");
        assert!(t0.elapsed() < Duration::from_millis(200), "满队列应立刻失败: {:?}", t0.elapsed());

        // 守卫是 RAII:等待者结束后位置必须归还,否则队列会永久“满”。
        for w in waiters {
            assert!(w.await.unwrap(), "先入队的请求应等到冷却到期并拿到租约");
        }
        assert_eq!(s.waiting.load(std::sync::atomic::Ordering::Relaxed), 0, "排队位必须全部归还");
    }

    /// **容量口径**:额度跑干的号即使开了排队开关,也不得计入队列容量 ——
    /// 否则一堆跑干的号把 cap 撑大,等待者远超真实吞吐、全部排到超时。
    /// 这里 a(健康,conc=1)+ b(跑干,conc=10):容量应是 1 而不是 11。
    #[tokio::test]
    async fn queue_capacity_excludes_quota_exhausted_accounts() {
        let cfg = SchedulerConfig {
            rate_limit_cooldown_secs: 3,
            queue_wait_ms: 5_000,
            ..SchedulerConfig::default()
        };
        let s = Arc::new(AccountScheduler::new(vec![qacct("a", 1), qacct("b", 10)], &cfg));
        s.report_failure("b", UpstreamErrorKind::QuotaExhausted);
        // a 限流冷却中 → 有等待理由;容量只应来自 a(=1)。
        s.report_failure("a", UpstreamErrorKind::RateLimited);

        let sc = s.clone();
        let w = tokio::spawn(async move { sc.acquire(Some("s")).await.is_ok() });
        tokio::time::sleep(Duration::from_millis(150)).await;

        let t0 = Instant::now();
        let err = match s.acquire(Some("s")).await {
            Ok(_) => panic!("容量为 1 时第二个请求不该入队"),
            Err(e) => e,
        };
        assert!(matches!(err, AcquireError::AllBusy), "实际={err:?}");
        assert!(t0.elapsed() < Duration::from_millis(200), "应立刻失败: {:?}", t0.elapsed());
        assert!(w.await.unwrap(), "先入队的应等到 a 冷却到期");
        assert_eq!(s.waiting.load(std::sync::atomic::Ordering::Relaxed), 0, "排队位必须归还");
    }

    /// 节流的**本质区别**:429 后账号**不下线**(`disabled=false`,面板仍显示正常),
    /// 只是 pace 窗口内不被选中,到点自动可选 —— 这就是"保持一个频率访问"。
    #[tokio::test]
    async fn paced_account_is_not_disabled_and_returns_by_itself() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 120,
            rate_limit_cooldown_secs: 600, // 故意设很大:若走了二值冷却,本用例必然超时失败
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![qacct("a", 2)], &cfg);
        s.report_failure("a", UpstreamErrorKind::RateLimited);

        let snap = s.status_snapshot();
        assert!(!snap[0].disabled, "节流不该把号下线");
        assert_eq!(snap[0].reason, "", "面板 reason 应仍为正常,而不是 rate_limited");

        // 窗口内不可选。
        let t0 = Instant::now();
        let lease = s.acquire(Some("s")).await;
        assert!(lease.is_ok(), "单号被节流后应等窗口过去再拿到,而不是报错");
        assert!(t0.elapsed() >= Duration::from_millis(100), "应等过节流窗口: {:?}", t0.elapsed());
        assert!(t0.elapsed() < Duration::from_secs(5), "不该退化成 600s 冷却: {:?}", t0.elapsed());
    }

    /// 节流**只对开了排队的号**生效;普通号照旧走二值冷却(行为不变)。
    #[tokio::test]
    async fn pacing_only_applies_to_queue_enabled_accounts() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 120,
            rate_limit_cooldown_secs: 600,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![acct("plain", 2, None)], &cfg);
        s.report_failure("plain", UpstreamErrorKind::RateLimited);
        let snap = s.status_snapshot();
        assert!(snap[0].disabled, "普通号仍应被二值冷却下线");
        assert_eq!(snap[0].reason, "rate_limited");
    }

    /// **默认语义**:熔断阈值 0 = 开了排队的号**永远不因 429 下线**,只节流。
    /// 唯一能让它下线的是真被上游 suspend(走 TemporarilyBlocked 分支)。
    #[tokio::test]
    async fn queue_account_never_cools_down_on_429_by_default() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 30,
            rate_limit_cooldown_secs: 600,
            ..SchedulerConfig::default()
        };
        assert_eq!(
            SchedulerConfig::default().rate_limit_pace_max_strikes,
            0,
            "默认必须是不熔断"
        );
        let s = AccountScheduler::new(vec![qacct("a", 2)], &cfg);
        for i in 1..=50 {
            s.report_failure("a", UpstreamErrorKind::RateLimited);
            assert!(!s.status_snapshot()[0].disabled, "第 {i} 次 429 也不该下线");
        }
        // 但真被 suspend 时必须下线 —— 这是唯一的下线信号。
        s.report_failure("a", UpstreamErrorKind::TemporarilyBlocked);
        let snap = s.status_snapshot();
        assert!(snap[0].disabled, "被 suspend 必须下线");
        assert_eq!(snap[0].reason, "temporarily_suspended");
    }

    /// **熔断**:连续 429 撞满阈值后放弃节流、退回二值冷却 —— 上游真把号限死时
    /// 不能无限定频硬撞(历史上 22 分钟送走 5 个号)。
    #[tokio::test]
    async fn pacing_falls_back_to_cooldown_after_consecutive_strikes() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 50,
            rate_limit_pace_max_strikes: 3,
            rate_limit_cooldown_secs: 600,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![qacct("a", 2)], &cfg);
        for i in 1..=3 {
            s.report_failure("a", UpstreamErrorKind::RateLimited);
            assert!(!s.status_snapshot()[0].disabled, "第 {i} 次仍应是节流,不下线");
        }
        s.report_failure("a", UpstreamErrorKind::RateLimited); // 第 4 次:超阈值
        let snap = s.status_snapshot();
        assert!(snap[0].disabled, "撞满连击后应退回二值冷却");
        assert_eq!(snap[0].reason, "rate_limited");
    }

    // ——— 降层前先等(tier hold)———
    //
    // 场景是生产实测出来的:自购速刷号挂 priority 0、PRO+ 兜底号挂 100,速刷号被 429
    // 节流那几百毫秒里,请求会**立刻**掉到 100 层(30 分钟窗口漏了 169 个,与 429 次数
    // 分钟级 1:1)。下面七条锁住新开关的边界。

    /// 双档配置:hi 在最高层且开了排队(会被节流),lo 是低优先兜底。
    fn tier_pair(pace_ms: u64, hold_ms: u64) -> AccountScheduler {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: pace_ms,
            tier_hold_ms: hold_ms,
            tier_hold_window_ms: 2_000,
            rate_limit_cooldown_secs: 600, // 故意很大:走了二值冷却本组用例必然超时
            ..SchedulerConfig::default()
        };
        AccountScheduler::new(vec![qacct_p("hi", 10, 0), acct("lo", 10, Some(100))], &cfg)
    }

    /// 核心行为:高优先层只是被节流时,**等它回来**而不是把量送给低优先兜底池。
    #[tokio::test]
    async fn tier_hold_waits_for_paced_top_tier_instead_of_descending() {
        let s = tier_pair(120, 400);
        s.report_failure("hi", UpstreamErrorKind::RateLimited);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "hi", "节流窗口内不该降层到兜底号");
        assert!(t0.elapsed() >= Duration::from_millis(100), "应等过节流窗口: {:?}", t0.elapsed());
    }

    /// 预算用尽必须**降层成功**,而不是把错误抛给客户 —— 这条是整个开关的安全底线。
    #[tokio::test]
    async fn tier_hold_falls_through_to_lower_tier_when_budget_exhausted() {
        let s = tier_pair(5_000, 100); // 节流 5s 远超 100ms 预算
        s.report_failure("hi", UpstreamErrorKind::RateLimited);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "lo", "预算用尽应降层兜底");
        assert!(t0.elapsed() < Duration::from_secs(1), "不该傻等满 5s: {:?}", t0.elapsed());
    }

    /// `tier_hold_ms = 0`(默认)= 开关关闭,与引入前逐字节等价:节流即降层。
    #[tokio::test]
    async fn tier_hold_disabled_descends_immediately() {
        assert_eq!(SchedulerConfig::default().tier_hold_ms, 0, "默认必须是关");
        let s = tier_pair(5_000, 0);
        s.report_failure("hi", UpstreamErrorKind::RateLimited);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "lo");
        assert!(t0.elapsed() < Duration::from_millis(80), "关掉时不该有等待: {:?}", t0.elapsed());
    }

    /// **只等节流,不等冷却**:高层号是被二值冷却下线的(非 queue_enabled 号的 429 归宿),
    /// 那是 `queue_wait` 的职责,tier hold 不抢这个活,否则两套预算叠加。
    #[tokio::test]
    async fn tier_hold_ignores_cooled_down_top_tier() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 120,
            tier_hold_ms: 400,
            tier_hold_window_ms: 2_000,
            rate_limit_cooldown_secs: 600,
            ..SchedulerConfig::default()
        };
        // hi **不开**排队 → 429 走二值冷却(disabled),不是节流。
        let s = AccountScheduler::new(vec![acct("hi", 10, Some(0)), acct("lo", 10, Some(100))], &cfg);
        s.report_failure("hi", UpstreamErrorKind::RateLimited);
        assert!(s.status_snapshot().iter().find(|x| x.account_id == "hi").unwrap().disabled);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "lo");
        assert!(t0.elapsed() < Duration::from_millis(80), "冷却态不归 tier hold 管: {:?}", t0.elapsed());
    }

    /// **只要还有排队模式的高优先号会在预算内回来,就不许降层。**
    ///
    /// 用户口径(2026-08-09):"只要我有排队模式的号,低优先就不应该吃流量;如果没有,
    /// 那高优先的正常掉到低优先"。排队号(`queue_enabled`)是自购速刷号:它们撞 429 是
    /// 跨租户竞争,冷却几秒就回来 —— 这几秒把量送给低优兜底池,等于用被封风险换几秒延迟。
    ///
    /// 与 [`Self::tier_hold_wait`] 的区别:那个只认 `paced_until`(429 节流窗口),
    /// 冷却态(`disabled` + cooldown)一律立即降层。可 `rate_limit_pace_max_strikes`
    /// 熔断后、以及 `queue_enabled` 但未开节流的号,撞 429 走的正是二值冷却分支 ——
    /// 生产上 G0 高优层 29 个号全是 `queue_enabled`,冷却 2s,于是每次 429 都漏一批量
    /// 到只有 31 个 permit 的低优层,把本已半死的兜底号打到封禁。
    #[tokio::test]
    async fn queue_tier_hold_waits_for_cooling_queue_account_instead_of_descending() {
        let cfg = SchedulerConfig {
            // 走**二值冷却**(不是节流):pace 关掉,冷却 200ms —— 预算内会自愈。
            rate_limit_pace_ms: 0,
            rate_limit_cooldown_secs: 1,
            queue_wait_ms: 2_000,
            ..SchedulerConfig::default()
        };
        // hi = 排队号挂最高层;lo = 普通兜底号。
        let s = AccountScheduler::new(vec![qacct_p("hi", 10, 0), acct("lo", 10, Some(100))], &cfg);
        s.report_failure("hi", UpstreamErrorKind::RateLimited);
        assert!(
            s.status_snapshot().iter().find(|x| x.account_id == "hi").unwrap().disabled,
            "前提:hi 走的是二值冷却(disabled),不是节流"
        );

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(
            lease.account_id(),
            "hi",
            "排队号冷却中且预算内会回来 → 必须等它,不许把量送给低优兜底号"
        );
        assert!(t0.elapsed() >= Duration::from_millis(500), "应等过冷却: {:?}", t0.elapsed());
    }

    /// 边界:排队号的冷却**超出预算**时照常降层 —— 用户口径的后半句
    /// "如果没有,那高优先的正常掉到低优先"。1h 封禁属于这一类。
    #[tokio::test]
    async fn queue_tier_hold_descends_when_cooldown_exceeds_budget() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 0,
            rate_limit_cooldown_secs: 600, // 远超预算
            queue_wait_ms: 200,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![qacct_p("hi", 10, 0), acct("lo", 10, Some(100))], &cfg);
        s.report_failure("hi", UpstreamErrorKind::RateLimited);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "lo", "排队号短期回不来 → 正常降层兜底");
        assert!(t0.elapsed() < Duration::from_secs(1), "不该傻等满 600s: {:?}", t0.elapsed());
    }

    /// 封禁(1h)不该把请求钉在高层等 —— `suspended_cooldown` 远超预算,必须降层。
    #[tokio::test]
    async fn queue_tier_hold_descends_on_long_ban() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 0,
            queue_wait_ms: 300,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![qacct_p("hi", 10, 0), acct("lo", 10, Some(100))], &cfg);
        s.report_failure("hi", UpstreamErrorKind::TemporarilyBlocked);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "lo", "1h 封禁远超预算 → 降层");
        assert!(t0.elapsed() < Duration::from_secs(1), "不该等封禁: {:?}", t0.elapsed());
    }

    // ─────────────────────────── RPM 定频闸门 ───────────────────────────
    //
    // 背景(2026-08-09 实测):`kiro-apikey-a3863547bf6b` 一生 75 分钟、2037 次调用,
    // 均值 27 次/分、峰值 53 次/分,随后收到
    //   403 {"reason":"TEMPORARILY_SUSPENDED","message":"...We detected unusual user
    //        activity and locked it as a security precaution..."}
    // 全程 `max_concurrency` 从未被突破 —— 并发限的是"同时几个",管不了"一分钟几个"。
    // 付费 social 号同期 `RateLimited`=0、`EmptyResponse`=0:上游**不发预警**,第一个
    // 信号就是封号,所以所有"等上游软信号再退避"的冷却参数在它们身上从不执行。

    /// 取号 = 首发调用的名额在 `select_id` 里**已预留**,所以这里不再额外记账。
    /// (生产里 `note_upstream_call` 只用于同一 lease 上的追加调用。)
    async fn acquire_and_call(s: &AccountScheduler, key: &str) -> String {
        let l = s.acquire(Some(key)).await.unwrap();
        l.account_id().to_string()
    }

    #[tokio::test]
    async fn rpm_limit_blocks_selection_after_quota_used_up() {
        // rpm=2:两次真实调用后该号本窗口内不可选,请求落到不限速的兜底号。
        let s = sched(vec![racct("hi", 10, 0, 2), acct("lo", 10, Some(100))]);
        assert_eq!(acquire_and_call(&s, "s1").await, "hi", "第 1 次:配额充足");
        assert_eq!(acquire_and_call(&s, "s2").await, "hi", "第 2 次:配额刚好用完");
        assert_eq!(acquire_and_call(&s, "s3").await, "lo", "第 3 次:RPM 打满 → 不选它");
    }

    /// **同一个 lease 上的重试也必须计数**(对抗审查抓到的最严重缺陷):
    /// token 刷新重试 / profileArn 修复重试 / Overloaded 退避都会在同号再打一次上游,
    /// 只按"选号次数"记账会让实际调用数数倍超限,定频防封失效。
    #[tokio::test]
    async fn rpm_counts_every_upstream_call_not_just_selections() {
        let s = sched(vec![racct("a", 10, 0, 3), acct("lo", 10, Some(100))]);
        let lease = s.acquire(Some("s")).await.unwrap();
        assert_eq!(lease.account_id(), "a");
        // 一次选号 = 首发调用(名额已预留)+ 两次同号重试(各记一次)= 共 3 次。
        s.note_upstream_call("a");
        s.note_upstream_call("a");
        drop(lease);
        let snap = s.status_snapshot();
        let a = snap.iter().find(|x| x.account_id == "a").unwrap();
        assert_eq!(a.rpm_used, 3, "三次真实调用必须记三次");
        // 配额已满 → 下一个请求不该再选它。
        assert_eq!(acquire_and_call(&s, "s2").await, "lo", "配额满 → 降到兜底号");
    }

    /// **并发准入必须原子**(对抗审查第二轮 [高]):只剩 1 个名额时,多个并发请求
    /// 不得在第一个记账之前全部通过检查 —— 那会让实际调用突破 rpm_limit,
    /// 突发幅度可达 `max_concurrency`(生产上是 100)。
    #[tokio::test]
    async fn rpm_admission_is_atomic_under_concurrency() {
        // hi 有 20 个 permit 但 rpm 只剩 3 → 并发打 10 个,只有 3 个能落在 hi。
        let s = std::sync::Arc::new(sched(vec![racct("hi", 20, 0, 3), acct("lo", 20, Some(100))]));
        let mut handles = Vec::new();
        for i in 0..10 {
            let s2 = s.clone();
            handles.push(tokio::spawn(async move {
                let l = s2.acquire(Some(&format!("s{i}"))).await.unwrap();
                let id = l.account_id().to_string();
                // 持住 lease 直到全部取完,确保不是靠 permit 释放来串行化。
                tokio::time::sleep(Duration::from_millis(120)).await;
                id
            }));
        }
        let mut on_hi = 0;
        for h in handles {
            if h.await.unwrap() == "hi" {
                on_hi += 1;
            }
        }
        assert_eq!(on_hi, 3, "只剩 3 个名额,不该有第 4 个请求落在 hi(实际 {on_hi})");
        assert_eq!(s.status_snapshot().iter().find(|x| x.account_id == "hi").unwrap().rpm_used, 3);
    }

    /// permit 抢不到而重选别的号时,**不该**扣掉本号配额 —— 没发出任何上游请求。
    #[tokio::test]
    async fn rpm_not_consumed_when_lease_fails_no_upstream_call() {
        // hi 只有 1 个 permit 且被占住 → 第二个请求会重选到 lo,hi 不该扣配额。
        let s = sched(vec![racct("hi", 1, 0, 5), acct("lo", 10, Some(100))]);
        let hog = s.acquire(Some("hog")).await.unwrap();
        assert_eq!(hog.account_id(), "hi"); // 占位者的名额已在选号时预留
        let second = s.acquire(Some("s2")).await.unwrap();
        assert_eq!(second.account_id(), "lo", "hi permit 满 → 落 lo");
        let snap = s.status_snapshot();
        let hi = snap.iter().find(|x| x.account_id == "hi").unwrap();
        assert_eq!(hi.rpm_used, 1, "只有真实发出的那 1 次被计,选号失败不扣配额");
    }

    #[tokio::test]
    async fn rpm_unset_means_unlimited_zero_overhead() {
        // 不设 rpm_limit 的号(企业号现状)行为完全不变:连打 20 次都还选它。
        let s = sched(vec![acct("only", 50, Some(0))]);
        for i in 0..20 {
            let id = acquire_and_call(&s, &format!("s{i}")).await;
            assert_eq!(id, "only", "第 {} 次不该被限", i + 1);
        }
        let snap = s.status_snapshot();
        assert_eq!(snap[0].rpm_limit, None, "未设上限");
        assert_eq!(snap[0].rpm_used, 0, "未设上限的号不记账(零开销)");
    }

    #[tokio::test]
    async fn rpm_limit_zero_or_negative_means_unlimited_not_zero_quota() {
        // 0 / 负数视为"不限"而不是"一次都不许"——后者会让一个笔误静默停掉一个号。
        for bad in [0i64, -1] {
            let s = sched(vec![racct("a", 4, 0, bad)]);
            assert_eq!(acquire_and_call(&s, "s").await, "a", "rpm_limit={bad} 应视为不限");
            assert_eq!(s.status_snapshot()[0].rpm_limit, None);
        }
    }

    #[tokio::test]
    async fn rpm_used_is_visible_in_snapshot_for_diagnosis() {
        // 面板必须能看出"号显示正常却不吃流量"的原因,否则无法运维。
        let s = sched(vec![racct("a", 10, 0, 5), acct("b", 10, Some(100))]);
        assert_eq!(acquire_and_call(&s, "s1").await, "a");
        let snap = s.status_snapshot();
        let a = snap.iter().find(|x| x.account_id == "a").unwrap();
        assert_eq!(a.rpm_limit, Some(5));
        assert_eq!(a.rpm_used, 1, "选中一次即计一次");
        assert!(!a.disabled, "RPM 卡住**不等于**禁用:不进 /health 统计");
        assert_eq!(a.reason, "", "不该有 disabled_reason");
    }

    /// 全组都被 RPM 卡住时**不得**报 503:配额会滑出窗口,等一小会儿即可。
    /// 这是与 `paced_until` 同类的软状态,若两边都不认领就会掉进 AllDisabled。
    #[tokio::test]
    async fn rpm_exhausted_whole_group_waits_instead_of_503() {
        let cfg = SchedulerConfig { queue_wait_ms: 300, ..SchedulerConfig::default() };
        // 唯一的号,rpm=1:第二个请求必须等而不是 503。
        let s = AccountScheduler::new(vec![racct("only", 10, 0, 1)], &cfg);
        let first = s.acquire(Some("s1")).await.unwrap();
        drop(first); // 名额已在选号时预留,drop lease 不退还
        let err = match s.acquire(Some("s2")).await {
            Ok(l) => panic!("窗口 60s 远超 300ms 预算,不该拿到号: {}", l.account_id()),
            Err(e) => e,
        };
        // 预算内等不到 → 报可重试错误,但**绝不能**是 NoModelSupport(400,客户端不重试)。
        assert!(
            matches!(err, AcquireError::AllDisabled | AcquireError::AllBusy),
            "应是可重试类错误,实际={err:?}"
        );
    }

    /// RPM 闸门不该把请求钉在高层:高层被 RPM 卡住时,正常降层到兜底号。
    #[tokio::test]
    async fn rpm_exhausted_top_tier_descends_not_holds() {
        let cfg = SchedulerConfig {
            queue_wait_ms: 15_000,
            tier_hold_ms: 300,
            tier_hold_window_ms: 2_000,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(
            vec![qacct_p("hi", 10, 0), acct("lo", 10, Some(100))],
            &cfg,
        );
        // hi 无 rpm 限制,先确认正常走高层。
        let a = s.acquire_in_group(Some("s1"), |_| true, None, true).await.unwrap();
        assert_eq!(a.account_id(), "hi");
        drop(a);

        // 换一组:高层带 rpm=1 且已用满 → 应降层,不该干等 60s。
        let s2 = AccountScheduler::new(
            vec![racct("hi2", 10, 0, 1), acct("lo2", 10, Some(100))],
            &cfg,
        );
        let used = s2.acquire_in_group(Some("s1"), |_| true, None, true).await.unwrap();
        drop(used); // rpm=1 已在选号时用掉
        let t0 = Instant::now();
        let b = s2.acquire_in_group(Some("s2"), |_| true, None, true).await.unwrap();
        assert_eq!(b.account_id(), "lo2", "高层 RPM 满 → 降层兜底");
        assert!(t0.elapsed() < Duration::from_millis(400), "不该等满窗口: {:?}", t0.elapsed());
    }

    /// 最坏情形:整个高优层都被封 1h(2026-08-09 生产上 12/12 就是这样)。
    /// 必须**立刻**降层兜底,绝不能把请求钉住等一小时。
    #[tokio::test]
    async fn queue_tier_hold_descends_immediately_when_whole_top_tier_banned() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 0,
            queue_wait_ms: 15_000, // 与生产同值
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(
            vec![qacct_p("hi-1", 10, 0), qacct_p("hi-2", 10, 0), acct("lo", 10, Some(100))],
            &cfg,
        );
        s.report_failure("hi-1", UpstreamErrorKind::TemporarilyBlocked);
        s.report_failure("hi-2", UpstreamErrorKind::TemporarilyBlocked);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "lo", "高优层全封 → 必须降层,不能钉住等 1h");
        assert!(t0.elapsed() < Duration::from_millis(300), "不该等: {:?}", t0.elapsed());
    }

    /// 冷却比 `queue_wait` 预算长时,等待**总时长受预算收敛**,不会累积超发。
    #[tokio::test]
    async fn queue_tier_hold_total_wait_bounded_by_budget() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 0,
            rate_limit_cooldown_secs: 3, // 3s 冷却 > 600ms 预算
            queue_wait_ms: 600,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![qacct_p("hi", 10, 0), acct("lo", 10, Some(100))], &cfg);
        s.report_failure("hi", UpstreamErrorKind::RateLimited);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "lo", "预算内等不回来 → 降层");
        // 200ms 分片多轮循环,但总时长必须收敛到预算附近(留足 CI 抖动余量)。
        assert!(t0.elapsed() < Duration::from_millis(1_500), "总等待超发: {:?}", t0.elapsed());
    }

    /// **不开排队的高优号不受此闸门管** —— 保持既有语义(见
    /// `tier_hold_ignores_cooled_down_top_tier`):普通高优号冷却即降层。
    #[tokio::test]
    async fn queue_tier_hold_ignores_non_queue_top_tier() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 0,
            rate_limit_cooldown_secs: 1,
            queue_wait_ms: 2_000,
            ..SchedulerConfig::default()
        };
        // hi **不开**排队 → 不在闸门保护范围内。
        let s = AccountScheduler::new(vec![acct("hi", 10, Some(0)), acct("lo", 10, Some(100))], &cfg);
        s.report_failure("hi", UpstreamErrorKind::RateLimited);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "lo", "非排队号冷却 → 立即降层(既有语义不变)");
        assert!(t0.elapsed() < Duration::from_millis(200), "不该等: {:?}", t0.elapsed());
    }

    /// 同层还有排队号能立刻服务时**不等** —— 等的意义只在于"不降层"。
    #[tokio::test]
    async fn queue_tier_hold_does_not_wait_when_same_tier_has_free_account() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 0,
            rate_limit_cooldown_secs: 1,
            queue_wait_ms: 2_000,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(
            vec![qacct_p("hi-1", 10, 0), qacct_p("hi-2", 10, 0), acct("lo", 10, Some(100))],
            &cfg,
        );
        s.report_failure("hi-1", UpstreamErrorKind::RateLimited);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "hi-2", "同层有空闲号 → 直接用它,不等不降层");
        assert!(t0.elapsed() < Duration::from_millis(200), "不该等: {:?}", t0.elapsed());
    }

    /// 高层号**并发满**时不等:窗口过去了照样租不到,等只是把延迟加给客户。
    #[tokio::test]
    async fn tier_hold_does_not_wait_for_saturated_top_tier() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 5_000,
            tier_hold_ms: 400,
            tier_hold_window_ms: 2_000,
            rate_limit_cooldown_secs: 600,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![qacct_p("hi", 1, 0), acct("lo", 10, Some(100))], &cfg);
        let _hog = s.acquire_in_group(Some("hog"), |_| true, None, true).await.unwrap();
        assert_eq!(_hog.account_id(), "hi", "唯一的 permit 先被占住");
        s.report_failure("hi", UpstreamErrorKind::RateLimited);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "lo");
        assert!(t0.elapsed() < Duration::from_millis(80), "满 permit 的号不值得等: {:?}", t0.elapsed());
    }

    /// 被节流的号**就在当前最高层**(没有更高层可等)时不拦截:那条路交给既有的
    /// 「全不可选 → 等 pace 窗口 / 排队」分支,重复拦截会让两套预算叠加。
    #[tokio::test]
    async fn tier_hold_does_not_fire_within_the_same_tier() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 5_000,
            tier_hold_ms: 400,
            tier_hold_window_ms: 2_000,
            rate_limit_cooldown_secs: 600,
            ..SchedulerConfig::default()
        };
        // 同层两个号:a 被节流,b 立刻可用 → 直接用 b,不等。
        let s = AccountScheduler::new(vec![qacct_p("a", 10, 0), qacct_p("b", 10, 0)], &cfg);
        s.report_failure("a", UpstreamErrorKind::RateLimited);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, true).await.unwrap();
        assert_eq!(lease.account_id(), "b", "同层还有号能立刻服务就别等");
        assert!(t0.elapsed() < Duration::from_millis(80), "同层不该触发等待: {:?}", t0.elapsed());
    }

    /// `allow_tier_hold = false`(调用方判定本请求已超出等待窗口)→ 照常降层。
    /// 这是兜底闸,worker 用请求级墙上时钟(`retry_started.elapsed()` vs 窗口)驱动它。
    #[tokio::test]
    async fn tier_hold_respects_caller_opt_out() {
        let s = tier_pair(5_000, 400);
        s.report_failure("hi", UpstreamErrorKind::RateLimited);

        let t0 = Instant::now();
        let lease = s.acquire_in_group(Some("s"), |_| true, None, false).await.unwrap();
        assert_eq!(lease.account_id(), "lo");
        assert!(t0.elapsed() < Duration::from_millis(80), "opt-out 时不该等: {:?}", t0.elapsed());
    }

    /// 成功一次即清零连击并解除节流闸:竞争期间偶发的 429 不该累积到把号打回冷却。
    #[tokio::test]
    async fn success_resets_rate_limit_strikes_and_pace_gate() {
        let cfg = SchedulerConfig {
            rate_limit_pace_ms: 5_000, // 很长:若没被 report_success 解除,下面必然等超时
            rate_limit_pace_max_strikes: 3,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![qacct("a", 2)], &cfg);
        s.report_failure("a", UpstreamErrorKind::RateLimited);
        s.report_failure("a", UpstreamErrorKind::RateLimited);
        s.report_success("a");

        let t0 = Instant::now();
        assert!(s.acquire(Some("s")).await.is_ok(), "成功后节流闸应已解除");
        assert!(t0.elapsed() < Duration::from_millis(300), "不该还在等 5s 节流: {:?}", t0.elapsed());

        // 连击已清零:再撞 3 次仍是节流而非冷却。
        for _ in 0..3 {
            s.report_failure("a", UpstreamErrorKind::RateLimited);
        }
        assert!(!s.status_snapshot()[0].disabled, "连击清零后不该立刻熔断");
    }

    /// 排队开关默认关闭:全禁用时立刻报错,与引入本开关前逐字节等价。
    #[tokio::test]
    async fn queue_wait_off_by_default_fails_fast_on_all_disabled() {
        let s = AccountScheduler::new(vec![acct("a", 2, None)], &SchedulerConfig::default());
        assert_eq!(SchedulerConfig::default().queue_wait_ms, 0, "默认必须是关的");
        s.report_failure("a", UpstreamErrorKind::RateLimited);
        let t0 = Instant::now();
        let err = match s.acquire(Some("s")).await {
            Ok(_) => panic!("本用例期望取号失败"),
            Err(e) => e,
        };
        assert!(matches!(err, AcquireError::AllDisabled), "实际={err:?}");
        assert!(t0.elapsed() < Duration::from_millis(200), "不该等待: {:?}", t0.elapsed());
    }

    /// 开启后:唯一的号在 429 冷却中,acquire 应等到自愈再返回租约,而不是把 503 透给客户。
    #[tokio::test]
    async fn queue_waits_out_rate_limit_cooldown_instead_of_failing() {
        let cfg = SchedulerConfig {
            rate_limit_cooldown_secs: 1,
            queue_wait_ms: 5_000,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![qacct("a", 2)], &cfg);
        s.report_failure("a", UpstreamErrorKind::RateLimited);
        let t0 = Instant::now();
        let lease = s.acquire(Some("s")).await.expect("应等到冷却到期后拿到租约");
        assert_eq!(lease.account_id(), "a");
        assert!(t0.elapsed() >= Duration::from_millis(900), "应真的等过冷却: {:?}", t0.elapsed());
    }

    /// **安全闸**:额度跑干(disabled_until = None)不构成等待理由 —— 池子真干时必须
    /// 立刻失败,否则每个请求都会被挂满整个预算,把容量问题放大成全站卡死。
    #[tokio::test]
    async fn queue_does_not_wait_for_quota_exhausted_accounts() {
        let cfg = SchedulerConfig { queue_wait_ms: 5_000, ..SchedulerConfig::default() };
        let s = AccountScheduler::new(vec![qacct("a", 2)], &cfg);
        s.report_failure("a", UpstreamErrorKind::QuotaExhausted);
        let t0 = Instant::now();
        let err = match s.acquire(Some("s")).await {
            Ok(_) => panic!("本用例期望取号失败"),
            Err(e) => e,
        };
        assert!(matches!(err, AcquireError::AllDisabled), "实际={err:?}");
        assert!(t0.elapsed() < Duration::from_millis(200), "不该为跑干的号等待: {:?}", t0.elapsed());
    }

    /// 冷却到期时刻在预算之外(如 1h 的临时封禁)同样立刻失败,不做无望的等待。
    #[tokio::test]
    async fn queue_does_not_wait_when_cooldown_outlasts_budget() {
        let cfg = SchedulerConfig {
            suspended_cooldown_secs: 3600,
            queue_wait_ms: 300,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![qacct("a", 2)], &cfg);
        s.report_failure("a", UpstreamErrorKind::TemporarilyBlocked);
        let t0 = Instant::now();
        let err = match s.acquire(Some("s")).await {
            Ok(_) => panic!("本用例期望取号失败"),
            Err(e) => e,
        };
        assert!(matches!(err, AcquireError::AllDisabled), "实际={err:?}");
        assert!(t0.elapsed() < Duration::from_millis(200), "1h 封禁不该等: {:?}", t0.elapsed());
    }

    /// 等待有硬预算:冷却比预算长一点点时,超预算即放弃(不会无限等)。
    #[tokio::test]
    async fn queue_gives_up_at_budget() {
        let cfg = SchedulerConfig {
            rate_limit_cooldown_secs: 5,
            queue_wait_ms: 400,
            ..SchedulerConfig::default()
        };
        let s = AccountScheduler::new(vec![qacct("a", 2)], &cfg);
        s.report_failure("a", UpstreamErrorKind::RateLimited);
        let t0 = Instant::now();
        let err = match s.acquire(Some("s")).await {
            Ok(_) => panic!("本用例期望取号失败"),
            Err(e) => e,
        };
        assert!(matches!(err, AcquireError::AllDisabled), "实际={err:?}");
        assert!(t0.elapsed() < Duration::from_secs(3), "不该等满 5s 冷却: {:?}", t0.elapsed());
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
    // ───────── 按分组的成员视图(账号↔分组 N:M) ─────────

    /// 造一张视图:`(account_id, 组内优先级)` 列表。
    fn view(pairs: &[(&str, i64)]) -> GroupView {
        GroupView::new(pairs.iter().map(|(id, p)| ((*id).to_string(), *p)).collect())
    }

    /// **「重构不改变现有分组行为」的机械证明**:同一批号、同一串会话,带 `None` 视图与
    /// 走老路径 `acquire_where` 必须选出**完全相同**的账号序列。
    #[tokio::test]
    async fn view_none_matches_acquire_where_exactly() {
        let mk = || sched(vec![acct("a", 2, Some(0)), acct("b", 2, Some(0)), acct("c", 2, Some(100))]);
        let (old, new) = (mk(), mk());
        let (mut seq_old, mut seq_new) = (Vec::new(), Vec::new());
        for i in 0..12 {
            let sess = format!("s{}", i % 5);
            seq_old.push(old.acquire_where(Some(&sess), |_| true).await.unwrap().account_id().to_string());
            seq_new
                .push(new.acquire_in_group(Some(&sess), |_| true, None, true).await.unwrap().account_id().to_string());
        }
        assert_eq!(seq_old, seq_new, "view=None 必须与老路径逐个选号完全一致");
    }

    /// 非成员对本组**不存在**。反向断言证明那个号本来是首选 —— 否则「没选中它」
    /// 可能只是测试构造得巧,而非成员过滤真的起作用。
    #[tokio::test]
    async fn account_not_in_group_is_invisible() {
        // 主力号 id 字典序靠前且优先级更高:无视图时必被首选。
        let s = sched(vec![acct("a-main", 4, Some(0)), acct("b-backup", 4, Some(100))]);
        let only_backup = view(&[("b-backup", 0)]);
        for i in 0..6 {
            let lease = s
                .acquire_in_group(Some(&format!("s{i}")), |_| true, Some(&only_backup), true)
                .await
                .unwrap();
            assert_eq!(lease.account_id(), "b-backup", "不在成员集里的号绝不能被选中");
        }
        assert_eq!(
            s.acquire(Some("normal")).await.unwrap().account_id(),
            "a-main",
            "反向:无视图的请求本来就该首选主力号"
        );
    }

    /// **本次重构的核心能力**:同一个号在两个组里可以排到不同的层。
    /// 旧模型 priority 挂在账号上,所有组共用一套排序,这条根本表达不了。
    #[tokio::test]
    async fn same_account_ranks_differently_per_group() {
        let s = sched(vec![acct("power", 4, Some(0)), acct("promax", 4, Some(100))]);
        // 正常组:主力优先。低价组:小号优先、主力号当兜底。
        let normal = view(&[("power", 0), ("promax", 100)]);
        let low = view(&[("promax", 0), ("power", 100)]);
        assert_eq!(
            s.acquire_in_group(Some("n1"), |_| true, Some(&normal), true).await.unwrap().account_id(),
            "power"
        );
        assert_eq!(
            s.acquire_in_group(Some("l1"), |_| true, Some(&low), true).await.unwrap().account_id(),
            "promax",
            "同一个号在低价组必须排到主力号前面"
        );
    }

    /// **2026-07-27 事故的反面**:低价组的小号压满/冷却后应当**溢出**到主力号,
    /// 而不是硬报错。旧模型只能硬报错(档位区间把主力号完全挡在外面)。
    #[tokio::test]
    async fn low_tier_group_spills_to_mainstay_when_saturated() {
        let s = sched(vec![acct("power", 4, Some(0)), acct("promax", 4, Some(100))]);
        let low = view(&[("promax", 0), ("power", 100)]);
        // 小号冷却 → 本组仍有兜底层(主力号),必须溢出过去而不是 503。
        s.report_failure("promax", UpstreamErrorKind::RateLimited);
        let lease = s.acquire_in_group(Some("s"), |_| true, Some(&low), true).await.unwrap();
        assert_eq!(lease.account_id(), "power", "小号不可用时必须溢出到组内下一层");

        // 反向:组里**只有**小号时,没有下一层可溢出 → 报可重试的错,绝不越界取非成员。
        let only_low = view(&[("promax", 0)]);
        assert!(
            s.acquire_in_group(Some("s2"), |_| true, Some(&only_low), true).await.is_err(),
            "组里没有别的成员时不得偷偷用非成员的号"
        );
    }

    /// 组里一个成员都没有(或成员都不支持该模型)→ `GroupEmpty`(503,运维加条边就好),
    /// **绝不能**是 `NoModelSupport`(400,客户端不重试,且提示"换模型"完全误导)。
    #[tokio::test]
    async fn group_empty_is_never_no_model_support() {
        let s = sched(vec![acct("a", 4, Some(0))]);
        let empty = view(&[]);
        let e = s.acquire_in_group(Some("s"), |_| true, Some(&empty), true).await.err();
        assert_eq!(e, Some(AcquireError::GroupEmpty));
        assert_ne!(e, Some(AcquireError::NoModelSupport), "组没配号不是订阅能力问题");

        // 反向:真的没号支持该模型时,语义不能被视图改写。
        let s2 = sched(vec![acct_sub("a-free", 4, "KIRO FREE")]);
        assert_eq!(
            s2.acquire_in_group(Some("s"), opus_pred, None, true).await.err(),
            Some(AcquireError::NoModelSupport),
            "全 FREE 请求 opus 仍应是 NoModelSupport"
        );
    }

    /// 自愈必须**按视图收窄**:低价组一次全灭,不得把别的组刚合法禁用的号一起复活
    /// 并清零失败计数 —— 那会让连续失败保护对所有组一起失效。
    #[tokio::test]
    async fn heal_is_scoped_to_group_members() {
        let s = sched(vec![acct("mine", 4, Some(0)), acct("theirs", 4, Some(0))]);
        // 两个号都因连续失败被禁用。
        for _ in 0..10 {
            s.report_failure("mine", UpstreamErrorKind::ServerError);
            s.report_failure("theirs", UpstreamErrorKind::ServerError);
        }
        // 前提用快照断言:**不能**用 acquire(None) —— 那本身就是全量视图,
        // 会当场触发自愈把两个号都救活,前提就被自己的探测破坏了。
        let disabled = |s: &AccountScheduler, id: &str| {
            s.status_snapshot().iter().find(|a| a.account_id == id).map(|a| a.disabled)
        };
        assert_eq!((disabled(&s, "mine"), disabled(&s, "theirs")), (Some(true), Some(true)));

        // 只含 mine 的组发起请求 → 自愈只该复活 mine。
        let only_mine = view(&[("mine", 0)]);
        let lease = s.acquire_in_group(Some("s"), |_| true, Some(&only_mine), true).await.unwrap();
        assert_eq!(lease.account_id(), "mine", "本组成员应被自愈复活");
        drop(lease);
        assert_eq!(disabled(&s, "theirs"), Some(true), "别的组的号绝不能被顺手复活");

        // 此刻 mine 已healthy → **全池不再告罄** → 自愈闸门关闭,theirs 组拿不到自愈。
        // 这是刻意的:自愈是"等价重启"的最后手段,只能由全池告罄触发。否则一个只含单个
        // 坏号的小组每个请求都能触发一次自愈,该号被反复复活,连续失败保护对它彻底失效
        // (对抗审查 Skeptic#4)。
        let only_theirs = view(&[("theirs", 0)]);
        assert_eq!(
            s.acquire_in_group(Some("s2"), |_| true, Some(&only_theirs), true).await.err(),
            Some(AcquireError::AllDisabled),
            "池里还有健康号时,小组不得靠自愈把自己的坏号捞回来"
        );
    }

    /// 自愈闸门:**全池告罄**才允许自愈。小组视图为空但池里仍有健康号时,绝不触发 ——
    /// 否则连续失败保护会被"每请求自愈一次"架空。
    #[tokio::test]
    async fn heal_requires_whole_pool_exhausted() {
        let s = sched(vec![acct("bad", 4, Some(0)), acct("good", 4, Some(0))]);
        for _ in 0..10 {
            s.report_failure("bad", UpstreamErrorKind::ServerError);
        }
        let only_bad = view(&[("bad", 0)]);
        assert_eq!(
            s.acquire_in_group(Some("s"), |_| true, Some(&only_bad), true).await.err(),
            Some(AcquireError::AllDisabled),
            "池里 good 还健康,只含坏号的小组不得触发自愈"
        );
        // 反向:池子真的全灭时,自愈照常兜底(保护不是永久锁死)。
        for _ in 0..10 {
            s.report_failure("good", UpstreamErrorKind::ServerError);
        }
        assert!(
            s.acquire_in_group(Some("s2"), |_| true, Some(&only_bad), true).await.is_ok(),
            "全池告罄时自愈必须仍然生效"
        );
    }

    /// **已知且刻意保留的耦合**(对抗审查 Skeptic#5):`last_selected_at` 挂在账号上、
    /// 全 worker 共享,所以两个组若共享同一层里的号,一个组选走它会改变另一个组在**同层内**
    /// 的轮转顺序。
    ///
    /// 保留而不修的理由:分层(哪一层可见、先用哪层)才是隔离语义,已由成员边严格保证;
    /// 而层内 LRU 只影响"同层里先用谁",不影响可见性、不影响溢出顺序、不会让流量落到
    /// 非成员上。要做到层内 LRU 也按组独立,得给每个组维护一份 last_selected_at ——
    /// 那会让"同一个号被多个组用"的负载统计彼此看不见,反而更容易把号打爆。
    #[tokio::test]
    async fn shared_account_lru_is_coupled_across_groups_by_design() {
        let s = sched(vec![acct("a", 4, Some(0)), acct("b", 4, Some(0))]);
        let big = view(&[("a", 0), ("b", 0)]);
        let small = view(&[("a", 0)]);
        // 先让 small 组用掉 a(平局时 tiered_lru 按 id 选,首选就是 a)。
        let first = s.acquire_in_group(None, |_| true, Some(&small), true).await.unwrap();
        assert_eq!(first.account_id(), "a");
        drop(first);
        // big 组的首个请求因此改选 b —— 这是共享 LRU 的直接后果,不是 bug。
        let next = s.acquire_in_group(None, |_| true, Some(&big), true).await.unwrap();
        assert_eq!(next.account_id(), "b", "同层共享号的 LRU 是跨组耦合的(已知取舍)");
    }

    /// 隔离语义的真正保证:低价组的流量**不会让正常组选到非成员**,也不会改变正常组
    /// 的分层顺序(哪一层先用)。层内先用谁的耦合见上一条测试。
    #[tokio::test]
    async fn group_selection_keeps_lru_cursor_stable() {
        let mk = || sched(vec![acct("a", 4, Some(0)), acct("b", 4, Some(0)), acct("c", 4, Some(100))]);
        let (base, mixed) = (mk(), mk());
        let low = view(&[("c", 0)]);
        let mut seq_base = Vec::new();
        let mut seq_mixed = Vec::new();
        for i in 0..8 {
            let sess = format!("n{i}");
            seq_base.push(base.acquire(Some(&sess)).await.unwrap().account_id().to_string());
            // 每轮先让低价组打一次(只会落到 c),再让正常请求打一次。
            let _ = mixed.acquire_in_group(Some(&format!("l{i}")), |_| true, Some(&low), true).await.unwrap();
            seq_mixed.push(mixed.acquire(Some(&sess)).await.unwrap().account_id().to_string());
        }
        assert_eq!(seq_base, seq_mixed, "低价组的流量不得改变正常请求的轮转顺序");
    }

    /// `GroupEmpty` 的文案必须能与 `NoModelSupport` 区分(运维看日志要能分辨)。
    #[test]
    fn acquire_error_display_distinguishes_group_empty() {
        let t = AcquireError::GroupEmpty.to_string();
        assert!(!t.is_empty());
        assert_ne!(t, AcquireError::NoModelSupport.to_string());
    }

    // ───── upstream_cut 软冷却(draining)─────

    /// 以**当前代际**在窗口内连报 `n` 次 cut(默认口径:600s 窗 / 阈值 2 / 1500s 冷却)。
    fn report_cuts(s: &AccountScheduler, id: &str, n: u32) {
        let epoch = s.current_cut_epoch(id);
        let now = Instant::now();
        for _ in 0..n {
            s.report_upstream_cut_at(id, epoch, now, Duration::from_secs(600), 2, Duration::from_secs(1500));
        }
    }

    #[test]
    fn upstream_cut_below_threshold_not_draining() {
        let s = sched(vec![acct("a", 1, Some(100))]);
        report_cuts(&s, "a", 1);
        assert!(!s.is_draining("a"), "未达阈值不进软冷却");
    }

    #[test]
    fn upstream_cut_threshold_triggers_drain() {
        let s = sched(vec![acct("a", 1, Some(100))]);
        report_cuts(&s, "a", 2);
        assert!(s.is_draining("a"), "窗口内达阈值应进软冷却");
    }

    #[test]
    fn upstream_cut_window_expiry_does_not_accumulate() {
        let s = sched(vec![acct("a", 1, Some(100))]);
        let epoch = s.current_cut_epoch("a");
        let t0 = Instant::now();
        let window = Duration::from_secs(600);
        s.report_upstream_cut_at("a", epoch, t0, window, 2, Duration::from_secs(1500));
        // 第二次已在窗口外:第一次被剪掉,窗口内只剩 1 次 → 不触发。
        s.report_upstream_cut_at(
            "a",
            epoch,
            t0 + window + Duration::from_secs(1),
            window,
            2,
            Duration::from_secs(1500),
        );
        assert!(!s.is_draining("a"), "窗口外的 cut 不得累计");
    }

    #[test]
    fn upstream_cut_stale_epoch_is_dropped() {
        let s = sched(vec![acct("a", 1, Some(100))]);
        let epoch = s.current_cut_epoch("a");
        let now = Instant::now();
        // 代际不符:报多少次都丢弃。
        for _ in 0..3 {
            s.report_upstream_cut_at("a", epoch + 1000, now, Duration::from_secs(600), 2, Duration::from_secs(1500));
        }
        assert!(!s.is_draining("a"), "代际不符的上报必须丢弃");
        // reset 换发新代际并清空冷却态;旧代际的迟到上报继续被丢弃。
        assert!(s.reset_account("a"));
        let new_epoch = s.current_cut_epoch("a");
        assert_ne!(epoch, new_epoch, "reset 必须换发软冷却代际");
        for _ in 0..2 {
            s.report_upstream_cut_at("a", epoch, now, Duration::from_secs(600), 2, Duration::from_secs(1500));
        }
        assert!(!s.is_draining("a"), "reset 后旧代际上报不得生效");
        // 新代际正常累计。
        report_cuts(&s, "a", 2);
        assert!(s.is_draining("a"));
    }

    #[tokio::test]
    async fn draining_new_session_prefers_normal_account() {
        let s = sched(vec![acct("a", 1, Some(100)), acct("b", 1, Some(100))]);
        report_cuts(&s, "a", 2);
        let lease = s.acquire(None).await.unwrap();
        assert_eq!(lease.account_id(), "b", "无亲和新会话优先 normal 号");
    }

    #[tokio::test]
    async fn draining_fail_open_when_normal_empty() {
        let s = sched(vec![acct("a", 1, Some(100))]);
        report_cuts(&s, "a", 2);
        let lease = s.acquire(None).await.unwrap();
        assert_eq!(lease.account_id(), "a", "normal 全空 fail-open:draining 仍服务,不全池 503");
    }

    #[tokio::test]
    async fn draining_keeps_existing_affinity_sticky() {
        let s = sched(vec![acct("a", 1, Some(100)), acct("b", 1, Some(100))]);
        // 先把会话钉到 a(同层 LRU 平局按 id 序,"a" < "b")。
        let l1 = s.acquire(Some("s")).await.unwrap();
        assert_eq!(l1.account_id(), "a");
        drop(l1);
        report_cuts(&s, "a", 2);
        let l2 = s.acquire(Some("s")).await.unwrap();
        assert_eq!(l2.account_id(), "a", "已有亲和:primary draining 也保持粘着");
    }

    #[tokio::test]
    async fn draining_does_not_bypass_disabled_gate() {
        // a 配置禁用 + draining:fail-open 也不得复活禁用号(draining 不绕过任何既有闸门)。
        let s = sched(vec![acct_disabled("a", true), acct("b", 1, Some(100))]);
        report_cuts(&s, "a", 2);
        let lease = s.acquire(None).await.unwrap();
        assert_eq!(lease.account_id(), "b");
    }

    #[tokio::test]
    async fn draining_still_respects_rpm_gate() {
        // 两个号都 draining;a 的 RPM 打满后,fail-open 也不得再选它。
        let s = sched(vec![racct("a", 2, 100, 1), acct("b", 2, Some(100))]);
        report_cuts(&s, "a", 2);
        report_cuts(&s, "b", 2);
        // 全部 draining → fail-open;同层 LRU 平局按 id → a(消耗 a 唯一一格 RPM)。
        let l1 = s.acquire(None).await.unwrap();
        assert_eq!(l1.account_id(), "a");
        drop(l1);
        let l2 = s.acquire(None).await.unwrap();
        assert_eq!(l2.account_id(), "b", "RPM 打满的 draining 号不得被 fail-open 选中");
    }
}
