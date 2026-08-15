//! worker 角色 —— 实际反代。
//!
//! 绑定固定出口(egress)+ 管理一组账号(account_group)。
//! 暴露 `/v1/messages`(和对外一样,绑 localhost 高位端口)+ `/health`。
//! 选号走组内 v52 会话亲和调度(见 [`scheduler`]):同会话钉同账号,最大化 Kiro
//! prefix cache 命中(缓存按上游账号隔离)。

mod scheduler;

use std::path::Path;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use gw_core::account::Account;
use gw_core::config::{AccountsConfig, InstancesConfig, SystemConfig};
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::openai::{Wire, WireFrame};
use gw_core::provider::{
    AccountQuota, CallCtx, ChatRequest, ChatUsage, Provider, SseEvent, StreamItem,
};
use gw_core::store::{RequestLog, UsageRecord, UsageSink};
use gw_store::SqliteStore;

use crate::egress;
use crate::registry::Registry;
use scheduler::AccountScheduler;

/// 设置热调链路的**可观测状态**:worker 此刻真正在用的值 + 最近一次同步的结果。
///
/// ## 为什么需要它
///
/// 热调链路有两处**静默**失败:overlay 解析失败会每 30s 跳过一轮并**永久**保持旧配置
/// (只留一行 error 日志);本版本不认识的字段会被无声忽略(镜像比写库的那个旧时发生)。
/// 两者在面板上与「保存成功」完全无法区分,于是「我改了没生效」变成一个查不下去的问题 ——
/// 只能靠翻业务数据反推(如观察计费命中率有没有跳变),而那要等几分钟且需要有流量。
///
/// 把 worker **实际生效的值**回显出来,「没生效」与「生效了但我看早了」才分得开。
#[derive(Clone, Default)]
struct SettingsSync {
    /// 最近一次**成功应用**的 unix 秒。0 = 启动后一次都没成功过。
    applied_at: i64,
    /// 最近一次同步的错误(空 = 正常)。非空时 [`Self::applied_at`] 停在上次成功的时刻,
    /// 「现在 − applied_at」就是配置已经僵住多久 —— 这正是静默失效唯一的外部特征。
    error: String,
    /// 本版本不认识、已被忽略的 overlay 字段。非空 = 本进程镜像比写库的那个旧,
    /// 表现为「别的设置都生效,就这一个不生效」。
    unknown: Vec<String>,
    /// worker **应用之后**在用的关键热调值(不是 DB 里存的那份)。
    effective: serde_json::Value,
    /// 本 worker 的 provider 是否真的热应用 **provider 级**设置(缓存计费/图像/实验开关)。
    /// `false` 时 [`Self::effective`] 里那半边是「算得出但没生效」——必须让面板知道,
    /// 否则它会对着一份从未应用的值报「一致」。scheduler 那半边不受影响,一直是热的。
    provider_hot: bool,
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// worker 此刻在用的**全部**热调值,键名与 `GET /settings` 的应然值逐字对齐。
///
/// 走 `SystemSettings::from_effective` 而不是手写字段清单,有三个理由:
///
/// 1. **手写清单会制造新的盲区**。漏掉的字段在面板上会得出「一致」这个错误结论,
///    而漏掉的恰恰是没人想到的那个(对抗审查三个视角都点了这条)。
/// 2. **键名天然对齐**。应然值就是同一个类型序列化出来的,前端逐键比对不需要映射表。
/// 3. **`default_proxy` 传 `None`**:代理 URL 里可能带 `user:pass@`,而这份数据要经
///    admin 上到浏览器。应然值那边有 `redact_proxy_url` 掩码,这边最省事也最稳的做法
///    是**根本不回显它** —— 少一个字段,少一条泄漏路径。
fn effective_view(eff: &gw_core::config::SystemConfig) -> serde_json::Value {
    serde_json::to_value(gw_core::config::SystemSettings::from_effective(eff, None))
        .unwrap_or(serde_json::Value::Null)
}

/// 掉进面板的错误串要先脱敏:serde 的类型错误会把出错的**值**嵌进消息里,
/// 而那个值可能是 `socks5://user:pass@host`(对抗审查 Architect#6)。
fn redact_err(msg: &str) -> String {
    crate::admin::redact_proxy_url(msg)
}

struct WorkerState {
    instance: u32,
    egress_desc: String,
    group: String,
    provider: Arc<dyn Provider>,
    /// 设置热调的可观测状态,见 [`SettingsSync`]。`/health` 直接回显。
    settings_sync: parking_lot::RwLock<SettingsSync>,
    /// 组内账号 v52 会话亲和调度器(选号 + 并发 + 冷却/禁用生命周期 + 凭证真值)。
    scheduler: AccountScheduler,
    /// per-account 刷新单飞锁:同一账号同时只允许一个 in-flight refresh(契约 H4)。
    /// 避免两个首请求并发刷新、互相覆盖 rolling refresh_token 导致一方 invalid_grant。
    refresh_locks: parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// usage 落库汇(#130)。打开失败时为 None(降级:usage 仅记日志不入库)。
    usage_sink: Option<Arc<dyn UsageSink>>,
    /// 在途异步 usage 落库登记(停机排空时等它们收尾)。
    pending_writes: Arc<PendingWrites>,
    /// 控制面库(账号事实源):刷新后回写 rolling refresh_token、30s 周期 sync 账号集。
    /// None = 库打开失败(降级:账号只来自 yaml 启动快照,改动需重启)。
    store: Option<Arc<SqliteStore>>,
    /// 账号配额缓存(account_id → (配额或 None, 上次**尝试**时刻)),TTL [`QUOTA_TTL`]。
    /// 关键:成功与失败都写入时刻 → 失败也受 TTL 节流(否则每次 /health 都重打上游,
    /// 审查 Skeptic#5)。value 的 `Option` 为 None 表示"查过但失败/无配额"。
    quota_cache: parking_lot::Mutex<
        std::collections::HashMap<String, (Option<AccountQuota>, std::time::Instant)>,
    >,
    /// 配额刷新在途去重:同账号同时只一个 getUsageLimits 在跑。
    quota_inflight: parking_lot::Mutex<std::collections::HashSet<String>>,
    /// 配额刷新并发上限信号量:防 /health 一次性给上百个账号同时刷新造成 stampede
    /// (审查 Architect#4)。后台任务先抢 permit 再打上游。
    quota_sem: Arc<tokio::sync::Semaphore>,
    /// 账号集同步串行锁(审查 Skeptic#1/Architect#1):30s 周期循环与 `/sync` 端点
    /// 并发跑 `sync_accounts_from_db` 时,若不串行"读 DB 快照 → 应用 scheduler",
    /// 旧快照可能最后应用,把刚导入的账号从内存移掉(等下轮才回来)。
    sync_lock: tokio::sync::Mutex<()>,
    /// **分组成员视图快照**:组名 → 该组在本 worker 名下的成员集与组内优先级。
    /// 与账号集同一轮同步(同一把 `sync_lock`),admin 改完可用 `/sync` 立即推送。
    ///
    /// 查不到的组名一律当**空视图**处理(→ 503),**绝不回落成全量池** —— 那等于
    /// 把一个受限分组的请求以全量权限放行,是静默提权。
    group_views: parking_lot::RwLock<std::collections::HashMap<String, scheduler::GroupView>>,
    /// worker 的 egress client(provider 已持有同一个;此处保留供诊断)。
    _client: reqwest::Client,
}

/// 配额缓存 TTL:被查看时每账号 ≤1 次/分钟打上游(只读 getUsageLimits,安全)。
const QUOTA_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// 配额刷新最大并发(stampede 护栏):上百账号同时被查看也只 N 个在打上游。
const QUOTA_MAX_CONCURRENCY: usize = 3;

/// 后台配额轮询一轮间隔下/上限(秒)。🟢 对齐 static_flow(240–300s + 抖动)。
/// worker 自身每轮兜底刷新一遍账号配额,不依赖 /health 被打。
/// ⚠️ 与 [`QUOTA_TTL`](=60s)耦合:轮询用 `QUOTA_POLL_MIN_SECS` 作 stale floor,必须 **>
/// `QUOTA_TTL`**,否则会和 /health 路径重复打点(被 /health 在 floor 内刷过的账号本轮才会跳过)。
/// 调整任一常量时检查二者关系(对抗审查 Architect#7)。
const QUOTA_POLL_MIN_SECS: u64 = 240;
const QUOTA_POLL_MAX_SECS: u64 = 300;
/// 启动后首轮配额 sweep 的短延迟:让启动握手/warm 先跑,但远早于 240s,避免"重启后无人看
/// dashboard 时前几分钟配额全空"的冷启动窗口(对抗审查 Skeptic#5)。
const QUOTA_POLL_WARMUP_SECS: u64 = 20;

/// 在 `[min, max]` 内取一个秒数。用 `SystemTime` 亚秒纳秒做熵,免引入 `rand` 依赖——
/// 抖动只为打散轮询时刻、避免精确固定间隔成为可识别指纹(防封),不需密码学强度随机。
fn jittered_secs(min: u64, max: u64) -> u64 {
    if min >= max {
        return min;
    }
    let span = max - min + 1;
    let entropy = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    min + (entropy % span)
}

impl WorkerState {
    /// 取该账号的 per-account 刷新锁(单飞:同账号同时只一个刷新;
    /// flush_dirty_extras / 配额回填同表取锁,互斥持久化)。
    fn refresh_lock(&self, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        lock_for(&self.refresh_locks, account_id)
    }

    /// 确保账号持有**未过期**的 access_token。
    ///
    /// 流程(对齐 kiro.rs try_ensure_token 双检锁 + expires_at 检查):
    /// 1. 有非空 access_token 且未过期/未临近过期 → 直接用(快路径,无锁);
    /// 2. 否则取 per-account 单飞锁,锁内**二次检查**(其他请求可能刚刷新好,从 scheduler
    ///    读最新),仍需刷新才真正 refresh_auth;
    /// 3. 刷新成功 → 回写 scheduler(rolling refresh_token 进选号池),返回新账号;
    /// 4. 刷新失败 → 原样返回 [`UpstreamError`](保留 kind:invalid_grant=TokenInvalid 永久,
    ///    网络/5xx/429=对应 transient,由调用方据 kind 决定禁用 vs 重试)。
    async fn ensure_credentialed(
        &self,
        account: Arc<Account>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        if has_fresh_token(&account) {
            return Ok(account);
        }
        self.refresh_locked(account).await
    }

    /// 强制刷新该账号一次:**无条件**走单飞锁 + 上游 refresh_auth,忽略 token 新鲜度。
    ///
    /// 人工“刷新 token”按钮用——操作者要真正向上游换一次,以验证 refresh_token 可用 /
    /// 轮换 rt 后立即拿到新 access_token。锁内取 scheduler 最新副本作基底(拿当前 rolling
    /// refresh_token、不抹并发 merge 字段),不因 fresh 提前返回。
    async fn force_refresh(
        &self,
        account: Arc<Account>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        let lock = self.refresh_lock(&account.account_id);
        let _guard = lock.lock().await;
        let base = self
            .scheduler
            .account(&account.account_id)
            .unwrap_or(account);
        self.do_refresh_and_persist(base).await
    }

    /// chat 收 403 TokenInvalid 后的同号刷新:**仅当** scheduler 现存 access_token 仍是
    /// 这枚被拒 token(并发请求尚未替我刷过)才真打上游;否则直接返回最新副本。
    ///
    /// 判据是 **token 是否被换掉**,而非 expires_at 新鲜度——被拒 token 常仍“看似新”
    /// (吊销/时钟漂移),旧 `refresh_locked` 的 fresh 早返回会把它当好 token 返回、retry
    /// 必再失败。改用 CAS 后:同账号 N 个并发 403,第一个刷新换掉 token,其余进锁后发现
    /// token 已变 → 直接用新的、不再各自重刷,避免放大 token 交换 / rolling refresh_token
    /// (审查 Skeptic#2 / Architect#3)。
    async fn refresh_after_rejection(
        &self,
        account: Arc<Account>,
        rejected_token: Option<&str>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        let lock = self.refresh_lock(&account.account_id);
        let _guard = lock.lock().await;
        let base = match self.scheduler.account(&account.account_id) {
            Some(fresh) => {
                // 他人已把 token 换成别的(不再是被拒的那枚)→ 用新的,别再刷。
                if fresh.extra_str("access_token") != rejected_token {
                    return Ok(fresh);
                }
                fresh
            }
            None => account,
        };
        self.do_refresh_and_persist(base).await
    }

    /// 单飞锁内刷新:锁内二次检查(他人可能刚刷好)→ 仍需则 refresh_auth → 回写 scheduler。
    async fn refresh_locked(
        &self,
        account: Arc<Account>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        let lock = self.refresh_lock(&account.account_id);
        let _guard = lock.lock().await;
        // 锁内取**最新**副本作为刷新基底:他人可能刚刷好(直接用);即便仍需刷新,
        // 基底也必须是 scheduler 真值——用调用方旧快照会 (a) 拿已作废的 rolling
        // refresh_token 去刷新,(b) 整块回写时抹掉期间 merge_extra 进来的字段
        // (如配额回填的 subscription_title)(审查②R Architect#2/Minimalist#2)。
        let base = match self.scheduler.account(&account.account_id) {
            Some(fresh) => {
                if has_fresh_token(&fresh) {
                    return Ok(fresh);
                }
                fresh
            }
            None => account,
        };
        self.do_refresh_and_persist(base).await
    }

    /// 刷新 + 回写 + 持久化的共享尾段(`refresh_locked` / `force_refresh` 复用)。
    ///
    /// 调用方必须**已持有**该账号的 per-account 刷新锁,并已选定 `base`(scheduler 最新副本)。
    async fn do_refresh_and_persist(
        &self,
        base: Arc<Account>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        let refreshed = Arc::new(self.provider.refresh_auth(&base).await?);
        // 回写 scheduler:带新 token 的副本进入选号池(单一事实来源)。
        // **原子**「替换 + 置脏」(同一把 entries 锁):分两步的话,30s sync 会在
        // 中间窗口看到 dirty=false,用 DB 旧值洗掉新 token(审查②R Skeptic#1)。
        self.scheduler.update_account_dirty(refreshed.clone());
        // 持久化(rolling refresh_token 不落库,重启即回退已作废旧 token):
        // - **增量合并**只写本次刷新改动的字段(相对 base),不整块替换——并发的
        //   admin 修改(priority/region 等)不被旧内存快照抹掉(审查 Architect#4);
        // - 置脏后持久化、成功才清位:失败窗口内 sync 不会用 DB 旧值洗内存,
        //   由 sync 循环负责重试(审查 Minimalist#1)。
        match &self.store {
            Some(store) => {
                let delta: std::collections::BTreeMap<&String, &serde_json::Value> = refreshed
                    .extra
                    .iter()
                    .filter(|(k, v)| base.extra.get(*k) != Some(*v))
                    .collect();
                let persisted = serde_json::to_string(&delta)
                    .map_err(anyhow::Error::from)
                    .and_then(|j| store.merge_account_extra(&refreshed.account_id, &j));
                match persisted {
                    Ok(_) => self.scheduler.clear_extra_dirty(&refreshed.account_id),
                    Err(e) => tracing::warn!(account = %refreshed.account_id,
                        "刷新回写 DB 失败,已置脏待 sync 重试: {e}"),
                }
            }
            // 无库(降级模式):没有 sync 循环、也没人会清脏,直接清掉避免悬挂标记。
            None => self.scheduler.clear_extra_dirty(&refreshed.account_id),
        }
        Ok(refreshed)
    }

    /// 读账号配额缓存;命中(<TTL,含失败记录)直接返回,陈旧/缺失则触发**后台**刷新
    /// (去重)并返回当前缓存值(可能旧/None)。**不阻塞 /health**:只读查询在后台跑。
    fn quota_cached_or_refresh(self: &Arc<Self>, account_id: &str) -> Option<AccountQuota> {
        let now = std::time::Instant::now();
        let cached = self.quota_cache.lock().get(account_id).cloned();
        // fresh 看"上次尝试时刻"(成功或失败都算),失败也受 TTL 节流。
        // 例外:provider 的配额是本地廉价读(quota_is_local,如 dario 从流量捕获的内存快照)
        // 时**永不视为 fresh** → 每次都后台刷新,使刚捕获的快照下一轮(~一个前端轮询)就上面板,
        // 不被陈旧 None/旧值挡满一个 TTL(对抗审查 #1)。本地刷新无昂贵上游往返,节流无意义。
        let fresh = !self.provider.quota_is_local()
            && cached
                .as_ref()
                .is_some_and(|(_, at)| now.duration_since(*at) < QUOTA_TTL);
        if !fresh {
            self.spawn_quota_refresh(account_id.to_string());
        }
        cached.and_then(|(q, _)| q)
    }

    /// 后台刷新某账号配额(去重:同账号同时只一个在途)。无 tokio 上下文则跳过。
    fn spawn_quota_refresh(self: &Arc<Self>, account_id: String) {
        if !self.quota_inflight.lock().insert(account_id.clone()) {
            return; // 已有刷新在途。
        }
        if tokio::runtime::Handle::try_current().is_err() {
            self.quota_inflight.lock().remove(&account_id);
            return;
        }
        let st = self.clone();
        tokio::spawn(async move {
            st.refresh_quota_once(&account_id).await;
            st.quota_inflight.lock().remove(&account_id);
        });
    }

    /// 实际查一次配额并写缓存:抢并发 permit → 确保 token 有效(可能刷新,安全)→
    /// provider.account_quota。**无论成功失败都写"尝试时刻"**(失败写 None),让 TTL 同样
    /// 节流失败重试。只读 getUsageLimits + 刷新,绝不发 chat(见 no-chat-test-on-real-accounts)。
    async fn refresh_quota_once(self: &Arc<Self>, account_id: &str) {
        let _permit = match self.quota_sem.clone().acquire_owned().await {
            Ok(p) => p,
            Err(_) => return, // 信号量被关闭(停机),放弃。
        };
        let result = self.try_fetch_quota(account_id).await;
        // 顺手回填订阅档位:getUsageLimits 的 subscriptionTitle 是模型能力过滤
        // (FREE 不给 opus)的数据源——只导了 rt 的账号缺 subscription_title,
        // 首次配额查询后在此收敛。
        if let Ok(Some(q)) = &result {
            if let Some(title) = &q.currency {
                self.backfill_subscription_title(account_id, title).await;
            }
        }
        let value = match result {
            Ok(q) => q, // Some(配额) 或 None(账号已不在/无配额)
            Err(e) => {
                tracing::debug!(account = %account_id, "配额查询失败(节流后重试): {e}");
                None
            }
        };
        // 写时刻(成功 Some / 失败或无 None),fresh 判定据此节流。
        self.quota_cache
            .lock()
            .insert(account_id.to_string(), (value, std::time::Instant::now()));
    }

    /// 把配额响应里的订阅档位写回账号 extra(内存就地合并 + 持久化 DB)。
    ///
    /// - 内存:`merge_extra` 在调度器锁内单字段合并,不携带旧账号快照,与并发
    ///   token 刷新互不覆盖;值未变(60s 周期刷新的常态)直接跳过。
    /// - 持久化:取 per-account 刷新锁与 token 刷新的「置脏→落库→清脏」互斥,
    ///   避免本函数 clear 误清掉刷新失败留下的脏标记(丢 rolling token 重试)。
    async fn backfill_subscription_title(&self, account_id: &str, title: &str) {
        self.persist_extra_field(
            account_id,
            "subscription_title",
            serde_json::Value::String(title.to_string()),
            "订阅档位(模型过滤数据源)",
        )
        .await;
    }

    /// 把一个**发现/查询得来的持久字段**写回账号 extra(内存就地合并 + DB 持久化)。
    /// subscription_title(配额回填)、profile_arn(ListAvailableProfiles 发现)共用。
    ///
    /// 必须先拿 per-account 刷新锁再动内存:token 刷新在锁内「读基底→整块替换」,
    /// 锁外 merge 会落进它的读写窗口被整块覆盖(审查②R Minimalist#2)。
    async fn persist_extra_field(
        &self,
        account_id: &str,
        key: &str,
        value: serde_json::Value,
        what: &str,
    ) {
        let lock = self.refresh_lock(account_id);
        let _guard = lock.lock().await;
        if !self.scheduler.merge_extra(account_id, key, value.clone()) {
            return; // 值未变,无事可做。
        }
        let Some(store) = &self.store else { return };
        // 账号已有待重试的整块脏 extra(刷新回写失败):不抢着写,30s sync 的
        // flush_dirty_extras 会连本字段一起落库。
        if self.scheduler.is_extra_dirty(account_id) {
            return;
        }
        self.scheduler.mark_extra_dirty(account_id);
        let delta = serde_json::json!({ key: value }).to_string();
        match store.merge_account_extra(account_id, &delta) {
            Ok(_) => self.scheduler.clear_extra_dirty(account_id),
            Err(e) => tracing::warn!(account = %account_id,
                "{what} 回写 DB 失败,置脏待 sync 重试: {e}"),
        }
        tracing::debug!(account = %account_id, key, "已回填 {what}");
    }

    /// 确保企业/IdC 账号带 profileArn:缺失则 `ListAvailableProfiles` 发现并持久化,
    /// 返回带 profileArn 的更新副本(chat / 配额本次即用)。发现失败不阻断——让后续
    /// 上游 400「profileArn is required」自然报错(BadRequest,不惩罚账号)。
    /// social/builderid(固定兜底)与已有显式值的账号直接短路返回,无网络调用。
    async fn ensure_profile_arn(&self, account: Arc<Account>) -> Arc<Account> {
        // API Key 凭据不需要 profileArn(TokenType: API_KEY 让服务端按 key 自身账号解析),
        // 短路免掉一次 discover 调用。
        if gw_kiro::machine_id::is_api_key_credential(&account) {
            return account;
        }
        if account
            .extra_str("profile_arn")
            .is_some_and(|s| !s.trim().is_empty())
        {
            return account; // 已有显式值(含上次发现后持久化的)。
        }
        match self.provider.discover_profile_arn(&account).await {
            Ok(Some(arn)) => {
                self.persist_extra_field(
                    &account.account_id,
                    "profile_arn",
                    serde_json::Value::String(arn.clone()),
                    "profileArn(ListAvailableProfiles 发现)",
                )
                .await;
                tracing::info!(account = %account.account_id, "已发现并持久化 profileArn");
                self.correct_region_from_arn(&account, &arn).await;
                // 取回带新 profile_arn 的副本供本次请求使用。
                self.scheduler.account(&account.account_id).unwrap_or(account)
            }
            Ok(None) => account, // 固定兜底或无可用 profile,按原样(headers 兜底处理)。
            Err(e) => {
                tracing::debug!(account = %account.account_id,
                    "ListAvailableProfiles 发现失败(不阻断): {e}");
                account
            }
        }
    }

    /// 预热订阅档位:对缺 `subscription_title` 的账号后台拉一次配额(getUsageLimits
    /// 只读,安全)。它是模型能力过滤的数据源——不预热的话,只导了 rt 的 FREE 号在
    /// 冷启动期间会被「未知放行」接 opus 然后 403(审查②R Architect#3)。
    /// 受 quota_sem(并发 3)+ TTL 节流,账号多也不会打爆;完整导入(KiroManager)
    /// 的账号自带该字段,不产生任何调用。
    fn warm_subscription_titles(self: &Arc<Self>) {
        for snap in self.scheduler.status_snapshot() {
            let Some(acc) = self.scheduler.account(&snap.account_id) else { continue };
            if acc.extra_str("subscription_title").is_none() {
                self.spawn_quota_refresh(snap.account_id);
            }
        }
    }

    /// 后台配额轮询的单轮(在轮询任务内**顺序内联**执行,不再 per-account spawn——避免上万
    /// 账号周期性创建上万个仅挂在 semaphore 上的 task,对抗审查 Architect#1)。
    ///
    /// 对**未禁用**且**距上次尝试 ≥ `QUOTA_POLL_MIN_SECS`** 的账号顺序刷新一次 getUsageLimits
    /// (只读,安全):
    /// - **跳过 disabled 账号**(人工隔离 / invalid_refresh_token / quota_exhausted / 配置禁用):
    ///   禁用即"不再使用",不该对其周期性打上游/试刷 token(对抗审查 Skeptic#4 / Architect#2 /
    ///   Minimalist#2)。
    /// - 与 /health 路径经 `quota_inflight` 去重(同账号同时只一个在途);被 /health
    ///   (TTL=[`QUOTA_TTL`]=60s)在 floor 内刷过的账号本轮自然跳过 → 不双倍打点。
    /// - 顺序处理:后台 sweep 不赶时间;`quota_sem` 仍护 /health 侧并发。
    async fn sweep_stale_quotas(self: &Arc<Self>) {
        let floor = std::time::Duration::from_secs(QUOTA_POLL_MIN_SECS);
        let now = std::time::Instant::now();
        let due: Vec<String> = self
            .scheduler
            .status_snapshot()
            .into_iter()
            .filter(|s| !s.disabled)
            .map(|s| s.account_id)
            .filter(|id| match self.quota_cache.lock().get(id) {
                Some((_, at)) => now.duration_since(*at) >= floor,
                None => true, // 从未查过 → 该刷。
            })
            .collect();
        for id in due {
            // 与 /health 触发去重:已在途则跳过,否则同账号并发两次 getUsageLimits。
            if !self.quota_inflight.lock().insert(id.clone()) {
                continue;
            }
            self.refresh_quota_once(&id).await;
            self.quota_inflight.lock().remove(&id);
        }
    }

    /// 拉一次配额(可能触发 token 刷新)。Ok(None) = 账号已不在/上游无配额。
    /// 错误保留 [`UpstreamError`](而非 anyhow 包装):按需验活端点要透出可分类的
    /// 状态码/kind(invalid_grant vs 网络抖动),后台轮询调用方只 log 不受影响。
    async fn try_fetch_quota(
        self: &Arc<Self>,
        account_id: &str,
    ) -> Result<Option<AccountQuota>, UpstreamError> {
        let Some(account) = self.scheduler.account(account_id) else {
            return Ok(None);
        };
        let account = self.ensure_credentialed(account).await?;
        // 企业/IdC 号 getUsageLimits 同样要求 profileArn:缺则先发现+持久化。
        let account = self.ensure_profile_arn(account).await;
        match self.provider.account_quota(&account).await {
            // TokenInvalid(401/403 bearer invalid)兜底一次,治两类常见误判:
            //  (1) at 已过期——导入号无 expires_at,has_fresh_token 误当新鲜、没预刷;
            //  (2) profileArn 套错——付费 builderid 号被免费层固定共享 ARN 短路,拿不到自己的 profile。
            // 故:强制刷新 at + (若仍无自带 profile_arn)强制重发现真实 profileArn 并持久化,
            // 再重试一次;仍失败才透出(真死号/真被封)。对齐 chat 路径 refresh_after_rejection 精神。
            // API Key 号排除:ksk_ 无可刷新(force_refresh 空操作)、也不需要 profileArn,
            // 兜底刷新+重试同 key 纯属放大配额轮询流量,直接透出错误(审查 r2 Skeptic#1)。
            Err(e)
                if matches!(e.kind, UpstreamErrorKind::TokenInvalid)
                    && !gw_kiro::machine_id::is_api_key_credential(&account) =>
            {
                let account = self.force_refresh(account).await?;
                // 付费号 profileArn 套错自愈:强制发现真实 ARN 并持久化(免费号/已有值不动)。
                // 与 chat 路径 403 兜底共用 discover_paid_profile_arn,逻辑单一来源不分叉。
                let account = self
                    .discover_paid_profile_arn(&account, "profileArn(配额 403 兜底强制发现)")
                    .await
                    .unwrap_or(account);
                self.provider.account_quota(&account).await
            }
            other => other,
        }
    }

    /// 付费号 profileArn 套错**自愈**:强制发现真实 profileArn 并持久化,返回更新后账号。
    ///
    /// 仅当 [`needs_profile_discovery`] 成立(账号无自带 profile_arn 且订阅为付费非 FREE)
    /// 才发现——免费号维持免费层固定共享 ARN,绝不被后台/chat 的例行 403 带进 force_discover
    /// 污染其真实 chat(resolve_profile_arn 被 chat 复用)。返回:
    /// - `Some(账号)`:发现到真实 arn 且已持久化(调用方改用它重试);
    /// - `None`:非付费 / 已有 arn / 发现失败(调用方维持原账号)。
    ///
    /// 配额路径(`try_fetch_quota`)与 chat 路径 403 兜底共用本方法,避免逻辑分叉。
    async fn discover_paid_profile_arn(
        self: &Arc<Self>,
        account: &Arc<Account>,
        trigger: &str,
    ) -> Option<Arc<Account>> {
        if !needs_profile_discovery(account) {
            return None;
        }
        match self.provider.force_discover_profile_arn(account).await {
            Ok(Some(arn)) => {
                self.persist_extra_field(
                    &account.account_id,
                    "profile_arn",
                    serde_json::Value::String(arn.clone()),
                    trigger,
                )
                .await;
                self.correct_region_from_arn(account, &arn).await;
                Some(
                    self.scheduler
                        .account(&account.account_id)
                        .unwrap_or_else(|| account.clone()),
                )
            }
            // 免费号发现不到(空/错)/ 发现调用失败 → None,维持固定兜底靠刷新后的新 at 过。
            _ => None,
        }
    }

    /// profileArn 一旦被**真正发现**(运行时首次拿到,导入时该号没带),就反过来修正
    /// `region`/`api_region`——号商导出常不带 profileArn、顶层 region 靠猜(常错),
    /// 真实服务区只有靠 ARN 才知道。与导入时 [`gw_kiro::import::region_from_profile_arn`]
    /// 同一份真理来源,不重复实现;`ensure_profile_arn`/`discover_paid_profile_arn` 首次
    /// 发现 ARN 后都调用本方法,自愈不需要人工 PATCH。不改 `auth_region`(独立来源)。
    async fn correct_region_from_arn(&self, account: &Account, arn: &str) {
        let Some(region) = region_correction_from_arn(arn, account.extra_str("region")) else {
            return;
        };
        tracing::info!(account = %account.account_id, region,
            "profileArn 揭示服务区不符,自动修正 region/api_region");
        self.persist_extra_field(
            &account.account_id,
            "region",
            serde_json::Value::String(region.into()),
            "region(profileArn 揭示服务区)",
        )
        .await;
        self.persist_extra_field(
            &account.account_id,
            "api_region",
            serde_json::Value::String(region.into()),
            "api_region(profileArn 揭示服务区)",
        )
        .await;
    }

    /// 从 DB 重读组内账号集并同步进 scheduler —— 30s 周期循环与 `/sync` 立即同步
    /// **共用本实现**(勿另写一份,语义漂移=同步行为分叉)。先冲刷上轮回写失败的
    /// 脏 extra,再差量同步账号集。返回 (added, removed);无库/读库失败 → None。
    /// 全程持 `sync_lock`:保证"读快照→应用"原子,后读的快照必然后应用。
    async fn sync_accounts_from_db(self: &Arc<Self>) -> Option<(usize, usize)> {
        let store = self.store.clone()?;
        let _serialized = self.sync_lock.lock().await;
        // 先冲刷 suspend 生命周期落库队列(最新状态覆盖旧的;退役条目同事务置
        // disabled=1)。**条件写**:Ok(false) = epoch 竞态落败(人工恢复更新),
        // 不回队——紧随其后的对账会采用库内真相;Err 才回队下轮重试。
        for (id, (lc, set_disabled)) in self.scheduler.take_pending_lifecycles() {
            match store.persist_suspend_lifecycle(&id, &lc, set_disabled) {
                Ok(true) => {
                    if set_disabled {
                        tracing::warn!(account = %id, "suspend 自动退役已落库(disabled=1)");
                    }
                }
                Ok(false) => {
                    tracing::debug!(account = %id, "suspend 生命周期落库 epoch 落败,由对账收敛");
                }
                Err(e) => {
                    tracing::error!(account = %id, "suspend 生命周期落库失败,下轮 sync 重试: {e}");
                    self.scheduler.requeue_lifecycle(&id, (lc, set_disabled));
                }
            }
        }
        // 对账:库内 epoch 更新(=人工恢复)的号,采用库内真相重水合运行态——
        // 运行态冷却的号 DB disabled 恒 0,配置翻转路径感知不到人工恢复,靠这里送达。
        match store.load_suspend_lifecycles() {
            Ok(rows) => self.scheduler.adopt_db_lifecycles(&rows),
            Err(e) => tracing::warn!("suspend 生命周期对账读库失败,本轮跳过: {e}"),
        }
        // 先重试上轮回写失败的 extra(脏账号),失败下轮再试。
        flush_dirty_extras(&self.scheduler, &store, &self.refresh_locks, "sync 重试").await;
        // 成员边与账号集**先都读出来,两个都成功才发布**(对抗审查 Skeptic#3)。
        // 分别读、分别发布会留下无限期的撕裂态:membership 成功而账号读失败 → 新视图
        // 立即生效但账号快照停在上一轮;反过来 membership 读失败而账号成功 → **已被撤销
        // 的成员边继续授权**,这是提权方向,尤其危险。任一失败就整轮跳过。
        let (memberships, accounts) = match (
            store.load_group_memberships(&self.group),
            store.load_owned_accounts(&self.group),
        ) {
            (Ok(m), Ok(a)) => (m, a),
            (m, a) => {
                let err = m.err().map(|e| e.to_string()).or_else(|| a.err().map(|e| e.to_string()));
                tracing::warn!("sync 读库失败,整轮跳过(不发布半份快照): {err:?}");
                return None;
            }
        };
        // 发布顺序:**先视图后账号**。这样被撤销的成员边立刻失效(安全方向),而新进账号
        // 至多短暂不可选(仅是暂时不可用,不是提权)。反过来发布会让撤销晚一步生效。
        *self.group_views.write() = memberships
            .into_iter()
            .map(|(g, rank)| (g.clone(), scheduler::GroupView::new(g, rank)))
            .collect();
        let accounts =
            filter_by_provider(accounts.into_iter().map(Arc::new).collect(), self.provider.family());
        let out = self.scheduler.sync_accounts(accounts);
        if out.added + out.removed > 0 {
            tracing::info!(added = out.added, removed = out.removed, "账号集已按 DB 同步");
            // 新进账号若缺订阅档位,预热配额查询补齐(模型过滤数据源)。
            self.warm_subscription_titles();
        }
        Some((out.added, out.removed))
    }
}

/// 配额 → JSON(null = 尚无缓存/查询中或查询失败,前端显示 —)。
fn quota_to_json(q: Option<AccountQuota>) -> serde_json::Value {
    match q {
        Some(q) => serde_json::json!({
            "used": q.used,
            "limit": q.limit,
            "remaining": q.remaining,
            "percent_used": q.percent_used,
            "label": q.currency,
            // 多窗口利用率(dario 的 5h/7d);Kiro 为空数组,前端据此分流显示。
            "windows": q.windows.iter().map(|w| serde_json::json!({
                "label": w.label,
                "percent_used": w.percent_used,
                "reset_at": w.reset_at,
            })).collect::<Vec<_>>(),
            // 超额(on-demand)额度:null = 该 provider 无此概念 / 未查到,前端显示 —。
            "on_demand": q.on_demand.as_ref().map(|od| serde_json::json!({
                "enabled": od.enabled,
                "limit": od.limit,
                "used": od.used,
                "unlimited": od.unlimited,
            })),
        }),
        None => serde_json::Value::Null,
    }
}

/// 是否需要为该账号强制发现真实 profileArn:无自带 profile_arn(空/缺)**且**订阅为付费
/// (subscription_title 存在且不含 FREE)。付费 builderid 号有自己的 profile,被免费层固定
/// 共享 ARN 短路后 getUsageLimits/chat 都 403;免费号(或订阅档未回填)一律不发现,维持
/// 共享 ARN,绝不误污染健康免费号。付费闸门 = 免费/付费结构同构下唯一安全的区分维度。
fn needs_profile_discovery(account: &Account) -> bool {
    account
        .extra_str("profile_arn")
        .map_or(true, |s| s.trim().is_empty())
        && account
            .extra_str("subscription_title")
            .is_some_and(|t| !t.to_uppercase().contains("FREE"))
}

/// 若新发现的 profileArn 揭示了与当前配置**不同**的已知服务区,返回该区域(供调用方
/// 据此修正 region + api_region)。ARN 区域段不在 [`gw_kiro::import::region_from_profile_arn`]
/// 认可的已知服务区清单内,或与当前一致 → `None`(不改动)。纯函数,不发网络请求。
///
/// 大小写不敏感比较:`current_region` 是 DB 里的原始字符串,admin `PATCH /accounts/{id}`
/// 的 `extra` 整块替换不做区域大小写归一,可能存进混合大小写值(如 "US-EAST-1")。若直接
/// `!=` 会把等价值误判成"不同"、触发多余的重写(无害但吵)——按 discovered 的归一口径比较。
fn region_correction_from_arn(arn: &str, current_region: Option<&str>) -> Option<&'static str> {
    let discovered = gw_kiro::import::region_from_profile_arn(arn)?;
    let already_matches = current_region.is_some_and(|r| r.eq_ignore_ascii_case(discovered));
    (!already_matches).then_some(discovered)
}

/// 账号是否持有未过期(且非临近过期)的 access_token。
///
/// 无 access_token → false(需刷新)。有 token 但无 expires_at → 视为有效(无从判断,
/// 沿用旧行为;真过期会被上游 403 触发 force_refresh 兜底)。有 expires_at → 距现在
/// < 60s 视为临近过期需提前刷新(对齐 kiro.rs cred_expiring_soon)。
fn has_fresh_token(account: &Account) -> bool {
    // API Key 凭据:ksk_ 长期有效、无刷新概念 → 永远"新鲜",绝不触发 refresh_auth。
    if gw_kiro::machine_id::is_api_key_credential(account) {
        return true;
    }
    let Some(tok) = account.extra_str("access_token") else {
        return false;
    };
    if tok.is_empty() {
        return false;
    }
    match account.extra_str("expires_at").and_then(parse_rfc3339_unix) {
        Some(exp) => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            exp - now > 60 // 留 60s 余量提前刷新
        }
        None => true, // 无过期信息 → 当作有效,靠 403 兜底
    }
}

/// 解析 "YYYY-MM-DDTHH:MM:SSZ"(token.rs 写入的格式)为 Unix 秒。失败返回 None。
fn parse_rfc3339_unix(s: &str) -> Option<i64> {
    // 仅支持本项目 token.rs 产出的 UTC "Z" 形态(纯算术,不引 chrono)。
    let s = s.strip_suffix('Z').unwrap_or(s);
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    let mut t = time.split(':');
    let hh: i64 = t.next()?.parse().ok()?;
    let mm: i64 = t.next()?.parse().ok()?;
    let ss: i64 = t.next().unwrap_or("0").parse().ok()?;
    // civil → days since epoch(Howard Hinnant 算法,与 token.rs format_unix_utc 互逆)。
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hh * 3600 + mm * 60 + ss)
}

pub async fn run(
    instance: u32,
    instances_path: &Path,
    accounts_path: &Path,
    system_path: &Path,
    db_path: &Path,
) -> anyhow::Result<()> {
    let instances: InstancesConfig = load_yaml(instances_path)?;
    instances.validate()?; // 拓扑约束:同组多 worker 等违规直接拒绝启动。
    // accounts.yaml 自切片④起为可选:首启播种用,导入后 DB 是账号事实源。
    let accounts_cfg: Option<AccountsConfig> = match load_yaml::<AccountsConfig>(accounts_path) {
        Ok(c) => Some(c),
        Err(e) => {
            tracing::info!("accounts.yaml 不可用(账号将只来自 DB): {e}");
            None
        }
    };
    // system.yaml **缺失**是合法形态(全用默认值);但**存在却解析不了**必须当场拒绝启动。
    //
    // 原先一律 `unwrap_or_default()`:各配置段都带 `deny_unknown_fields`,所以一个拼错的字段名
    // 会让**整个** SystemConfig 静默换成默认值 —— 上游超时、调度参数、缓存计费、图像压缩、
    // 实验开关一起被重置,而线上只表现为"行为莫名其妙变了",日志里连一行指向配置的线索都没有。
    // 对抗审查 Skeptic#1 指出这条,新增 thinking 段又给它添了一个新触发点。
    let system: SystemConfig = if system_path.exists() {
        load_yaml(system_path).map_err(|e| {
            anyhow::anyhow!(
                "{} 解析失败,拒绝以默认配置启动(默认值会静默重置超时/调度/缓存/实验开关): {e}",
                system_path.display()
            )
        })?
    } else {
        tracing::info!("{} 不存在,SystemConfig 全用默认值", system_path.display());
        SystemConfig::default()
    };

    let wcfg = instances
        .worker(instance)
        .ok_or_else(|| anyhow::anyhow!("instances.yaml 中无 instance={instance}"))?
        .clone();

    // 控制面库:账号事实源 + usage 落库(与 router 共享同一 SQLite,WAL 多进程并发)。
    // 打不开则降级:账号退回 yaml 启动快照、usage 仅记日志不入库。
    let store: Option<Arc<SqliteStore>> = match SqliteStore::open(db_path) {
        Ok(s) => Some(Arc::new(s)),
        Err(e) => {
            tracing::warn!("控制面库打开失败(账号回退 yaml,usage 不落库): {e}");
            None
        }
    };

    // 首启播种:yaml → DB 幂等导入(已有行不覆盖,绝不回滚已 roll 的 token)。
    if let (Some(store), Some(cfg)) = (&store, &accounts_cfg) {
        match store.import_accounts(cfg) {
            Ok(0) => {}
            Ok(n) => tracing::info!(imported = n, "accounts.yaml 新账号已导入 DB"),
            Err(e) => tracing::warn!("accounts.yaml 导入失败: {e}"),
        }
    }

    // 账号集:DB 优先(admin 可管理),无库时退回 yaml。
    let accounts: Vec<Arc<Account>> = match &store {
        Some(store) => store
            .load_owned_accounts(&wcfg.account_group)?
            .into_iter()
            .map(Arc::new)
            .collect(),
        None => accounts_cfg
            .as_ref()
            .and_then(|c| c.group_accounts_with_provider(&wcfg.account_group))
            .unwrap_or_default()
            .into_iter()
            .map(Arc::new)
            .collect(),
    };
    if accounts.is_empty() {
        tracing::warn!(group = %wcfg.account_group,
            "组内暂无账号(可经 admin 添加,30s 内生效);期间请求将报『组内无账号』");
    }

    // provider 家族:yaml 组定义优先,否则取组内首个账号的 provider,缺省 kiro。
    let provider_family = accounts_cfg
        .as_ref()
        .and_then(|c| c.group(&wcfg.account_group))
        .map(|g| g.provider.clone())
        .or_else(|| {
            accounts
                .first()
                .map(|a| a.provider.clone())
                .filter(|p| !p.is_empty())
        })
        .unwrap_or_else(|| "kiro".to_string());

    // 单 provider 组模型:与本 worker provider 家族不符的账号一律跳过——
    // 否则会拿 Kiro 实现去刷新/调用别家凭据(审查 Architect#2)。
    let accounts = filter_by_provider(accounts, &provider_family);

    let registry = Registry::with_builtins();
    tracing::debug!(providers = ?registry.families(), "已注册 provider");
    // 先按本 worker 的固定出口构造 egress client,注入 provider——
    // 保证该 provider 所有上游请求走同一出口 IP(防关联封号)。
    let client = egress::build_client(&wcfg.egress, system.upstream_timeout_secs)?;
    let egress_desc = egress::describe(&wcfg.egress);
    // DB 设置 overlay:叠在 system.yaml 基线上得"有效配置"(热调首启即生效)。
    // default_proxy 单独取出注入 provider(出口选择,不属运行开关)。
    let mut effective_system = system.clone();
    // 启动期这次解析的结果要**带到 /health 上**,不能只进日志:退回 YAML 基线是会打出
    // 全量 503 的状态,而它原本的唯一痕迹是一行启动日志 —— 事后没人翻得到。
    let mut initial_settings_error = String::new();
    let mut initial_settings_unknown: Vec<String> = Vec::new();
    let initial_default_proxy: Option<String> = match store
        .as_ref()
        .and_then(|s| s.get_settings().ok().flatten())
    {
        Some(json) => match serde_json::from_str::<gw_core::config::SystemSettings>(&json) {
            Ok(s) => {
                if !s.unknown.is_empty() {
                    // 启动期没有"上一轮配置"可保持,只能忽略这几个字段继续起。
                    // 但必须喊出来:它意味着本镜像比写库的那个旧。
                    initial_settings_unknown = s.unknown_keys().into_iter().map(String::from).collect();
                    tracing::warn!(
                        keys = ?s.unknown_keys(),
                        "settings overlay 含本版本不认识的字段,已忽略这几个、其余照常生效\
                         (通常说明本进程镜像偏旧)"
                    );
                }
                s.apply_to(&mut effective_system);
                s.default_proxy.clone()
            }
            Err(e) => {
                // ⚠️ 这条是**启动期**的回落,刻意不 fail-closed:否则一次 admin 误操作
                // 就能让整套栈起不来。但级别从 warn 提到 error —— 此时全部调度参数
                // (含 429 冷却)都退回 YAML 基线,是会打出全量 503 的状态。
                initial_settings_error = format!("启动期 overlay 解析失败,已退回 YAML 基线: {e}");
                tracing::error!("settings overlay 解析失败,本次启动退回 YAML 基线(调度参数全部失去热值): {e}");
                None
            }
        },
        None => None,
    };

    // cache_sim 全局 store 的 TTL/容量从有效 cache 同步(否则恒用编译期默认 300s/4096)。
    gw_kiro::cache_sim::global().set_ttl_secs(effective_system.cache.sim_ttl_secs);
    gw_kiro::cache_sim::global().set_max_sessions(effective_system.cache.max_sessions);

    // provider 工厂 cfg:注入有效 cache(缓存计费)+ image(图像压缩,"image" 子对象)+
    // 可选 default_proxy(全局默认出口代理)。序列化失败退回缺省(provider 各自回退,不致命)。
    let mut provider_cfg =
        serde_json::to_value(&effective_system.cache).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut provider_cfg {
        if let Ok(img) = serde_json::to_value(&effective_system.image) {
            map.insert("image".into(), img);
        }
        if let Some(dp) = &initial_default_proxy {
            map.insert("default_proxy".into(), serde_json::json!(dp));
        }
        if let Ok(dario) = serde_json::to_value(&effective_system.dario) {
            map.insert("dario".into(), dario);
        }
    }
    let provider = registry.build(&provider_family, &provider_cfg, client.clone())?;
    // 在 provider 被 move 进 WorkerState 之前取一次:这是「provider 级设置是否热生效」
    // 的唯一权威来源(见 Provider::hot_settings_supported)。
    let provider_hot_supported = provider.hot_settings_supported();

    // 启动即把"有效设置"热应用一次:实验开关等进程级全局仅由 env 初始化,DB overlay 原本要等
    // 30s 轮询(首跳被跳过)才生效。期间若 DB 已设 `thinking_signature=false`,这段窗口里 Kiro
    // 仍会发 Kiro 合成签名 → 重演跨通道 THINKING_SIGNATURE_INVALID(对抗审查 #1)。这里用与轮询
    // 同一 `from_effective` 口径先应用一次,关掉窗口。对 kiro 是幂等(cache/image/proxy 已经 from_config
    // 设过),对 dario 是 no-op(未覆盖 apply_hot_settings)。
    {
        let full = gw_core::config::SystemSettings::from_effective(
            &effective_system,
            initial_default_proxy.clone(),
        );
        if let Ok(sv) = serde_json::to_value(&full) {
            provider.apply_hot_settings(&sv);
        }
        // cursor 追加模型目录启动初载(与 30s 轮询同一口径,别留 30s 窗口)。
        apply_cursor_extra_models(&effective_system);
        // 护栏策略句同理:不初载的话头 30s 用的是内置默认,而线上跑的是配置版 ——
        // 那段窗口的收口率会被记到错误的 guard_rev 上。
        if let Err(e) = apply_cursor_tool_guard(&effective_system) {
            tracing::error!("cursor 护栏策略句启动初载失败,本进程沿用内置默认: {e}");
        }
    }

    // Fix6: For dario workers, filter out accounts that fail validate_account
    // (missing access_token + refresh_token) before they enter the scheduler.
    // Scope this to "claude-dario" only — kiro's validate_account requires
    // refresh_token + machine_id, and there may be edge-case kiro accounts
    // on-line that intentionally omit machine_id (e.g. legacy rows that rely
    // on runtime defaults).  Restricting to dario eliminates any kiro regression
    // risk while still catching dario accounts imported without credentials.
    // cursor 一并纳入(2026-08-07):它的 validate_account 除了 token 还会校验
    // **出口代理构造得出来**。代理配错的 cursor 号必须挡在池外 —— 让它进池的话,
    // 每个打到它的请求都会失败(provider 侧 fail-closed,绝不回退默认出口:回退
    // 等于把本该隔离的号并到同一 IP,已实测的封号维度)。挡在池外 + 启动告警,
    // 比"号在池里但每次都失败"好查得多。
    let accounts = if matches!(provider_family.as_str(), "claude-dario" | "cursor") {
        let total = accounts.len();
        let valid: Vec<Arc<Account>> = accounts
            .into_iter()
            .filter(|a| match provider.validate_account(a.as_ref()) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!(
                        account = %a.account_id,
                        reason = %e,
                        family = %provider_family,
                        "账号校验失败,跳过不进调度池"
                    );
                    false
                }
            })
            .collect();
        let skipped = total - valid.len();
        if skipped > 0 {
            tracing::info!(
                total,
                valid = valid.len(),
                skipped,
                family = %provider_family,
                "账号校验:部分账号未通过(缺凭据 / 出口代理非法)被跳过"
            );
        }
        valid
    } else {
        accounts
    };

    let usage_sink: Option<Arc<dyn UsageSink>> =
        store.clone().map(|s| s as Arc<dyn UsageSink>);

    tracing::info!(
        instance,
        listen = %wcfg.listen,
        egress = %egress_desc,
        group = %wcfg.account_group,
        accounts = accounts.len(),
        provider = provider.family(),
        usage_sink = usage_sink.is_some(),
        "worker 就绪"
    );
    if provider_family == "claude-dario" {
        tracing::warn!(
            worker_egress = %egress_desc,
            "dario 组:本 worker 的 egress 必须与所连 dario sidecar 的出口为同一 IP,且等于该账号登录授权的来源 IP(二者均 direct 或同一代理皆可;不一致=刷新 IP≠发包 IP,关联封号风险)"
        );
    }

    // 成员边**必须在开始接流量之前同步装载**。周期同步的首跳是被跳过的(见下方
    // `tick.tick().await`,注释"启动时刚加载过"只对账号成立),若在这里留空,启动后
    // 头 30 秒内每个带组名的请求都会取到空视图 → GroupEmpty/503,等于每次发版打出一个
    // 30 秒全组不可用窗口(对抗审查 Skeptic#1)。读库失败按空视图起,由周期同步补齐。
    let initial_views: std::collections::HashMap<String, scheduler::GroupView> = store
        .as_ref()
        .map(|s| s.load_group_memberships(&wcfg.account_group))
        .transpose()
        .unwrap_or_else(|e| {
            tracing::error!("启动装载成员边失败,本轮以空视图起(30s 后由周期同步补齐): {e}");
            None
        })
        .unwrap_or_default()
        .into_iter()
        .map(|(g, rank)| (g.clone(), scheduler::GroupView::new(g, rank)))
        .collect();
    tracing::info!(groups = initial_views.len(), "成员边启动装载完成");

    // suspend 生命周期(退避/观察期/退役/落库)只对 kiro 启用:0/35 不可恢复的
    // 实测数据只覆盖 Kiro 号,dario 等 provider 的 TemporarilyBlocked 语义不同,
    // 保持逐字节旧行为(平冷却、到期满血复活)。
    let lifecycle_enabled = provider.family() == "kiro";
    // 落库的 suspend 生命周期在构造时水合:重启不丢退避进度、不重抽抖动,
    // 部署重启不会让"已退到 24h 档"的号提前复活。读库失败按空表起(=旧行为),
    // 由周期 sync 把内存里的新转换陆续落库补齐。
    let lifecycles = if lifecycle_enabled {
        store
            .as_ref()
            .map(|s| s.load_suspend_lifecycles())
            .transpose()
            .unwrap_or_else(|e| {
                tracing::error!("启动装载 suspend 生命周期失败,本轮以空表起(退避进度本轮不水合): {e}");
                None
            })
            .unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    if !lifecycles.is_empty() {
        tracing::info!(accounts = lifecycles.len(), "suspend 生命周期启动水合完成");
    }
    let scheduler =
        AccountScheduler::new_with_lifecycles(accounts, &effective_system.scheduler, &lifecycles, lifecycle_enabled);
    if lifecycle_enabled {
        if let Some(s) = &store {
            scheduler.set_lifecycle_store(s.clone());
        }
    }

    let state = Arc::new(WorkerState {
        group_views: parking_lot::RwLock::new(initial_views),
        instance,
        egress_desc,
        group: wcfg.account_group.clone(),
        provider,
        scheduler,
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink,
        pending_writes: PendingWrites::new(),
        store: store.clone(),
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(QUOTA_MAX_CONCURRENCY)),
        sync_lock: tokio::sync::Mutex::new(()),
        // 启动期的同步结果照实记:此时 `effective_system` 已经叠过 overlay(或在解析失败时
        // 退回了 YAML 基线),两种情况都要能在面板上看出来 —— 启动就退回基线是最危险的
        // 状态(调度参数全失去热值),而它原本只有一行日志。
        settings_sync: parking_lot::RwLock::new(SettingsSync {
            applied_at: if initial_settings_error.is_empty() { now_unix() } else { 0 },
            // 库打不开时 30s 轮询循环**根本不会 spawn**,热调从此永久失效。
            // 不写一句的话,面板 90 秒后只会永久标红「N 秒前同步」,没有任何线索,
            // 运维会把一个按设计降级运行的进程当成同步卡死去重启(对抗审查 Architect#9)。
            error: if store.is_none() && initial_settings_error.is_empty() {
                "控制面库不可用,本 worker 的设置热调不生效(改设置必须重启它)".to_string()
            } else {
                initial_settings_error
            },
            unknown: initial_settings_unknown,
            effective: effective_view(&effective_system),
            provider_hot: provider_hot_supported,
        }),
        _client: client,
    });

    // 账号配置 sync:30s 从 DB 重读组内账号集,admin 增删改无需重启 worker 即生效。
    // (翻转语义见 scheduler::sync_accounts;读库失败跳过本轮,不影响服务。)
    if let Some(store) = store {
        let st = state.clone();
        // YAML 基线快照:每轮把 DB overlay 叠在它之上算"有效配置"再热应用。
        let sys_base = system.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // 首跳立即触发,跳过(启动时刚加载过)。
            // 上一轮告警过的未知字段集。只在**变化时**告警,否则 30s 一条会淹掉日志。
            loop {
                tick.tick().await;
                // 账号集同步(含脏 extra 冲刷)—— 与 /sync 立即同步共用实现。
                st.sync_accounts_from_db().await;
                // 热应用 DB 设置 overlay:代理/计费/图像(provider)+ 调度参数 + cache_sim。
                // 用**有效全量**(YAML 基线叠 overlay,再回灌 from_effective)喂给 provider,
                // 这样 overlay 删某字段时能正确恢复到 YAML 默认(而非停留在上次热值)。
                match store.get_settings() {
                    Ok(opt) => {
                        // ⚠️ 这里曾是 `.ok().unwrap_or_default()` —— 解析失败**静默**变成空
                        // overlay,于是全部调度参数回落 YAML 基线(冷却 300s),企业号几秒内
                        // 被全部打下线 → 全量 503,而日志一行都没有。现在:解析失败**跳过本轮**,
                        // 保持上一轮已生效的配置,并按 error 级别喊出来。
                        let overlay = match opt {
                            None => gw_core::config::SystemSettings::default(),
                            Some(j) => {
                                match serde_json::from_str::<gw_core::config::SystemSettings>(&j) {
                                    Ok(s) => s,
                                    Err(e) => {
                                        // 这一跳会**每 30s 重复发生且永不自愈**,配置就此僵在
                                        // 上一轮的值上。只记日志的话,面板上看到的是「保存成功」,
                                        // 实际再也不会生效 —— 必须让它在 /health 上冒头。
                                        st.settings_sync.write().error = format!(
                                            "overlay 解析失败,配置已僵在上一次成功的值: {}",
                                            redact_err(&e.to_string())
                                        );
                                        tracing::error!(
                                            "settings overlay 解析失败,**保持上一轮已生效配置**\
                                             (不回落 YAML 基线): {e}"
                                        );
                                        continue;
                                    }
                                }
                            }
                        };
                        // 未知字段不再作废整份 overlay(其余字段照常生效),但必须可见:
                        // 它的含义通常是「本进程的镜像比写库的那个旧」。
                        // BTreeMap → 顺序稳定,可以直接比。去重基准取自 settings_sync,
                        // 不再另存一份 warned_unknown(两份手工同步早晚会不一致)。
                        let unknown: Vec<String> =
                            overlay.unknown.keys().cloned().collect();
                        let warned_unknown = st.settings_sync.read().unknown.clone();
                        if unknown != warned_unknown {
                            if !unknown.is_empty() {
                                tracing::warn!(
                                    keys = ?unknown,
                                    "settings overlay 含本版本不认识的字段,已忽略这几个、其余照常生效\
                                     (通常说明本进程镜像偏旧)"
                                );
                            } else if !warned_unknown.is_empty() {
                                tracing::info!("settings overlay 的未知字段已消失,恢复完全识别");
                            }
                        }
                        let mut eff = sys_base.clone();
                        overlay.apply_to(&mut eff);
                        let full = gw_core::config::SystemSettings::from_effective(
                            &eff,
                            overlay.default_proxy.clone(),
                        );
                        // provider 侧应用失败必须记下来。原本这里是 `if let Ok(..)`
                        // 静默跳过:序列化一失败,provider 拿不到新值,而紧随其后的
                        // scheduler / cache_sim 照常更新,快照照记「成功」——
                        // 面板于是显示「一致」而 provider 级设置(缓存计费)根本没变。
                        // 这正是本功能要消灭的那种谎(对抗审查三视角一致指出)。
                        let apply_err = match serde_json::to_value(&full) {
                            Ok(sv) => {
                                st.provider.apply_hot_settings(&sv);
                                String::new()
                            }
                            Err(e) => {
                                tracing::error!("settings 序列化失败,provider 级设置本轮未应用: {e}");
                                format!("provider 级设置未应用(序列化失败): {}", redact_err(&e.to_string()))
                            }
                        };
                        st.scheduler.update_tuning(&eff.scheduler);
                        gw_kiro::cache_sim::global().set_ttl_secs(eff.cache.sim_ttl_secs);
                        gw_kiro::cache_sim::global().set_max_sessions(eff.cache.max_sessions);
                        apply_cursor_extra_models(&eff);
                        // 护栏策略句:校验失败保留上一份有效值,并把原因并进本轮同步错误 ——
                        // 与上面 provider 级设置同一条原则(应用失败必须说出来,不能让面板报「一致」)。
                        let apply_err = match apply_cursor_tool_guard(&eff) {
                            Ok(()) => apply_err,
                            Err(e) => {
                                tracing::error!("cursor 护栏策略句未应用(沿用上一份有效值): {e}");
                                let msg = format!("cursor 护栏策略句未应用: {e}");
                                if apply_err.is_empty() { msg } else { format!("{apply_err}; {msg}") }
                            }
                        };
                        // 应用成功才刷新时间戳与快照:面板上「多久前同步的」才是可信的
                        // 新鲜度指标 —— 失败时它会停住不动,一眼看出配置僵了多久。
                        *st.settings_sync.write() = SettingsSync {
                            applied_at: now_unix(),
                            error: apply_err,
                            unknown: unknown.clone(),
                            effective: effective_view(&eff),
                            // 本 provider 压根不热应用 provider 级设置(dario 就是这样):
                            // scheduler 那半边确实生效了,但缓存计费这半边要重启才动。
                            // 不说出来的话,面板会对着一份「算得出但没生效」的值报「一致」。
                            provider_hot: st.provider.hot_settings_supported(),
                        };
                    }
                    Err(e) => {
                        st.settings_sync.write().error =
                            format!("读库失败,本轮跳过: {}", redact_err(&e.to_string()));
                        tracing::warn!("settings sync 读库失败,跳过本轮: {e}");
                    }
                }
            }
        });
    }

    // static_flow 式后台配额轮询:worker 自身每 240–300s(带抖动)兜底刷新一遍账号配额,
    // **不依赖 /health 被打**。目的:① 复刻真实 Kiro IDE 每 ~5min 一次 getUsageLimits 的
    // ambient 流量(防封——纯反代账号在两次聊天间对上游静默是最易被审计的指纹);② 让配额
    // 面板在无人看 dashboard 时也保持新鲜。被 /health(60s TTL)刚刷过的账号本轮自动跳过,
    // 不产生双倍流量;上游并发仍受 quota_sem(3)节流。
    //
    // 启停:**始终 spawn**,每轮读 `scheduler.quota_poll_enabled()` 热开关(经 SchedulerConfig
    // + 30s settings overlay 热生效——设置面板可即时启停,无需重启;对抗审查 Architect#5/
    // Minimalist#4 取代了原 env 开关)。冷启动:首轮只等 QUOTA_POLL_WARMUP_SECS 就 sweep 一遍
    // (对抗审查 Skeptic#5),之后进入抖动节律。
    // shutdown:本任务是 daemon(同既有 30s sync loop,无独立 shutdown 协调)。安全性依据:
    // refresh_locked 刷新 token 后**同步落库**(merge_account_extra),故停机中途被 drop 也不丢
    // rolling token;sweep 唯一外部副作用是只读 getUsageLimits,被 drop 无损(对抗审查 Architect#6)。
    {
        let st = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(QUOTA_POLL_WARMUP_SECS)).await;
            loop {
                if st.scheduler.quota_poll_enabled() {
                    st.sweep_stale_quotas().await;
                }
                let secs = jittered_secs(QUOTA_POLL_MIN_SECS, QUOTA_POLL_MAX_SECS);
                tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
            }
        });
        tracing::info!(
            min = QUOTA_POLL_MIN_SECS,
            max = QUOTA_POLL_MAX_SECS,
            "后台配额轮询已就绪(static_flow 式 ambient getUsageLimits;启停经设置面板热控)"
        );
    }

    // 启动预热:缺订阅档位的账号后台拉一次配额(getUsageLimits 只读,安全),
    // 让模型能力过滤(FREE 不给 opus)冷启动即有数据源,而非等 /health 被动触发。
    state.warm_subscription_titles();

    let mut app = Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/models", get(models))
        .route("/health", get(health))
        // 只回显设置同步实况的**轻量**端点。
        //
        // 为什么不复用 /health:那个端点会跑全账号 status_snapshot,并对配额缓存
        // 陈旧的账号触发上游 getUsageLimits —— 也就是说「有人打开设置页」会变成
        // 「对付费账号打上游」。这个账号池对封禁很敏感,一个只读的可观测性接口
        // 不该和生产流量耦合(对抗审查三视角一致指出)。这里只读一把 RwLock。
        .route("/settings-sync", get(settings_sync));

    // OpenAI 线缆:cursor 上游本来就供 gpt / grok / gemini 这些非 Anthropic 家族的模型,
    // 却只能用 Anthropic 协议去要 —— 给它开 OpenAI 入口,顺带省掉下游 NewAPI 那道
    // 有损的 Claude→OpenAI 转换(它只认 5 种事件,见 `keepalive_frame` 的注释)。
    //
    // **按 family 条件挂载**是结构性闸门:kiro / dario / claude-subprocess 上这两条路径
    // 根本不存在(404),不靠文档里的君子协定约束。它们的主链路全程 Anthropic、
    // 零转换,那是刻意保住的资产(见 gw_core::provider 模块文档),不该被 OpenAI 入口稀释。
    if mount_openai_wire(state.provider.family()) {
        app = app
            .route("/v1/chat/completions", post(chat_completions))
            .route("/v1/responses", post(responses));
        tracing::info!(
            family = state.provider.family(),
            "已挂载 OpenAI 线缆入口: /v1/chat/completions + /v1/responses"
        );
    }

    // worker 不做对外鉴权、且信任 router 注入的 X-Gw-Client-Key;必须只绑 loopback,
    // 否则客户端可直连 worker 绕过 router 鉴权并伪造用量归属(审查 #2)。
    let loopback = is_loopback_listen(&wcfg.listen);
    if loopback {
        // 内网管理端点(无鉴权,信任同机 router):人工救号 + 人工强制刷新 token。
        // **仅 loopback 才挂载**——非 loopback 误配下暴露它等于把"清禁用保护/强制刷新"
        // 开给整个网络(审查②R 共识 high)。
        app = app
            .route("/accounts/{id}/reset", post(reset_account))
            .route("/accounts/{id}/refresh", post(refresh_account))
            .route("/accounts/{id}/quota", post(quota_account))
            .route("/accounts/{id}/on-demand", post(on_demand_account))
            .route("/accounts/{id}/models", post(models_account))
            .route("/accounts/{id}/models/local", get(models_local))
            .route("/accounts/{id}/probe", post(probe_account))
            .route("/oauth/exchange", post(oauth_exchange))
            .route("/sync", post(sync_now));
    } else {
        tracing::warn!(
            listen = %wcfg.listen,
            "⚠️ worker 绑定到非 loopback 地址:这会让客户端绕过 router 鉴权并伪造 client_key 归属;\
             reset 管理端点已禁用。请改绑 127.0.0.1,或为 router→worker 内网跳加共享密钥。"
        );
    }
    // 入站体积上限:messages 用 Json<Value> 全量缓冲,axum 默认 2MB 会在 handler 前 413。
    // 提到 effective_system.max_request_body_bytes(默认 16MB)。这是第二道入站咽喉——
    // router 放开后此处不放开仍会再 413(两处缺一不可)。管理端点无 body,层对其无害。
    // 启动日志打印有效值:与 router 日志对照即可发现两进程配置漂移(只重启一侧时原 bug
    // 会在 worker 侧复现——Architect 审查)。
    let max_body = effective_system.effective_max_request_body_bytes();
    tracing::info!(
        max_request_body_bytes = max_body,
        "worker 入站体积上限(应与 router 一致;不一致=配置漂移,大请求仍会在此 413)"
    );
    let app = app
        .layer(axum::extract::DefaultBodyLimit::max(max_body))
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind(&wcfg.listen).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(crate::shutdown_signal("worker"))
        .await?;

    // 排空后先等 Drop 里 detach 的 usage/quota 落库任务收尾(graceful shutdown 只等
    // 响应体,不等这些 spawn 任务;5s 上限防极端卡死)。
    if !state
        .pending_writes
        .wait_idle(std::time::Duration::from_secs(5))
        .await
    {
        tracing::warn!("停机:5s 内仍有 usage 落库任务未完成,最后一批记录可能丢失");
    }
    // 最后机会落盘脏 extra:30s 重试循环随进程退出被 drop,「刷新成功但 DB 回写
    // 失败」的 rolling token 若不在此落盘,退出即丢(账号下次只能拿旧 token 刷新,
    // 可能已被上游作废)。
    if let Some(store) = &state.store {
        flush_dirty_extras(&state.scheduler, store, &state.refresh_locks, "停机排空").await;
    }
    Ok(())
}

/// 在途异步落库登记:SSE 收尾的 usage 落库是 Drop 里 detach 的 spawn 任务,
/// graceful shutdown 只等响应体、不等这些任务——停机时经 wait_idle 等到清零
/// (或超时),否则最后一批 usage/quota 增量会随 runtime 关闭静默丢失(审查 Skeptic#2)。
struct PendingWrites {
    count: std::sync::atomic::AtomicUsize,
    notify: tokio::sync::Notify,
}

/// RAII 登记凭据:创建即计数 +1,Drop -1 并唤醒等待者。
struct PendingWriteGuard(Arc<PendingWrites>);

impl PendingWrites {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            count: std::sync::atomic::AtomicUsize::new(0),
            notify: tokio::sync::Notify::new(),
        })
    }

    fn enter(self: &Arc<Self>) -> PendingWriteGuard {
        self.count.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        PendingWriteGuard(self.clone())
    }

    /// 等到无在途落库或超时;返回最终是否清零。
    async fn wait_idle(&self, timeout: std::time::Duration) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if self.count.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return true;
            }
            let notified = self.notify.notified();
            // 注册 waker 后复查,封住「最后一个 guard 在两步之间 drop」的丢唤醒窗口。
            if self.count.load(std::sync::atomic::Ordering::Acquire) == 0 {
                return true;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return self.count.load(std::sync::atomic::Ordering::Acquire) == 0;
            }
        }
    }
}

impl Drop for PendingWriteGuard {
    fn drop(&mut self) {
        if self.0.count.fetch_sub(1, std::sync::atomic::Ordering::AcqRel) == 1 {
            self.0.notify.notify_waiters();
        }
    }
}

/// per-account 刷新锁表的取锁(WorkerState::refresh_lock 与 flush 共用同一张表,
/// 保证 flush 与 token 刷新/配额回填互斥)。
type RefreshLocks = parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>;

fn lock_for(locks: &RefreshLocks, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    let mut map = locks.lock();
    map.entry(account_id.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

/// 把调度器里标脏的 extra(刷新成功但 DB 回写失败的 rolling token)逐个落盘,
/// 成功才清脏位——清位前 sync 不会用 DB 旧值覆盖内存新 token。
/// 30s sync 循环(失败下轮重试)和停机排空(最后机会,失败即丢)共用。
///
/// **必须**逐账号持 refresh_lock 并在锁内重读副本+重查脏位:用入口处的旧快照
/// 直接 merge 会与并发刷新竞速——刷新已落库新 token 并清脏后,本函数再把旧快照
/// 整块写回 = DB 回滚到已作废 refresh_token(审查②R Skeptic#2)。
async fn flush_dirty_extras(
    scheduler: &AccountScheduler,
    store: &SqliteStore,
    refresh_locks: &RefreshLocks,
    context: &str,
) {
    let dirty_ids: Vec<String> = scheduler
        .dirty_accounts()
        .iter()
        .map(|a| a.account_id.clone())
        .collect();
    for id in dirty_ids {
        let lock = lock_for(refresh_locks, &id);
        let _guard = lock.lock().await;
        // 锁内重读:并发刷新可能已替换副本/自行落库清脏。
        if !scheduler.is_extra_dirty(&id) {
            continue;
        }
        let Some(acc) = scheduler.account(&id) else { continue };
        let persisted = serde_json::to_string(&acc.extra)
            .map_err(anyhow::Error::from)
            .and_then(|j| store.merge_account_extra(&id, &j));
        match persisted {
            Ok(_) => {
                scheduler.clear_extra_dirty(&id);
                tracing::info!(account = %id, "{context}: 脏 extra 持久化成功");
            }
            Err(e) => tracing::warn!(account = %id,
                "{context}: 脏 extra 持久化失败: {e}"),
        }
    }
}

/// 过滤掉与本 worker provider 家族不符的账号(单 provider 组模型;
/// provider 为空 = 跟随组家族,放行)。
fn filter_by_provider(accounts: Vec<Arc<Account>>, family: &str) -> Vec<Arc<Account>> {
    accounts
        .into_iter()
        .filter(|a| {
            let ok = a.provider.is_empty() || a.provider == family;
            if !ok {
                tracing::warn!(account = %a.account_id, provider = %a.provider, family,
                    "账号 provider 与本 worker 家族不符,跳过(不会被服务)");
            }
            ok
        })
        .collect()
}

/// listen 地址是否绑 loopback(127.0.0.1 / ::1 / localhost)。
fn is_loopback_listen(listen: &str) -> bool {
    if let Ok(addr) = listen.parse::<std::net::SocketAddr>() {
        return addr.ip().is_loopback();
    }
    match listen.rsplit_once(':') {
        Some((host, _)) => {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .map(|ip| ip.is_loopback())
                    .unwrap_or(false)
        }
        None => false,
    }
}

/// 把 cursor 热追加模型目录写进 gw-cursor 的进程全局表(启动初载与 30s 轮询共用)。
/// 对非 cursor worker 是无害 no-op(模型目录只有 cursor provider 会读)。
/// 空名条目直接丢弃(写侧不拦,读侧兜底)。
fn apply_cursor_extra_models(cfg: &gw_core::config::SystemConfig) {
    let models = cfg
        .cursor_extra_models
        .iter()
        .filter(|s| !s.name.trim().is_empty())
        .map(|s| {
            let params: Vec<(&str, &str)> = s
                .params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            let m = gw_cursor::run::Model::with_params(s.name.trim(), &params);
            // menu=false(默认)= 探测位:可被点名但不进 1.14 清单。
            if s.menu { m } else { m.probe() }
        })
        .collect();
    gw_cursor::set_extra_models(models);
}

/// 把 cursor 内建工具护栏的策略句写进 gw-cursor 的进程全局(启动初载与 30s 轮询共用)。
/// 对非 cursor worker 是无害 no-op(只有 cursor provider 会读)。
///
/// 校验失败**保留上一份有效值**并把原因交回调用方计入 settings 同步错误 ——
/// 不静默回默认:护栏的效果正在被按版本分桶比对收口率,悄悄换一版会让那份数据作废。
fn apply_cursor_tool_guard(cfg: &gw_core::config::SystemConfig) -> Result<(), String> {
    gw_cursor::set_tool_guard_policy(&cfg.cursor_tool_guard)
}

/// `GET /settings-sync` —— 设置热调实况(轻量,不碰 scheduler 与配额)。
async fn settings_sync(State(st): State<Arc<WorkerState>>) -> impl IntoResponse {

    Json(serde_json::json!({
        "role": "worker",
        "instance": st.instance,
        "group": st.group,
        "provider": st.provider.family(),
        "settings": settings_view(&st),
    }))
}

/// 设置同步实况的 JSON 投影(`/health` 与 `/settings-sync` 共用,免得两处分叉)。
fn settings_view(st: &WorkerState) -> serde_json::Value {
    let s = st.settings_sync.read();
    serde_json::json!({
        "applied_at": s.applied_at,
        // 距今多少秒。轮询周期 30s,所以 >90 基本就是同步停了。-1 = 一次都没成功过。
        "age_secs": if s.applied_at > 0 { now_unix() - s.applied_at } else { -1 },
        "error": s.error,
        "unknown": s.unknown,
        "effective": s.effective,
        // false = provider 级设置(缓存计费等)对本 worker 要重启才生效。
        "provider_hot": s.provider_hot,
        // cursor 内建工具护栏策略句的短指纹。**只回显指纹不回显全文** ——
        // 全文是每个请求都发给上游的系统提示的一部分,不该跟着健康快照到处走。
        // 它回答的是「线上跑的是哪一版、从什么时候起」,正是按版本分桶比对
        // 内建工具收口率需要的那个字段。非 cursor worker 上它恒为内置默认的指纹。
        "cursor_guard_rev": gw_cursor::tool_guard_rev(),
    })
}

async fn health(State(st): State<Arc<WorkerState>>) -> impl IntoResponse {
    // 每账号运行态 + 配额(配额读缓存,陈旧时后台刷新,不阻塞本响应)。
    let accounts_status: Vec<serde_json::Value> = st
        .scheduler
        .status_snapshot()
        .into_iter()
        .map(|s| {
            let quota = st.quota_cached_or_refresh(&s.account_id);
            let mut v = serde_json::to_value(&s).unwrap_or(serde_json::Value::Null);
            if let serde_json::Value::Object(map) = &mut v {
                map.insert("quota".into(), quota_to_json(quota));
            }
            v
        })
        .collect();
    Json(serde_json::json!({
        "role": "worker",
        "instance": st.instance,
        "egress": st.egress_desc,
        "group": st.group,
        "provider": st.provider.family(),
        "accounts": st.scheduler.total(),
        // 每账号运行态(冷却/封禁/并发占用)+ 配额,admin 账号页经 router 聚合展示。
        "accounts_status": accounts_status,
        // 排队实况(等待数 / 容量 / 已开号数),admin 账号页展示。
        "queue": st.scheduler.queue_stats(),
        // usage 是否在落库:库打开失败时为 false(降级,usage 不入库),便于运维发现。
        "usage_persist": st.usage_sink.is_some(),
        // 设置热调的实况:本 worker **真正在用**的值 + 最近一次同步的结果。
        // 面板据此回答「我保存的设置到底生效没有」—— 见 [`SettingsSync`]。
        "settings": settings_view(&st),
        "status": "ok"
    }))
}

/// `POST /accounts/{id}/reset` —— 内网管理:人工救号。清运行时禁用/冷却/失败计数
/// (含 429 节流窗口、RPM 滑动窗口、(账号,模型) 不可用标记;配置层 disabled 不动,
/// 那走 PATCH)。由 admin(router 进程)扇出调用;
/// worker 只绑 loopback,信任内网调用方。404 = 账号不在本 worker 组。
async fn reset_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    if st.scheduler.reset_account(&id) {
        Json(serde_json::json!({"reset": true, "account_id": id})).into_response()
    } else {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"reset": false, "account_id": id})),
        )
            .into_response()
    }
}

/// router→worker 换码请求体。`proxy`=该号 extra.proxy(None=组默认出口),`code`/`verifier`=PKCE。
#[derive(serde::Deserialize)]
struct OAuthExchangeBody {
    #[serde(default)]
    proxy: Option<String>,
    code: String,
    verifier: String,
}

/// `POST /oauth/exchange` —— **无状态**铸 token:`authorization_code` → token set。
///
/// 走**本 worker 的 egress**(由 `provider.oauth_exchange` 经 `egress_client_for` 选)=该号
/// 将来 refresh/chat 同一出口 IP(铸≠发=关联封号)。router 只把请求扇给目标组(account_group
/// 匹配)的 worker,故出口正确;非 dario provider 默认实现回 BadRequest(400)。
/// 账号此刻尚未入库——纯换码,不碰 scheduler。token 明文只回给同机 router 落库,不进日志。
async fn oauth_exchange(
    State(st): State<Arc<WorkerState>>,
    Json(body): Json<OAuthExchangeBody>,
) -> axum::response::Response {
    match st
        .provider
        .oauth_exchange(body.proxy.as_deref(), &body.code, &body.verifier)
        .await
    {
        Ok(tokens) => Json(tokens).into_response(),
        // 运维端点:换码失败的原因(上游 OAuth 报文)要原样给面板。
        Err(e) => admin_error_response(&e),
    }
}

/// `POST /accounts/{id}/refresh` —— 人工**强制刷新该账号 token**(rt→at 换一次)。
///
/// 安全性:这就是后台配额轮询 / 按需刷新本来就在做的 OIDC token 交换,**不是** chat,
/// 不触发风控(见 no-chat-test-on-real-accounts 记忆)。操作者可借此验证 refresh_token
/// 仍可用、或在轮换 rt 后立刻换到新 access_token。
///
/// 仅本组持有该账号的 worker 命中;账号不在本组 → 404(admin 据此向其余 worker 续问)。
/// 上游刷新失败(invalid_grant / 网络 / 5xx)→ 经 `upstream_error_response` 透出错误,
/// admin 层据状态码区分"无人持有(404)"与"持有方刷新失败(502/400)"。
async fn refresh_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let Some(account) = st.scheduler.account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"refreshed": false, "account_id": id})),
        )
            .into_response();
    };
    // 开工时快照 suspend 世代:本次刷新可能跑几秒,期间账号若发生状态转换
    // (suspend/复活/恢复),迟到的上报就是旧世代回声,不得改写新状态
    // (对抗审查阻断#7;本路径不走 lease,必须自己快照)。
    let gen = st.scheduler.current_suspend_gen(&id);
    match st.force_refresh(account).await {
        Ok(refreshed) => {
            // 绝不回传 token 明文;只给 expires_at 让操作者确认新 token 的有效期窗口。
            let expires_at = refreshed.extra_str("expires_at").map(|s| s.to_string());
            Json(serde_json::json!({
                "refreshed": true,
                "account_id": id,
                "expires_at": expires_at,
            }))
            .into_response()
        }
        Err(e) => {
            // 反映真实状态(审查 Skeptic#1):与 chat 路径一致地 report_failure——
            // rt 永久失效(TokenInvalid)→ 立即标 invalid_refresh_token 禁用,仪表盘即时
            // 见死号、不再被路由到;transient(网络/5xx)→ 仅计失败数(救号一键清)。
            // 不在此换号/重试(人工动作就是要看这一次结果)。
            st.scheduler.report_failure_with_gen(&id, e.kind, gen);
            // 运维端点:人工点"刷新"就是要看这一次的真实失败原因。
            admin_error_response(&e)
        }
    }
}

/// `POST /accounts/{id}/quota` —— 按需验活:确保 token 有效(必要时刷新,只读 OIDC
/// 交换)→ getUsageLimits 查配额。导入对话框逐账号验活用。**全程只读,绝不发 chat**
/// (见 no-chat-test-on-real-accounts 记忆)。
///
/// 仅本组持有该账号的 worker 命中;不在本组 → 404(admin 据此向其余 worker 续问)。
/// 上游失败 → report_failure(与 refresh 同理:死号立即标禁用,导入即见)+ 透出错误。
/// **人工探针**:钉住指定账号发一次**最小** chat,看上游到底出不出词。
///
/// 为什么需要它:`/quota` 的 `verified:true` 只证明**控制面**凭据活着,`/models` 只证明
/// 目录里有这个模型 —— 两者都不代表数据面能出词。实测过「有额度、有目录,一发 chat
/// 恒 `ModelNotAvailable`」的号。判定一个停用号能不能复活,只有真的收到 delta 才算数。
///
/// 风控约束(见 memory caio-kiro-key-suspend-lesson):`max_tokens=16`、单轮 "hi"、
/// 非流式语义下只读到首个文本 delta 就断流,走该账号自己的出口。**调用方必须串行 + 限速**,
/// 短时高频 chat 验号历史上直接导致过 `TEMPORARILY_SUSPENDED`。
///
/// **不上报账号健康**:这是人工动作,失败不该计入与真实流量共用的失败池/冷却,
/// 否则批量探测会把好号探成 `too_many_failures`(与 `/quota` 的取舍一致)。
async fn probe_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> axum::response::Response {
    use futures::StreamExt;

    let model = q
        .get("model")
        .map(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("claude-haiku-4.5")
        .to_string();

    let Some(account) = st.scheduler.account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"replied": false, "account_id": id})),
        )
            .into_response();
    };
    // 与配额探测共用信号量:人工探针不突破既有上游压力边界。
    let _permit = match st.quota_sem.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"replied": false, "account_id": id})),
            )
                .into_response();
        }
    };

    let account = match st.ensure_credentialed(account).await {
        Ok(a) => a,
        Err(e) => {
            return Json(serde_json::json!({
                "replied": false, "account_id": id, "model": model,
                "stage": "credential", "error_kind": format!("{:?}", e.kind),
                "error": e.message.clone(),
            }))
            .into_response();
        }
    };
    let account = st.ensure_profile_arn(account).await;

    let req = gw_core::provider::ChatRequest::from_anthropic_body(serde_json::json!({
        "model": model,
        "max_tokens": 16,
        "stream": true,
        "messages": [{"role": "user", "content": "hi"}],
    }));
    let ctx = gw_core::provider::CallCtx {
        account: account.clone(),
        session_id: format!("probe-{id}"),
        cache_key: format!("probe-{id}"),
    };

    let started = std::time::Instant::now();
    // 定频准入:人工探针也是一次真实的上游调用,同样受有效 RPM 上限约束(含暖机)。
    // 达限就如实报"被定频拦住"、绝不硬发 —— 探一个已接近上限的号把它推过阈值,
    // 正是定频闸门要防的事。探针无分组上下文,按 default_rank 口径(低频人工操作,
    // 取舍见 effective_rpm_limit 注释)。
    if !st.scheduler.note_upstream_call(&ctx.account.account_id, None) {
        return Json(serde_json::json!({
            "replied": false, "account_id": id, "model": model,
            "stage": "rpm_limited",
            "error": "该号已达有效 RPM 上限(含暖机),探针未发出",
        }))
        .into_response();
    }
    let stream = match st.provider.chat(req, &ctx).await {
        Ok(s) => s,
        Err(e) => {
            return Json(serde_json::json!({
                "replied": false, "account_id": id, "model": model,
                "stage": "connect", "error_kind": format!("{:?}", e.kind),
                "error": e.message.clone(),
            }))
            .into_response();
        }
    };

    // 收到首个文本 delta 即判定"能出词"并断流 —— 不把 16 个 token 读完,少烧一点是一点。
    let mut stream = stream;
    let mut text = String::new();
    let mut err: Option<(String, String)> = None;
    let deadline = tokio::time::sleep(std::time::Duration::from_secs(90));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => {
                err = Some(("Timeout".into(), "探针 90s 未收到任何内容".into()));
                break;
            }
            item = stream.next() => {
                match item {
                    None => break,
                    Some(Err(e)) => {
                        err = Some((format!("{:?}", e.kind), e.message.clone()));
                        break;
                    }
                    Some(Ok(gw_core::provider::StreamItem::Usage(_))) => {}
                    // 探针只认首个文本即断流,掐流信号与探针判定无关,忽略。
                    Some(Ok(gw_core::provider::StreamItem::UpstreamCut)) => {}
                    Some(Ok(gw_core::provider::StreamItem::Sse(ev))) => {
                        if let Some(t) = ev
                            .data
                            .get("delta")
                            .and_then(|d| d.get("text"))
                            .and_then(|t| t.as_str())
                        {
                            text.push_str(t);
                            if !text.trim().is_empty() {
                                break;
                            }
                        }
                    }
                }
            }
        }
    }
    drop(stream);

    let replied = !text.trim().is_empty();
    Json(serde_json::json!({
        "replied": replied,
        "account_id": id,
        "model": model,
        "elapsed_ms": started.elapsed().as_millis() as u64,
        // 只回显前 40 字符:够判断"是真回复还是拒答模板",又不至于把内容灌进日志。
        "text": text.chars().take(40).collect::<String>(),
        "error_kind": err.as_ref().map(|(k, _)| k.clone()),
        "error": err.as_ref().map(|(_, m)| m.clone()),
    }))
    .into_response()
}

async fn quota_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    if st.scheduler.account(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"verified": false, "account_id": id})),
        )
            .into_response();
    }
    // 与后台轮询/预热同走 quota_sem(审查 Architect#2/Minimalist#1):并发验活
    // 不突破 QUOTA_MAX_CONCURRENCY,不绕开既有上游压力边界。
    let _permit = match st.quota_sem.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"verified": false, "account_id": id})),
            )
                .into_response();
        }
    };
    match st.try_fetch_quota(&id).await {
        Ok(q) => {
            // 预检与取数之间账号可能被并发 /sync 移除(审查 Skeptic#3):try_fetch_quota
            // 对"不在本组"也回 Ok(None),与"上游无配额数据"撞语义——复查持有,不在则
            // 404 让 admin 继续问真正的持有方,而非误报 200"无配额"。
            if st.scheduler.account(&id).is_none() {
                return (
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({"verified": false, "account_id": id})),
                )
                    .into_response();
            }
            // 写共享配额缓存:表格积分列(/health runtime)立刻反映,且按 TTL
            // 节流后台轮询(刚验过的号本轮 sweep 自动跳过,不产生双倍流量)。
            st.quota_cache
                .lock()
                .insert(id.clone(), (q.clone(), std::time::Instant::now()));
            // verified=false + 200:账号可刷新但上游无配额数据(非硬失败,前端显示"—")。
            Json(serde_json::json!({
                "verified": q.is_some(),
                "account_id": id,
                "quota": quota_to_json(q),
            }))
            .into_response()
        }
        Err(e) => {
            // 只读探测失败**只对真死号惩罚**(审查三人共识):TokenInvalid(invalid_grant/
            // 403)→ report_failure 标 invalid_refresh_token,死号导入即现形;瞬时错误
            // (网络/5xx/429)只透传给前端展示,不计入与 chat 共用的失败池/冷却——
            // 否则上游抖动期间批量验活会把好号验成 too_many_failures。
            if matches!(e.kind, UpstreamErrorKind::TokenInvalid) {
                st.scheduler.report_failure(&id, e.kind);
            }
            // 失败也写"尝试时刻"(None),与后台轮询同一节流口径。
            st.quota_cache
                .lock()
                .insert(id.clone(), (None, std::time::Instant::now()));
            // 运维端点:导入对话框要按这段原文区分"死号"与"上游抖动"。
            admin_error_response(&e)
        }
    }
}

/// `POST /accounts/{id}/on-demand` —— 设置该号的超额(on-demand)额度上限。
///
/// body: `{"limit_usd": 50}`(美元整数);`0` 或 `null` = **关闭**超额。
///
/// ⚠️ 与 `/quota` 不同,这是**写**操作:它改上游账号的计费设置,开启后套餐用尽会产生
/// 真实费用。只由 admin 面板的显式运维动作触发,绝不在任何轮询路径上。
///
/// 成功后**顺带刷新配额缓存**:否则面板要等一个 TTL 才看到新上限,运维会以为没生效。
async fn on_demand_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
    body: Option<Json<serde_json::Value>>,
) -> axum::response::Response {
    if st.scheduler.account(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "account_id": id})),
        )
            .into_response();
    }
    if !st.provider.on_demand_supported() {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": {"message": format!("provider {} 不支持超额额度设置", st.provider.family())}
            })),
        )
            .into_response();
    }
    // limit_usd 缺省/null/0 → 关闭超额。负数或超 i32 的值直接拒:上游字段是 i32,
    // 静默截断会把「设 $50」变成别的数字。
    let raw = body
        .as_ref()
        .and_then(|Json(v)| v.get("limit_usd").cloned())
        .unwrap_or(serde_json::Value::Null);
    let limit_usd: Option<u32> = if raw.is_null() {
        None
    } else {
        match raw.as_i64() {
            Some(n) if (0..=i32::MAX as i64).contains(&n) => Some(n as u32),
            _ => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({
                        "error": {"message": "limit_usd 必须是 0..=2147483647 的整数(美元),0 或 null = 关闭超额"}
                    })),
                )
                    .into_response();
            }
        }
    };

    // 与配额查询同走 quota_sem:控制面对上游的并发压力边界只有这一处。
    let _permit = match st.quota_sem.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"ok": false, "account_id": id})),
            )
                .into_response();
        }
    };
    let Some(account) = st.scheduler.account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"ok": false, "account_id": id})),
        )
            .into_response();
    };
    let account = match st.ensure_credentialed(account).await {
        Ok(a) => a,
        Err(e) => return admin_error_response(&e),
    };
    if let Err(e) = st.provider.set_on_demand_limit(&account, limit_usd).await {
        // 上游拒绝(如未绑支付方式的 failed_precondition)按原文透出:运维要能区分
        // 「我们发错了」和「这号上游不允许」。写操作失败**不计失败池**:它与 chat
        // 可用性无关,一次计费设置被拒不该让号进冷却。
        return admin_error_response(&e);
    }
    // 回读一次(顺带刷新面板缓存)。回读失败不算设置失败:上游已经接受了。
    let quota = match st.try_fetch_quota(&id).await {
        Ok(q) => {
            st.quota_cache
                .lock()
                .insert(id.clone(), (q.clone(), std::time::Instant::now()));
            q
        }
        Err(e) => {
            tracing::debug!(account = %id, "超额设置成功但回读失败: {e}");
            None
        }
    };
    Json(serde_json::json!({
        "ok": true,
        "account_id": id,
        "quota": quota_to_json(quota),
    }))
    .into_response()
}

/// `POST /accounts/{id}/models` —— 用该账号拉一次上游**模型目录**并落库。
///
/// 目的是把 `rateMultiplier`(定价)与逐模型的 thinking 档位表从"代码里写死的印象"
/// 换成"上游当下的事实"。**全程只读**,与 `/quota` 同为控制面 GET,绝不发 chat
/// (见 no-chat-test-on-real-accounts 记忆)。
///
/// 仅本 worker 持有该账号时命中;不持有 → 404(admin 据此向其余 worker 续问)。
/// 与配额查询共用 `quota_sem`,不额外突破对上游的并发压力边界。
///
/// 目录是**全局**事实(不随账号变),所以落 `settings` 表单键覆盖;`fetched_by`
/// 记下是哪个号拉的,便于回溯不同订阅档位看到的模型集差异。
async fn models_account(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    // 预检只为快速 404,**不能**拿这里的副本去刷新 —— 见下方等锁后的复查。
    if st.scheduler.account(&id).is_none() {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"fetched": false, "account_id": id})),
        )
            .into_response();
    }
    let _permit = match st.quota_sem.clone().acquire_owned().await {
        Ok(p) => p,
        Err(_) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({"fetched": false, "account_id": id})),
            )
                .into_response();
        }
    };
    // ⚠️ 等信号量期间账号可能被 `/sync` 移走、由**另一个 worker 接管**。此时若拿等锁前
    // 的旧副本去 ensure_credentialed,两个进程会同时用同一枚 rolling refresh_token 刷新
    // —— 一方拿到新 token,另一方 invalid_grant,账号报废(禁用池里 8 个号就是这么死的)。
    // 故:**拿到 permit 之后重新取**,取不到就交还给真正的持有方。
    let Some(account) = st.scheduler.account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"fetched": false, "account_id": id})),
        )
            .into_response();
    };
    let account = match st.ensure_credentialed(account).await {
        Ok(a) => a,
        Err(e) => return admin_error_response(&e),
    };
    // 企业/IdC 号的控制面调用同样要求 profileArn,缺则先发现+持久化(与配额路径同款)。
    let account = st.ensure_profile_arn(account).await;
    match st.provider.model_catalog(&account).await {
        Ok(Some(catalog)) => {
            // 空目录**绝不落库**:上游/代理以 200 返回 `{}`、或字段名变更导致解析出空数组时,
            // 若照写会把上一份好快照(19 个模型 + 倍率)冲掉,而调用方还看到 persisted:true。
            // 宁可保留旧快照并如实说明没写。
            let model_count = catalog
                .get("models")
                .and_then(|m| m.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            let persisted = match (&st.store, serde_json::to_string(&catalog)) {
                _ if model_count == 0 => {
                    tracing::warn!("模型目录为空，拒绝落库以保护既有快照");
                    false
                }
                (Some(store), Ok(json)) => {
                    match store.upsert_kv(gw_store::SqliteStore::KEY_MODEL_CATALOG, &json) {
                        Ok(()) => true,
                        Err(e) => {
                            // 落库失败不算整个请求失败:目录本身已经拿到,先透出给调用方,
                            // 免得一次 DB 抖动就白打一次上游。
                            tracing::warn!("模型目录落库失败: {e}");
                            false
                        }
                    }
                }
                // 无库 = 降级模式(账号只来自 yaml 快照),此时只透出不落库。
                _ => false,
            };
            Json(serde_json::json!({
                "fetched": true,
                "persisted": persisted,
                "model_count": model_count,
                "account_id": id,
                "catalog": catalog,
            }))
            .into_response()
        }
        // provider 不支持(如 dario)——不是错误,如实说明。
        Ok(None) => Json(serde_json::json!({
            "fetched": false,
            "persisted": false,
            "account_id": id,
            "reason": "该 provider 不提供模型目录",
        }))
        .into_response(),
        Err(e) => {
            // ⚠️ **绝不 report_failure。** 配额路径那样做是因为它兼职"导入验活",要让死号
            // 立刻现形;而本端点只是拉一份**全局**目录,与"这个号好不好"无关。
            //
            // 若照抄配额的惩罚逻辑,会有一条报废健康账号的路:导入的号没有 `expires_at`,
            // `has_fresh_token` 误判 token 新鲜 → 不预刷 → 目录调用 401 → 按 TokenInvalid
            // 永久标 invalid_refresh_token。配额路径为此专门做了"强刷 + 强制发现 profileArn
            // 再重试一次"的兜底(见 try_fetch_quota),本路径没有,也不该为此把那套复制过来 ——
            // 一个管理员手点的目录刷新,失败就如实报错,不该有任何禁号副作用。
            admin_error_response(&e)
        }
    }
}

/// `GET /accounts/{id}/models/local` —— 该号的模型可用清单(**纯本地,绝不打上游**):
/// provider 静态目录 × (档位静态支持 `account_supports_model` − 已学 INVALID_MODEL_ID
/// 标记)。面板「查看模型」按钮的数据源。
///
/// 「可用」口径是**本地认知**:未观察到拒绝 ≠ 上游保证(半死号要真发一次才知道,
/// 上游真相用 `POST /accounts/{id}/models` 拉目录 + `/probe` 验)。本端点零风险,
/// 随便点。仅本 worker 持有该账号时命中;不持有 → 404(admin 据此向其余 worker 续问)。
async fn models_local(
    State(st): State<Arc<WorkerState>>,
    axum::extract::Path(id): axum::extract::Path<String>,
) -> axum::response::Response {
    let Some(account) = st.scheduler.account(&id) else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({"account_id": id})),
        )
            .into_response();
    };
    let catalog = match st.provider.list_models().await {
        Ok(c) => c,
        Err(e) => return admin_error_response(&e),
    };
    let marks = st.scheduler.model_marks_for(&id);
    let mark_secs: std::collections::HashMap<&str, u64> = marks
        .iter()
        .map(|m| (m.model.as_str(), m.remaining_secs))
        .collect();
    let mut in_catalog: std::collections::HashSet<&str> = std::collections::HashSet::new();
    let models: Vec<serde_json::Value> = catalog
        .iter()
        .map(|m| {
            in_catalog.insert(m.id.as_str());
            let supported = st.provider.account_supports_model(&account, &m.id);
            let mark = mark_secs.get(m.id.as_str()).copied();
            serde_json::json!({
                "id": m.id,
                "display_name": m.display_name,
                "supported": supported,
                "mark_remaining_secs": mark,
                "available": supported && mark.is_none(),
            })
        })
        .collect();
    // 目录外的标记(上游新模型/请求变体名打不中目录行)单独透出,不悄悄丢掉。
    let off_catalog_marks: Vec<serde_json::Value> = marks
        .iter()
        .filter(|m| !in_catalog.contains(m.model.as_str()))
        .map(|m| {
            serde_json::json!({"model": m.model, "remaining_secs": m.remaining_secs})
        })
        .collect();
    Json(serde_json::json!({
        "account_id": id,
        "models": models,
        "off_catalog_marks": off_catalog_marks,
    }))
    .into_response()
}
/// admin 导入账号后主动捅一下,消除"导入后 30s 内按号操作报无人持有"的窗口。
/// 无库(降级模式)/读库失败 → 503,调用方按 best-effort 忽略。
async fn sync_now(State(st): State<Arc<WorkerState>>) -> axum::response::Response {
    match st.sync_accounts_from_db().await {
        Some((added, removed)) => Json(serde_json::json!({
            "synced": true,
            "added": added,
            "removed": removed,
        }))
        .into_response(),
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({"synced": false})),
        )
            .into_response(),
    }
}

/// `GET /v1/models` —— 暴露 provider 的模型目录(Anthropic 线缆格式)。
/// provider 一处实现 `list_models`,框架在此映射成对外响应(写一次,各 provider 共享)。
async fn models(State(st): State<Arc<WorkerState>>) -> axum::response::Response {
    // created_at 占位:Kiro 不提供每模型创建时间(见 kiro-model-versioning 记忆),
    // 但 Anthropic /v1/models 条目带 created_at,严格类型客户端会校验——给个固定占位值
    // 以保证兼容(非真实日期,仅占位)。
    const MODEL_CREATED_AT: &str = "2025-01-01T00:00:00Z";
    // OpenAI 形状用的 unix 秒占位(与上面那个 ISO 串同一个时刻,只是另一种编码)。
    const MODEL_CREATED_UNIX: i64 = 1_735_689_600;
    // OpenAI 字段**只在开了 OpenAI 入口的 worker 上**追加。
    //
    // 这是个共享端点:kiro / dario / claude-subprocess 也走它。给所有家族无条件加字段
    // 就是为了迁就一种协议去改另一种协议的共享响应 —— 按 JSON Schema 严校验的客户端、
    // 或对响应体做快照/哈希的代理都会看到不兼容变化(对抗评审 Architect#2)。
    // 只有 cursor 的客户端会说 OpenAI,也只有它需要这几个键。
    let openai_shape = mount_openai_wire(st.provider.family());
    match st.provider.list_models().await {
        Ok(list) => {
            let data: Vec<serde_json::Value> = list
                .iter()
                .map(|m| {
                    let mut item = serde_json::json!({
                        "type": "model",
                        "id": m.id,
                        "display_name": m.display_name.clone().unwrap_or_else(|| m.id.clone()),
                        "created_at": MODEL_CREATED_AT,
                    });
                    if openai_shape {
                        if let Some(o) = item.as_object_mut() {
                            o.insert("object".into(), serde_json::json!("model"));
                            o.insert("created".into(), serde_json::json!(MODEL_CREATED_UNIX));
                            o.insert("owned_by".into(), serde_json::json!(st.provider.family()));
                        }
                    }
                    item
                })
                .collect();
            let first = list.first().map(|m| m.id.clone());
            let last = list.last().map(|m| m.id.clone());
            let mut body = serde_json::json!({
                "data": data,
                "has_more": false,
                "first_id": first,
                "last_id": last,
            });
            if openai_shape {
                if let Some(o) = body.as_object_mut() {
                    o.insert("object".into(), serde_json::json!("list"));
                }
            }
            Json(body).into_response()
        }
        Err(e) => {
            // 目录当前是本地生成、恒 Ok;一旦改成真实上游查询,原文只剩这一条日志路。
            tracing::warn!(kind = ?e.kind, "模型目录获取失败: {e}");
            upstream_error_response(&e)
        }
    }
}

/// 换号重试上限:按错误类别分档。
///
/// 良性冷却类错误(RateLimited/QuotaExhausted)自限流(命中即进冷却 → 后续请求的
/// `eligible_ids` 直接跳过该号、不再重复打上游)且不传播毒性,故解绑紧上限、允许沿
/// **优先级阶梯**下探到组内所有号(cap=total):高优先级层全限流/额度耗尽时,请求自动
/// 落到低优先级兜底池,而不是把限流错误抛回客户端。
///
/// 其余可换号错误(TokenInvalid/ServerError/Network/Other)仍守 `general_cap`(默认
/// max_switch_attempts=2)—— 2026-06 大面积封号雪崩正是这类「毒请求/高频重试」逐个打爆
/// 全池,绝不放开。(EmptyResponse/TemporarilyBlocked/BadRequest 更早被
/// `worth_switching_account()=false` 命中首个号即止,根本进不来这里。)
fn switch_cap(kind: UpstreamErrorKind, total: usize, general_cap: usize) -> usize {
    if matches!(
        kind,
        UpstreamErrorKind::RateLimited
            | UpstreamErrorKind::QuotaExhausted
            // ModelNotAvailable 同属良性(不禁号/不传播毒性):需沿组内遍历到有该模型的号,
            // 且每失败一次即把该 (号,模型) 标记不可用、下一跳自动跳过,不会重复打同一个号。
            | UpstreamErrorKind::ModelNotAvailable
    ) {
        total.max(1)
    } else {
        general_cap
    }
}

/// 解析 router 透传的**分组名**头。
///
/// - 头缺席 → `Ok(None)`:worker 用全量账号池(未分组请求 / 旧 router 滚动升级期间),
///   行为与本次重构之前逐字节相同。
/// - 头存在但为空 → `Err`:**必须拒绝而不是当成缺席**。当成缺席等于把一个本该受成员集
///   限制的请求以全量权限放行,且没有任何痕迹。头是内网自产的,畸形只可能是版本不匹配
///   或被篡改,两种情况都该硬失败。
fn parse_group_header(headers: &HeaderMap) -> Result<Option<String>, String> {
    let Some(raw) = headers.get(crate::GROUP_HEADER) else {
        return Ok(None);
    };
    let name = raw.to_str().map_err(|_| "分组头含非 ASCII 字符".to_string())?.trim();
    if name.is_empty() {
        return Err("分组头为空".into());
    }
    Ok(Some(name.to_string()))
}

/// 会话亲和键**按分组分命名空间**。
///
/// 必须这么做的原因:scheduler 的亲和表是全 worker 共用、按会话键索引的,而会话键由
/// **请求内容**派生(`Provider::affinity_key`),不含分组。两组若共用一个条目:某个正常
/// 客户的会话若钉在只属于正常组的号上,一个键名碰撞的低价请求会因该号不是自己组的成员
/// 而把它判为不合格 → 落进 `select_id` 的"primary 不可用 → 改选并当场转正、永不迁回"
/// 分支 → **永久改写正常客户的钉扎**,上游前缀缓存冷启动。分了命名空间,低价流量就只能
/// 动自己那一份条目。组名与会话键都不含 NUL,用 NUL 分隔。
///
/// `client` 非空时**再按客户 key 分一层**,见 [`affinity_scoped_by_client`]。
fn group_scoped_affinity_key(
    group: Option<&str>,
    client: Option<&str>,
    key: Option<String>,
) -> Option<String> {
    let key = key?;
    let scoped = match client.filter(|c| !c.is_empty()) {
        Some(c) => format!("{c}\u{0}{key}"),
        None => key,
    };
    Some(match group {
        Some(g) => format!("{g}\u{0}{scoped}"),
        None => scoped,
    })
}

/// 会话亲和键要不要**再按客户 key** 分一层命名空间。
///
/// ## 为什么 cursor 必须分(不分就是跨客户串话)
///
/// cursor 是**唯一有服务端会话续写**的上游:`ConvRegistry::phase_for` 在
/// (conversation_id, account_id) 都命中时返回 `Phase::Continuation`,于是本轮只发增量、
/// 由 Cursor 服务端接着上一轮往下说。而 `conversation_id` 来自
/// `gw_cursor::chat::affinity_key_from_body` = `hash(system + 第一条 user)` —— **纯内容派生,
/// 不含任何客户身份**。
///
/// 于是:同组两个不同 API key,只要 system 相同、开场那句话相同(通用 agent 提示词 +
/// "hello" 就够了),就会拿到同一个 conversation_id;亲和又把它们钉到同一个账号;
/// 两个条件一齐命中 → 后来那位客户的请求被当作前一位客户会话的**续写**发上去。
/// gw-cursor 自己的文档注释早就点了这个名:「第三条在 `CURSOR_STATEFUL=1` 下是跨用户串话」,
/// 而 `stateful` 的默认值是**开**(`CURSOR_STATEFUL` 只有显式 `=0` 才关)。
///
/// 这条路径在本次改动之前就存在(Anthropic 入口打 cursor worker 同样成立),但把入口开给
/// **NewAPI 这种多租户中转**正好凑齐了碰撞前提:同一个组下面挂着许多互不相识的客户。
/// 所以在这里补上客户维度 —— 不同客户永不共享 conversation_id,`Continuation` 也就永不跨客户。
///
/// ## 为什么只给 cursor
///
/// kiro / dario 没有服务端续写:每轮重传全量历史,共享的键只影响**前缀缓存命中**,
/// 泄不出上下文(kiro 发给上游的 conversationId 还额外按账号加盐)。给它们也分层会
/// 无谓打散主链路的缓存亲和,而主链路是生产主力 —— 不动。
fn affinity_scoped_by_client(family: &str) -> bool {
    family == "cursor"
}

/// 一条消息的 `content` 是否为空。
///
/// 空的判定与 Anthropic 官方 API 对齐:空串、纯空白、空数组、以及**含有空 text 块**的数组
/// 都会被上游拒。注意最后一种——`[{"type":"text","text":""}]` 看起来"有内容",但上游同样拒收。
///
/// 非文本块(image / tool_use / tool_result / thinking …)不参与判空:一条只带图片、
/// 没有文字的消息是合法的。
fn is_empty_message_content(content: Option<&serde_json::Value>) -> bool {
    match content {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::String(s)) => s.trim().is_empty(),
        Some(serde_json::Value::Array(blocks)) => {
            if blocks.is_empty() {
                return true;
            }
            // 任一空 text 块即非法(上游按块校验,不是按整条消息)。
            blocks.iter().any(|b| {
                b.get("type").and_then(|t| t.as_str()) == Some("text")
                    && b.get("text")
                        .and_then(|t| t.as_str())
                        .is_none_or(|t| t.trim().is_empty())
            })
        }
        // 其余类型(数字/布尔/对象)本就不是合法 content,交给上游报错,这里不拦。
        Some(_) => false,
    }
}

/// 校验 `messages` 数组里没有空 content 的消息。
///
/// 返回 `Err(人话错误)` —— 点名是第几条、什么角色,让客户端能直接定位;上游那句
/// "Improperly formed request." 什么都不说。
fn validate_message_contents(body: &serde_json::Value) -> Result<(), String> {
    let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) else {
        // 缺 messages / 类型不对:交给下游既有路径处理,这里只管空 content 这一件事。
        return Ok(());
    };
    for (i, m) in msgs.iter().enumerate() {
        if is_empty_message_content(m.get("content")) {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            return Err(format!(
                "messages[{i}] (role={role}) 的 content 为空。Anthropic 协议不允许空消息内容,\
                 上游会以 \"Improperly formed request\" 拒绝整个请求。请删除该条消息或填入非空内容。"
            ));
        }
    }
    Ok(())
}

/// 哪些 provider 家族挂 OpenAI 线缆入口。
///
/// 目前只有 `cursor`。单拎成函数是为了让这条策略**可被测试点名** —— 一句
/// `family == "cursor"` 埋在路由构造里,没人能证明它有没有被悄悄放宽。
fn mount_openai_wire(family: &str) -> bool {
    family == "cursor"
}

/// `POST /v1/messages` —— 原生 Anthropic 入口(全部非 cursor 流量走这条)。
async fn messages(
    State(st): State<Arc<WorkerState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    handle_chat(st, headers, body, Wire::Anthropic).await
}

/// `POST /v1/chat/completions` —— OpenAI ChatCompletions 入口(**仅 cursor 家族挂载**)。
///
/// 入站转成 Anthropic body 后走的就是 [`messages`] 那条链路,一个分支都不分:
/// 选号、会话亲和、租约、重试、计费、请求日志全部共用。
async fn chat_completions(
    State(st): State<Arc<WorkerState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    openai_entry(st, headers, body, gw_core::openai::chat_req::convert_request).await
}

/// `POST /v1/responses` —— OpenAI Responses 入口(**仅 cursor 家族挂载**)。
async fn responses(
    State(st): State<Arc<WorkerState>>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> axum::response::Response {
    openai_entry(st, headers, body, gw_core::openai::resp_req::convert_request).await
}

/// 两条 OpenAI 入口的共同前半段:解析 → 转换 → 交给 `handle_chat`。
///
/// **收 `Bytes` 而不是 `Json<Value>`**:后者的 extractor 在畸形 JSON 上直接返回 axum 的
/// 默认拒绝体(纯文本 / 非 OpenAI 形状),而那是在我方 handler 之前发生的,wire-aware
/// 的错误壳根本来不及套上 —— OpenAI SDK / NewAPI 按 `error` schema 解不出来
/// (对抗评审 Minimalist#4)。自己解就能保证**这条路径上每一个错误**都是 OpenAI 形状。
async fn openai_entry(
    st: Arc<WorkerState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
    convert: fn(
        &serde_json::Value,
    ) -> Result<gw_core::openai::Converted, gw_core::openai::ConvertError>,
) -> axum::response::Response {
    let parsed: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => {
            return openai_convert_error(&gw_core::openai::ConvertError::new(format!(
                "无效 JSON: {e}"
            )))
        }
    };
    match convert(&parsed) {
        Ok(c) => {
            // 静默降级留痕:客户端声明了我方没有的托管工具(web_search 等),
            // 我方丢掉后照常回答 —— 响应里几乎看不出来,不打日志就永远查不到。
            if !c.dropped_tools.is_empty() {
                tracing::warn!(
                    dropped = ?c.dropped_tools,
                    "OpenAI 入站请求声明了本通道不支持的托管工具,已丢弃(其余照常处理)"
                );
            }
            handle_chat(st, headers, c.body, c.wire).await
        }
        Err(e) => openai_convert_error(&e),
    }
}

/// 入站转换失败 → OpenAI 形状的 400。
///
/// **不占账号、不消耗配额**:与 `validate_message_contents` 同样的「在门口挡掉」思路,
/// 而且 `param` 会点名出问题的字段,客户端能直接定位。
fn openai_convert_error(e: &gw_core::openai::ConvertError) -> axum::response::Response {
    tracing::debug!("OpenAI 入站请求非法,本地拒绝: {e}");
    (
        StatusCode::BAD_REQUEST,
        Json(gw_core::openai::openai_error_body(
            400,
            &e.message,
            e.param.as_deref(),
            None,
        )),
    )
        .into_response()
}

async fn handle_chat(
    st: Arc<WorkerState>,
    headers: HeaderMap,
    body: serde_json::Value,
    wire: Wire,
) -> axum::response::Response {
    let req = ChatRequest::from_anthropic_body(body);
    // 入站结构校验:空 content 的消息上游必拒(2026-08-02 实测 失败样本命中 8/173、
    // 成功样本 0/400,零假阳性),且失败时上游回的是含糊的 "Improperly formed request",
    // 客户根本查不出是哪条消息的问题。在这里挡掉:不占账号、不消耗配额、报错点名下标。
    if let Err(msg) = validate_message_contents(&req.body) {
        tracing::debug!("入站请求结构非法,本地拒绝: {msg}");
        return error_response(wire, StatusCode::BAD_REQUEST, &msg);
    }
    // 请求日志(#③)采集:进入即计时。报文序列化(client/kiro)推迟到收尾的 blocking 任务里做,
    // 不在热路径(handler 入口)同步跑(审查 Skeptic#1)。
    let started_at = std::time::Instant::now();
    // 客户 key 归属:router 鉴权后经内网头透传(对外 Authorization 不到 worker)。
    // 请求所属分组:头由 router 依据 key 的分组生成、经内网白名单转发(客户端伪造的
    // 同名头在白名单转发时被丢弃)。头缺席 = 未分组请求 → 全量池,与重构前逐字节相同。
    // 畸形头**拒绝而非忽略**:忽略等于把受限分组的请求当全量放行,静默提权。
    let group = match parse_group_header(&headers) {
        Ok(g) => g,
        Err(msg) => {
            tracing::error!("分组头非法,拒绝请求: {msg}");
            // `error.type` 必填,少了它严格校验的客户端解析不了(见 anthropic_error_type)。
            return error_response(wire, StatusCode::INTERNAL_SERVER_ERROR, &msg);
        }
    };
    // 组名 → 成员视图。**查不到的组给空视图**(→ GroupEmpty/503),绝不回落全量池:
    // 那等于让一个成员边还没同步过来的分组瞬间拿到全部账号。
    let view = group
        .as_deref()
        .map(|g| st.group_views.read().get(g).cloned().unwrap_or_default());
    let client_key = headers
        .get(crate::CLIENT_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // 会话亲和键 = provider 派生的 conversationId(Kiro)。None → 无亲和按负载选号。
    // cursor 还要再按客户 key 分一层 —— 不分就是跨客户串话,见 `affinity_scoped_by_client`。
    let client_scope = affinity_scoped_by_client(st.provider.family()).then_some(client_key.as_str());
    let affinity_key = group_scoped_affinity_key(
        group.as_deref(),
        client_scope,
        st.provider.affinity_key(&req),
    );

    // 选号 + 发起 chat 的重试循环:token 失效(403/401)时刷新该号并对账号生命周期上报,
    // 换号重试;首包前的可重试错误最多走 total 个账号。committed(首包已出)后不重试。
    let total = st.scheduler.total().max(1);
    // 换号重试上限按错误类别分档(见 `switch_cap`):
    // - 良性冷却类(限速/额度耗尽):沿优先级阶梯下探到组内所有号(cap=total),落低优先级兜底池;
    // - 其余可换号错误:守 general_cap(默认 max_switch_attempts=2),防换号雪崩封号
    //   (2026-06 大面积封号根因正是让一个毒请求逐个打爆全池)。
    let general_cap = st.scheduler.max_switch_attempts().min(total).max(1);
    // **请求级重试总时限**。限流类错误的 switch_cap 是 `total`(全组),配上「排队等冷却」
    // 后每轮取号还各有一份 queue_wait 预算 —— 两者相乘,一个一直撞 429 的请求理论上能
    // 循环好几分钟。而下游 yapi 是 **300s 无 event 即中止**,超了客户只会看到一个
    // 更难查的中断。故在此处硬封顶:到点就把当前错误返回,让客户拿到明确结果。
    const RETRY_DEADLINE: std::time::Duration = std::time::Duration::from_secs(180);
    let retry_started = std::time::Instant::now();
    let mut attempts = 0;

    loop {
        attempts += 1;
        // 1. 按会话亲和取并发租约(持有到流结束)。合格账号须支持本次模型
        //    (FREE 订阅不支持 opus,过滤掉避免 403 误杀,对齐 kiro.rs supports_opus);
        //    并剔除已知对本模型返 INVALID_MODEL_ID 的号(区域/档位未上线),路由到有该
        //    模型的号——否则亲和会反复选中同一不支持的号(ModelNotAvailable 不禁号)死循环。
        let lease = match st
            .scheduler
            .acquire_in_group_until(
                affinity_key.as_deref(),
                |a| {
                    st.provider.account_supports_model(a, &req.model)
                        && !st.scheduler.is_model_unavailable(&a.account_id, &req.model)
                },
                view.as_ref(),
                // 「降层前先等高优先层的节流窗口」只在**请求开头的一小段时间窗**内允许。
                // 高优先层挂的是自购速刷号,被 429 节流那几百毫秒里降层 = 把量白送给低优先级
                // 兜底池(实测占比 14%)。但**必须封顶**:限流类错误的 switch_cap 是全组
                // (见 `switch_cap`),不封的话一个持续撞 429 的请求能在同一个号上来回弹到
                // 180s 总时限,省钱变成客户干等。窗口外照常降层 —— 结果与开关关闭时一致,
                // 绝不把错误抛给客户;窗口远小于 RETRY_DEADLINE,等待也就够不到那条硬线。
                //
                // ⚠️ 用**墙上时钟**而不是 `attempts`:后者对所有失败类别共用(凭证刷新失败、
                // ModelNotAvailable 都在消耗它),拿它当等待额度会出现「前两轮被无关错误吃掉、
                // 真撞上 429 时反而不许等」——正好是本开关要治的那个病。
                {
                    let w = st.scheduler.tier_hold_window();
                    !w.is_zero() && retry_started.elapsed() < w
                },
                // 请求级绝对截止:把 `RETRY_DEADLINE` 一路传进选号里的四条等待分支。
                // 不传的话每次 acquire 都重开一份本地预算,「两次换号各等一轮」就能
                // 越过 180s(对抗评审 [高])——而下游 yapi 是 300s 无 event 即中止,
                // 越线的代价是客户拿到一个比明确报错更难查的中断。
                Some(retry_started + RETRY_DEADLINE),
            )
            .await
        {
            Ok(l) => l,
            Err(e) => {
                // 穷尽 match(**不要改回二分 if**):新增变体时编译器会强制在这里做出
                // 显式决策,而不是默默落进 503 —— GroupEmpty 与 NoModelSupport 的
                // 状态码差异(可重试 vs 不可重试)正是靠这里区分的。
                let code = match e {
                    // 客户侧可解(换模型/升级订阅),重试无用 → 400。
                    scheduler::AcquireError::NoModelSupport => StatusCode::BAD_REQUEST,
                    // 池子/分组的配置或运行时状态,稍后可恢复 → 503。
                    scheduler::AcquireError::GroupEmpty
                    | scheduler::AcquireError::AllDisabled
                    | scheduler::AcquireError::AllBusy
                    | scheduler::AcquireError::AllRpmLimited
                    | scheduler::AcquireError::Empty => StatusCode::SERVICE_UNAVAILABLE,
                };
                // 内部原因(哪一档耗尽 / 组里压根没成员)只进日志:它描述的是账号池形态,
                // 对客户既没用又泄底。客户端只拿到"换模型"还是"稍后重试"。
                tracing::warn!(
                    group = %group.as_deref().unwrap_or("-"),
                    model = %req.model,
                    "选号失败: {e}"
                );
                return error_response(wire, code, e.client_message());
            }
        };
        let account_id = lease.account_id().to_string();
        // 选号时的 suspend 世代快照:本 lease 后续所有成功/失败上报都带它,
        // 调度器据此丢弃「状态转换前就在途」的迟到回声(见 suspend_gen)。
        let suspend_gen = lease.suspend_gen;

        // 2. 确保该号有未过期 access_token(按需刷新,带 expires_at 检查 + 单飞)。
        //    刷新失败按 kind 处理:invalid_grant(TokenInvalid)永久禁用;transient
        //    (网络/5xx/429)只记 transient 失败、换号重试,不永久打死健康号。
        let account = match st.ensure_credentialed(lease.account.clone()).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(account = %account_id, kind = ?e.kind, "凭证刷新失败: {e}");
                st.scheduler.report_failure_with_gen(&account_id, e.kind, suspend_gen);
                drop(lease);
                if !e.kind.worth_switching_account()
                    || retry_started.elapsed() >= RETRY_DEADLINE
                    || attempts >= switch_cap(e.kind, total, general_cap)
                {
                    return upstream_error_response_wire(wire, &e);
                }
                continue;
            }
        };
        // 企业/IdC 号确保带 profileArn(缺则 ListAvailableProfiles 发现+持久化);
        // social/builderid/已有值直接短路。发现失败不阻断,让上游自然报 400。
        let account = st.ensure_profile_arn(account).await;

        let ctx = CallCtx {
            account,
            // session_id / cache_key 用亲和键(= conversationId),与 cache_sim 同源。
            session_id: affinity_key.clone().unwrap_or_default(),
            cache_key: affinity_key.clone().unwrap_or_default(),
        };

        // 3. 发起上游 chat。首包前错误(committed=false)可处理:
        //    - TokenInvalid(403):access_token 失效 → **同号强制刷新一次并重试同号**
        //      (refresh-then-retry,不立刻换号,保住会话亲和/缓存);刷新或重试仍失败才换号。
        //    - BadRequest:请求本身问题,换号无益,直接返回。
        //    - Overloaded(模型级容量不足):**同号退避重试**(见 chat_with_overload_backoff),
        //      不换号、不记账号失败;退避用尽才透出 → 对外 529。
        //    - 其他可重试错误:上报失败、换号重试。
        match chat_with_overload_backoff(&st, &req, &ctx, &account_id, view.as_ref()).await {
            Ok(stream) => {
                // 服务端 web search:客户端声明了 `web_search_20250305` → 进执行回环
                // (反代自搜 + 注回),把首轮流当 round 1 续跑。其余流量字节一致走原路径。
                if let Some(spec) = crate::websearch::detect_web_search(&req.body) {
                    return finish_web_search_response(
                        st.clone(),
                        lease,
                        stream,
                        &req,
                        &client_key,
                        ctx,
                        spec,
                        started_at,
                        view.clone(),
                        wire,
                    )
                    .await;
                }
                return finish_response(
                    st.clone(),
                    lease,
                    stream,
                    &req,
                    &client_key,
                    ctx.account.clone(),
                    started_at,
                    wire,
                )
                .await;
            }
            // API Key 403:ksk_ 无可刷新,同号刷新是空操作、retry 同 key 必再 403 且放大上游
            // (审查 Skeptic#3/Architect#2)。故 apikey 的 403 **不走**同号刷新重试,直接落到下方
            // 通用失败分支:report_failure(TokenInvalid) 禁用该号 + 换号(误伤可 admin reset 复活)。
            Err(e)
                if e.kind == UpstreamErrorKind::TokenInvalid
                    && !gw_kiro::machine_id::is_api_key_credential(&ctx.account) =>
            {
                tracing::info!(account = %account_id, "chat 403 token 失效,尝试同号刷新后重试");
                // 传入这枚被拒的 access_token:并发 403 时,只有第一个进锁者真刷,
                // 其余发现 token 已被换掉即复用,避免 N 次重复上游刷新(审查 Skeptic#2)。
                let rejected = ctx.account.extra_str("access_token").map(|s| s.to_string());
                match st
                    .refresh_after_rejection(ctx.account.clone(), rejected.as_deref())
                    .await
                {
                    Ok(refreshed) => {
                        let retry_ctx = CallCtx {
                            account: refreshed,
                            session_id: affinity_key.clone().unwrap_or_default(),
                            cache_key: affinity_key.clone().unwrap_or_default(),
                        };
                        // 定频准入:这是同一个 lease 上的**第二次**上游调用,不拦就会超频
                        // (暖机 RPM=2 的号一次刷新重试就能发出第 3 次)。达限则**不硬发**:
                        // 该号没犯错(rt 刚证有效),绝不上报失败(那会按 TokenInvalid 永久禁用),
                        // 直接换号;预算/时限用尽则把原始错误透给客户。
                        if !st
                            .scheduler
                            .note_upstream_call(&retry_ctx.account.account_id, view.as_ref())
                        {
                            tracing::info!(account = %account_id,
                                "刷新后重试被 RPM 准入拦住(含暖机),换号");
                            drop(lease);
                            if retry_started.elapsed() >= RETRY_DEADLINE
                                || attempts >= switch_cap(e.kind, total, general_cap)
                            {
                                return upstream_error_response_wire(wire, &e);
                            }
                            continue;
                        }
                        match st.provider.chat(req.clone(), &retry_ctx).await {
                            Ok(stream) => {
                                if let Some(spec) =
                                    crate::websearch::detect_web_search(&req.body)
                                {
                                    return finish_web_search_response(
                                        st.clone(),
                                        lease,
                                        stream,
                                        &req,
                                        &client_key,
                                        retry_ctx,
                                        spec,
                                        started_at,
                                        view.clone(),
                                        wire,
                                    )
                                    .await;
                                }
                                return finish_response(
                                    st.clone(),
                                    lease,
                                    stream,
                                    &req,
                                    &client_key,
                                    retry_ctx.account.clone(),
                                    started_at,
                                    wire,
                                )
                                .await;
                            }
                            Err(e2) => {
                                // 刷新已成功(rt 证明有效)。若重试仍是 403 TokenInvalid,极可能
                                // profileArn 套错——付费 builderid 号被免费层固定共享 ARN 短路、
                                // 拿不到自己的 profile。镜像配额路径:强制发现真实 ARN 持久化后用真
                                // ARN 再重试一次,成功即救回(治「导入即入活跃池、客户 chat 抢在验活
                                // force_discover 前命中、用错 ARN 403」的竞态)。
                                let e2 = if e2.kind == UpstreamErrorKind::TokenInvalid {
                                    match st
                                        .discover_paid_profile_arn(
                                            &retry_ctx.account,
                                            "profileArn(chat 403 兜底强制发现)",
                                        )
                                        .await
                                    {
                                        Some(healed) => {
                                            let heal_ctx = CallCtx {
                                                account: healed,
                                                session_id: affinity_key
                                                    .clone()
                                                    .unwrap_or_default(),
                                                cache_key: affinity_key
                                                    .clone()
                                                    .unwrap_or_default(),
                                            };
                                            // 定频准入:profileArn 修复后的验证调用同样是真实
                                            // 上游调用;达限(含暖机)则不硬发。
                                            // ⚠️ 被闸门拦住 ≠ 修复失败(对抗审查复审 [高]):
                                            // ARN 已修好并持久化,号是好的 —— 绝不能把 e2 透给
                                            // 下方的 TokenInvalid 上报(那会把刚救回的号永久
                                            // 误禁 invalid_refresh_token;暖机一期 RPM=2 下这是
                                            // 确定性路径)。照 token 刷新分支语义:释放 lease
                                            // 换号,预算/时限用尽才透原始错误。
                                            if !st.scheduler.note_upstream_call(
                                                &heal_ctx.account.account_id,
                                                view.as_ref(),
                                            ) {
                                                tracing::info!(account = %account_id,
                                                    "profileArn 修复验证被 RPM 准入拦住(含暖机),换号(不上报失败)");
                                                drop(lease);
                                                if retry_started.elapsed() >= RETRY_DEADLINE
                                                    || attempts
                                                        >= switch_cap(e2.kind, total, general_cap)
                                                {
                                                    return upstream_error_response_wire(wire, &e2);
                                                }
                                                continue;
                                            }
                                            match st.provider.chat(req.clone(), &heal_ctx).await {
                                                Ok(stream) => {
                                                    if let Some(spec) =
                                                        crate::websearch::detect_web_search(
                                                            &req.body,
                                                        )
                                                    {
                                                        return finish_web_search_response(
                                                            st.clone(),
                                                            lease,
                                                            stream,
                                                            &req,
                                                            &client_key,
                                                            heal_ctx,
                                                            spec,
                                                            started_at,
                                                            view.clone(),
                                                            wire,
                                                        )
                                                        .await;
                                                    }
                                                    return finish_response(
                                                        st.clone(),
                                                        lease,
                                                        stream,
                                                        &req,
                                                        &client_key,
                                                        heal_ctx.account.clone(),
                                                        started_at,
                                                        wire,
                                                    )
                                                    .await;
                                                }
                                                // 带真 ARN 仍失败 → 真死号/真封禁,携 e3 继续统一失败处理。
                                                Err(e3) => e3,
                                            }
                                        }
                                        None => e2,
                                    }
                                } else {
                                    e2
                                };
                                // 刷新成功后仍失败:上报**真实** e2.kind + 换号。heal 已把可救的
                                // profileArn 套错救回(救回则上面直接 return);走到这里=该号确实当前
                                // 不能服务——e3(带真 ARN 仍 403)是最强死号信号、或非付费/发现失败,
                                // 一律保留原分类语义(TokenInvalid→invalid_refresh_token 永久禁用),
                                // 不弱化死号识别。注:「刷新成功⇒rt 有效」只证认证有效,不代表 entitlement
                                // 未被服务端撤销,故不能据此把 TokenInvalid 一律降级(对抗审查 HIGH)。
                                tracing::warn!(account = %account_id, kind = ?e2.kind, "刷新后重试仍失败: {e2}");
                                st.scheduler.report_failure_with_gen(&account_id, e2.kind, suspend_gen);
                                drop(lease);
                                if !e2.kind.worth_switching_account()
                                    || retry_started.elapsed() >= RETRY_DEADLINE
                                    || attempts >= switch_cap(e2.kind, total, general_cap)
                                {
                                    return upstream_error_response_wire(wire, &e2);
                                }
                                continue;
                            }
                        }
                    }
                    Err(re) => {
                        // 刷新失败:invalid_grant→永久禁用;transient→换号重试。
                        tracing::warn!(account = %account_id, kind = ?re.kind, "同号刷新失败: {re}");
                        st.scheduler.report_failure_with_gen(&account_id, re.kind, suspend_gen);
                        drop(lease);
                        if !re.kind.worth_switching_account()
                            || retry_started.elapsed() >= RETRY_DEADLINE
                            || attempts >= switch_cap(re.kind, total, general_cap)
                        {
                            return upstream_error_response_wire(wire, &re);
                        }
                        continue;
                    }
                }
            }
            Err(e) => {
                let kind = e.kind;
                tracing::warn!(account = %account_id, kind = ?kind, "chat 失败: {e}");
                st.scheduler.report_failure_with_gen(&account_id, kind, suspend_gen);
                // 该号对本模型不可用(INVALID_MODEL_ID):记 (号,模型) 不可用,后续选号跳过它、
                // 路由到有该模型的号(该号**不禁用**,仍服务其它模型)。
                if kind == UpstreamErrorKind::ModelNotAvailable {
                    st.scheduler.mark_model_unavailable(&account_id, &req.model);
                }
                drop(lease);
                if !kind.worth_switching_account()
                    || retry_started.elapsed() >= RETRY_DEADLINE
                    || attempts >= switch_cap(kind, total, general_cap)
                {
                    // 终态失败(首包前):落一条失败请求日志,让"失败"筛选能看到上游 400/耗尽
                    // (生产 400 风暴正是此类)。无 usage/ttfb;detach 到 blocking 线程池。
                    // 与客户端实收状态码同源(见 upstream_status):
                    // BadRequest/ModelNotAvailable→400、Overloaded→529、其余 502。
                    let status = Some(upstream_status(kind).as_u16() as i64);
                    spawn_request_log_blocking(
                        &st,
                        req.clone(),
                        Some(ctx.account.clone()),
                        client_key.clone(),
                        req.stream,
                        false,
                        status,
                        Some(format!("{kind:?}")),
                        Some(started_at.elapsed().as_millis() as i64),
                        None,
                        None,
                        ResponseLog::None, // 首包前失败:无模型回复。
                        st.provider.family(),
                    );
                    return upstream_error_response_wire(wire, &e);
                }
                continue;
            }
        }
    }
}

/// `Overloaded` 的同号退避梯度(毫秒)。长度即重试次数上限。
///
/// 只覆盖"上游容量秒级抖动"这一种场景,故总等待封顶约 3s——再长客户端体感就不如直接
/// 吐 529 让它自己重试。2026-07-25 实测尖峰持续约 10 分钟但**单点**空窗在秒级。
const OVERLOAD_BACKOFF_MS: &[u64] = &[250, 750, 2_000];

/// 0~40% 的正向抖动。**必需**:并发请求会在同一波容量抖动里齐刷刷失败,无抖动会同步重撞,
/// 把重试变成新的尖峰。熵取 uuid v4 的一个字节(workspace 已依赖 uuid+fast-rng,不为此引入 rand)。
fn jittered(base_ms: u64) -> std::time::Duration {
    // 先把熵字节(0..=255)折成百分比 0..=40,再按百分比加成——直接拿字节做系数容易算错倍率。
    let pct = uuid::Uuid::new_v4().as_bytes()[0] as u64 * MAX_JITTER_PCT / 255;
    std::time::Duration::from_millis(base_ms + base_ms * pct / 100)
}

/// 抖动上限(百分比)。测试 `overload_backoff_is_bounded_and_jittered_upward` 据此断言区间。
const MAX_JITTER_PCT: u64 = 40;

/// 发一次上游 chat,并按**模型级过载窗口**校正错误类型(见 [`correct_overload_kind`])。
///
/// 校正放在这一个点上,让下游(退避判定、`report_failure`、状态码、请求日志)全部看到
/// 同一个已校正的 kind,不必各自重算。
async fn chat_once_corrected(
    st: &Arc<WorkerState>,
    req: &ChatRequest,
    ctx: &CallCtx,
) -> Result<gw_core::provider::ChatStream, gw_core::error::UpstreamError> {
    match st.provider.chat(req.clone(), ctx).await {
        Ok(s) => Ok(s),
        Err(mut e) => {
            e.kind = correct_overload_kind(&st.scheduler, &req.model, e.kind);
            Err(e)
        }
    }
}

/// 按模型级过载窗口校正错误类型。
///
/// - 收到**显式**过载(`Overloaded`,即上游给了 `MODEL_TEMPORARILY_UNAVAILABLE`)→ 开窗并原样返回。
/// - 窗口内的通用 `ServerError` → 升级为 `Overloaded`。依据:2026-07-25 实测 176 条
///   `reason:null` 的通用 5xx 中 **84.7% 与显式过载落在同一分钟**,是同一波容量抖动;
///   上游只是有时不填 `reason`。
/// - 其余一律原样返回。**窗口外绝不升级**——重分类必须有上游显式信号背书,不靠猜。
///
/// 只在**首包前**的错误上调用(2026-07-25 观测到的 276 次 5xx 全部 `ttfb=NULL`,即都发生在
/// 首包前)。流中途冒出的 5xx 仍走 `finish_response` 的原路径,不做校正。
fn correct_overload_kind(
    scheduler: &AccountScheduler,
    model: &str,
    kind: UpstreamErrorKind,
) -> UpstreamErrorKind {
    if kind == UpstreamErrorKind::Overloaded {
        scheduler.mark_model_overloaded(model);
        return kind;
    }
    if kind == UpstreamErrorKind::ServerError && scheduler.is_model_overloaded(model) {
        tracing::info!(
            model = %model,
            "通用 5xx 落在该模型的过载窗口内,按过载处理(不惩罚账号/不换号)"
        );
        return UpstreamErrorKind::Overloaded;
    }
    kind
}

/// 发起上游 chat,遇 [`UpstreamErrorKind::Overloaded`] 时**在同一个号上**退避重试。
///
/// 与上方 TokenInvalid 的 refresh-then-retry 同构:握着同一枚 lease、同一个 `ctx` 重发。
/// 这样做的三个理由:
/// 1. 上游说的是"这个**模型**现在没容量",与用哪个号无关——换号打的还是同一个模型端点。
/// 2. 保住会话 cache 亲和。实测一次 opus-5 请求 `cache_read` 可达 10.7 万 token,换号全部重算。
/// 3. **爆炸半径 = 1 个号**,比换号重试(默认 2 个号)更小,故与 2026-06 防雪崩的
///    `max_switch_attempts` 硬上限不冲突,**无需放开后者**。
///
/// 退避用尽仍过载 → 原样返回 `Err`,由调用方走终态路径(`worth_switching_account()=false`
/// → 不换号,直接对外 529)。
async fn chat_with_overload_backoff(
    st: &Arc<WorkerState>,
    req: &ChatRequest,
    ctx: &CallCtx,
    account_id: &str,
    // 成员视图:追加调用的 RPM 准入与选号同口径(见 note_upstream_call)。
    view: Option<&scheduler::GroupView>,
) -> Result<gw_core::provider::ChatStream, gw_core::error::UpstreamError> {
    let mut last = match chat_once_corrected(st, req, ctx).await {
        Ok(s) => return Ok(s),
        Err(e) if e.kind.worth_same_account_backoff() => e,
        Err(e) => return Err(e),
    };
    for (i, base_ms) in OVERLOAD_BACKOFF_MS.iter().enumerate() {
        let wait = jittered(*base_ms);
        tracing::info!(
            account = %account_id, model = %req.model, attempt = i + 1,
            wait_ms = wait.as_millis() as u64,
            "上游模型过载,同号退避重试(不换号/不记账号失败)"
        );
        tokio::time::sleep(wait).await;
        // 定频准入在退避**完成之后、发出之前**(对抗审查复审 [中]):先记账再睡的话,
        // 任务在睡眠期被取消会留下从未发送的假 hit,且滑动窗口按预留时刻过期,
        // 窗口边界会提前放出名额。首发名额已在 select_id 预留,这里拦的是追加调用;
        // 检查+记账原子(见 note_upstream_call)。达限(含暖机)不再硬发:按退避用尽
        // 处理,返回最近一次过载错误 → 对外 529(客户端自动重试),与预算耗尽显同一路径。
        if !st.scheduler.note_upstream_call(account_id, view) {
            tracing::info!(account = %account_id, model = %req.model,
                "过载退避重试被 RPM 准入拦住(含暖机),按退避用尽处理");
            return Err(last);
        }
        match chat_once_corrected(st, req, ctx).await {
            Ok(s) => {
                tracing::info!(account = %account_id, model = %req.model, attempt = i + 1,
                    "同号退避重试成功");
                return Ok(s);
            }
            // 退避期间错误类型可能变(如容量恢复但换成 403):非过载即刻透出,别用过载预算硬打。
            Err(e) if e.kind.worth_same_account_backoff() => last = e,
            Err(e) => return Err(e),
        }
    }
    Err(last)
}

/// 上游错误 → 对外 HTTP 状态码。**唯一映射点**:响应与请求日志都走它,杜绝两处漂移
/// (日志里记 502 而客户端实收 529 会让排查彻底跑偏)。
///
/// - `BadRequest` / `ModelNotAvailable` → 400(客户端可解:改请求 / 换模型 / 升级订阅)
/// - `Overloaded` → **529**(Anthropic 官方过载语义:Claude Code 与各家 SDK 据此自动重试,
///   且不会把渠道判死。语义上这也确实不是"坏网关",是上游没容量)
/// - 其余 → 502
#[cfg(test)]
mod error_shape_tests {
    use super::*;
    use gw_core::error::{UpstreamError, UpstreamErrorKind as K};

    /// ⭐ 2026-08-07 事故:除 Overloaded 外都不发 `error.type`,按 schema 严格校验的
    /// 客户端(opencode/zod)连错误都解析不了,真 message 被吞掉。
    /// **每一种 kind 都必须给出非空的 `error.type`。**
    #[test]
    fn every_error_kind_emits_a_string_error_type() {
        for kind in [
            K::TokenInvalid, K::RateLimited, K::TemporarilyBlocked, K::QuotaExhausted,
            K::Network, K::ServerError, K::Overloaded, K::BadRequest,
            K::ModelNotAvailable, K::EmptyResponse, K::Other,
        ] {
            let e = UpstreamError::new(kind, "x");
            for (path, body) in [
                ("http", upstream_error_payload(&e)),
                ("sse", sse_error_payload(&e)),
            ] {
                let ty = body.pointer("/error/type").and_then(|v| v.as_str());
                assert!(
                    ty.is_some_and(|t| !t.is_empty()),
                    "{path} 路径 {kind:?} 缺 error.type:{body}"
                );
                assert!(body.pointer("/error/message").is_some_and(|m| m.is_string()));
                assert_eq!(body["type"], "error");
            }
        }
    }

    /// type 必须与状态码自洽 —— 两者打架会让客户端 SDK 的重试判断和状态码判断冲突。
    #[test]
    fn error_type_agrees_with_status_code() {
        assert_eq!(anthropic_error_type(K::BadRequest), "invalid_request_error");
        assert_eq!(anthropic_error_type(K::ModelNotAvailable), "invalid_request_error");
        assert_eq!(anthropic_error_type(K::Overloaded), "overloaded_error");
        // 选号失败那条路自己算状态码,走 error_type_for_status
        assert_eq!(error_type_for_status(StatusCode::SERVICE_UNAVAILABLE), "overloaded_error");
        assert_eq!(error_type_for_status(StatusCode::BAD_REQUEST), "invalid_request_error");
        assert_eq!(error_type_for_status(StatusCode::BAD_GATEWAY), "api_error");
        // 502 那一大类:api_error(而不是 authentication_error —— 那会让客户以为
        // 是自己的 key 有问题,而坏的是我们的上游账号)。
        for k in [K::TokenInvalid, K::EmptyResponse, K::Network, K::QuotaExhausted] {
            assert_eq!(anthropic_error_type(k), "api_error", "{k:?}");
        }
    }

    /// 上游自己发来的 error 载荷若没带 type,也要补上。
    #[test]
    fn sanitizing_upstream_payload_fills_missing_type() {
        let out = sanitize_upstream_error_payload(&serde_json::json!({
            "type":"error","error":{"message":"上游原文含指纹"}
        }));
        assert_eq!(out.pointer("/error/type").unwrap(), "api_error");
        assert_ne!(out.pointer("/error/message").unwrap(), "上游原文含指纹");
        // 带 type 的保留原 type(客户端按它判重试语义,不能动)。
        let kept = sanitize_upstream_error_payload(&serde_json::json!({
            "type":"error","error":{"type":"rate_limit_error","message":"raw"}
        }));
        assert_eq!(kept.pointer("/error/type").unwrap(), "rate_limit_error");
    }
}

fn upstream_status(kind: UpstreamErrorKind) -> StatusCode {
    match kind {
        UpstreamErrorKind::BadRequest | UpstreamErrorKind::ModelNotAvailable => {
            StatusCode::BAD_REQUEST
        }
        UpstreamErrorKind::Overloaded => overloaded_status(),
        _ => StatusCode::BAD_GATEWAY,
    }
}

/// `UpstreamErrorKind` → Anthropic 协议的 `error.type` 常量。
///
/// ## 为什么这个字段**必须**有(2026-08-07 实测事故)
///
/// Anthropic 的错误体形状是 `{"type":"error","error":{"type":..., "message":...}}` ——
/// `error.type` 是**必填**。早先除 `Overloaded` 外都只发 `message`,于是按 schema
/// 严格校验的客户端(opencode 用 zod)**连错误都解析不了**:
///
/// ```text
/// Type validation failed: {"error":{"message":"服务未返回内容,请重试"},"type":"error"}
/// Error message: [{ "expected":"string", "path":["error","type"],
///                   "message":"Invalid input: expected string, received undefined" }]
/// ```
///
/// 客户看到的是一句 zod 报错,真正的 message 被吞掉 —— 排查成本极高,而且**所有
/// provider 都受影响**(不只 cursor)。
///
/// 取值**跟着 [`upstream_status`] 的状态码走**,不另立一套:type 与 status 不一致
/// 会让客户端 SDK 的重试判断和状态码判断打架。
/// - 400 → `invalid_request_error`
/// - 529 → `overloaded_error`
/// - 502 → `api_error`(Anthropic taxonomy 里没有 bad_gateway;`api_error` 的语义
///   正是"服务端出问题,重试可能有用",与 502 一致,且**不会**让客户端以为是自己的
///   key 或请求有问题)
fn anthropic_error_type(kind: UpstreamErrorKind) -> &'static str {
    error_type_for_status(upstream_status(kind))
}

/// 状态码 → Anthropic `error.type`。
///
/// 单独拆出来是因为有些错误路径(选号失败)自己算状态码、手里没有 `UpstreamErrorKind`。
/// **type 必须跟着 status 走**:两者打架会让客户端 SDK 的重试判断与状态码判断冲突。
fn error_type_for_status(code: StatusCode) -> &'static str {
    match code.as_u16() {
        400 => "invalid_request_error",
        401 => "authentication_error",
        403 => "permission_error",
        404 => "not_found_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        // 503(池子暂时没号)与 529(模型级过载)都是"容量,稍后重试"。
        // Anthropic taxonomy 里没有 service_unavailable,`overloaded_error` 是语义最近的,
        // 而且客户端 SDK 对它的处置正是退避重试。
        503 | 529 => "overloaded_error",
        // 502 及其他 5xx:`api_error` = 服务端出问题、可重试,且**不指认**客户的
        // key 或请求有问题(那会让 Claude Code 之类去要求重新登录)。
        _ => "api_error",
    }
}

/// 把 [`UpstreamError`] 映射为对外 HTTP 响应(状态码见 [`upstream_status`])。
///
/// **message 走 `client_message()` 而不是 `to_string()`**:后者带内部 kind 标签 +
/// 上游原始报文(接口名 `generateAssistantResponse`、reason 码 …),等于把渠道来源
/// 印在每条报错上。诊断原文在调用方的 `tracing` 里一字不少。
fn upstream_error_response(e: &gw_core::error::UpstreamError) -> axum::response::Response {
    let code = upstream_status(e.kind);
    (code, Json(upstream_error_payload(e))).into_response()
}

/// 对外错误体。**永远带 `error.type`**,见 [`anthropic_error_type`]。
fn upstream_error_payload(e: &gw_core::error::UpstreamError) -> serde_json::Value {
    serde_json::json!({
        "type": "error",
        "error": {
            "type": anthropic_error_type(e.kind),
            "message": e.client_message(),
        },
    })
}

/// [`upstream_error_response`] 的 wire-aware 版本。
///
/// `Wire::Anthropic` **原样调用旧函数**,逐字节不变 —— 这是生产主链路,不容形状漂移。
/// OpenAI 系只换外壳:状态码、中性文案、脱敏纪律全部沿用同一套。
fn upstream_error_response_wire(
    wire: Wire,
    e: &gw_core::error::UpstreamError,
) -> axum::response::Response {
    if !wire.is_openai() {
        return upstream_error_response(e);
    }
    let code = upstream_status(e.kind);
    (
        code,
        Json(gw_core::openai::openai_error_body(
            code.as_u16(),
            &e.client_message(),
            None,
            None,
        )),
    )
        .into_response()
}

/// 自算状态码的错误(入站校验失败、选号失败)→ wire-aware 响应体。
///
/// Anthropic 分支与手写 `json!({"type":"error", ...})` **完全等价**:
/// `error_type_for_status` 就是那些字面量的来源(400→invalid_request_error、
/// 500→api_error),换成它是为了让两种线缆共用同一张状态码→类型表,而不是各写一份。
fn error_response(wire: Wire, code: StatusCode, message: &str) -> axum::response::Response {
    if wire.is_openai() {
        return (
            code,
            Json(gw_core::openai::openai_error_body(
                code.as_u16(),
                message,
                None,
                None,
            )),
        )
            .into_response();
    }
    (
        code,
        Json(serde_json::json!({"type":"error","error":{
            "type": error_type_for_status(code), "message": message}})),
    )
        .into_response()
}

/// 流中硬错误 → 对外 SSE `error` 事件。
///
/// 与 [`upstream_error_response`] 同口径:**只发中性文案**。首包已出后状态码改不了,
/// message 是唯一出口 —— 越是这种时候越不能让它带上游指纹。原文在调用处落 `tracing`。
fn sse_error_event(e: &gw_core::error::UpstreamError) -> Event {
    Event::default().event("error").data(sse_error_payload(e).to_string())
}

/// [`sse_error_event`] 的载荷。单独拆出来只为可测:`Event` 没有取回 data 的公开接口。
fn sse_error_payload(e: &gw_core::error::UpstreamError) -> serde_json::Value {
    // 与 HTTP 那条路同口径 —— 包括**必须带 `error.type`**(见 anthropic_error_type)。
    upstream_error_payload(e)
}

/// 把**上游自己发来的** Anthropic 形状 error 载荷改写成中性版本。
///
/// 与 [`UpstreamError`] 那条路不同:这条路上错误已经是"合法 SSE 事件"了,provider 没把它
/// 转成 `Err`(dario 直透 Anthropic 流即如此),`fold_sse_to_message` 也会把它原样当作
/// 非流式的错误体回给客户。两处都得过这个函数。
///
/// **保留 `error.type`,只换 `message`**:客户端 SDK(Claude Code / NewAPI)按 type 判是否
/// 重试、是否把渠道判死,动它就等于改重试语义 —— 本次明确不做。type 本身是 Anthropic
/// 协议里的公开常量,不含厂商线索。
fn sanitize_upstream_error_payload(data: &serde_json::Value) -> serde_json::Value {
    use UpstreamErrorKind as K;
    let ty = data.pointer("/error/type").and_then(|v| v.as_str());
    // type → 语义最接近的 kind,借用同一套中性文案,避免两处口径漂移。
    let msg = match ty {
        Some("overloaded_error") => K::Overloaded,
        Some("rate_limit_error") => K::RateLimited,
        Some("invalid_request_error") | Some("not_found_error") => K::BadRequest,
        Some("authentication_error") | Some("permission_error") => K::TokenInvalid,
        _ => K::ServerError,
    }
    .client_message();
    match ty {
        Some(t) => serde_json::json!({"type":"error","error":{"type": t, "message": msg}}),
        // 上游没给 type 也**必须补一个**:少了它,严格按 schema 校验的客户端连错误都
        // 解析不了(见 anthropic_error_type 的事故记录)。补 `api_error` 是最保守的选择 ——
        // 不指认客户的 key/请求有问题,语义是"服务端出问题,可重试"。
        None => serde_json::json!({"type":"error","error":{"type":"api_error","message": msg}}),
    }
}

/// 把 [`UpstreamError`] 映射为**运维面**响应 —— 与 [`upstream_error_response`] 的唯一
/// 区别是 message 带全量诊断(上游原文/接口名/刷新失败原因)。
///
/// 只给 worker 的**内网**运维端点用(`/oauth/exchange`、`/accounts/{id}/refresh`、
/// `/accounts/{id}/quota`):它们由 admin 面板扇出调用,listen 在 127.0.0.1,客户到不了。
/// 导号时"这个号到底是 invalid_grant 还是网络抖"全靠这段原文,脱敏等于把运维眼睛蒙上。
///
/// ⚠️ 新增端点前先问一句:客户能打到吗?能 → 用 [`upstream_error_response`]。
fn admin_error_response(e: &gw_core::error::UpstreamError) -> axum::response::Response {
    let code = upstream_status(e.kind);
    (
        code,
        Json(serde_json::json!({"type":"error","error":{
            "type": anthropic_error_type(e.kind), "message": e.to_string()}})),
    )
        .into_response()
}

/// HTTP 529 —— Anthropic 的过载状态码。`StatusCode` 无对应常量,且 `from_u16` 不是
/// `const fn`,故用函数而非常量。529 落在合法区间(100..1000),`from_u16` 不会失败;
/// 仍写 `unwrap_or` 兜底以免任何情况下 panic 在错误路径上。
fn overloaded_status() -> StatusCode {
    StatusCode::from_u16(529).unwrap_or(StatusCode::BAD_GATEWAY)
}

/// 流结束时把本次 usage 落库(#130)。`usage=None`(空/错误流无终结用量)或 `sink=None`
/// (控制面库打开失败降级)时直接跳过。落库失败仅告警,**绝不影响**已发给客户端的响应。
///
/// `client_key` = router 经内网头透传的客户 key(无则空串,归到"未归属"桶);连同
/// `account_id` 一起落库,支撑按账号 + 按客户(apikey)两个维度的用量/成本统计。
async fn finalize_usage(
    sink: Option<&Arc<dyn UsageSink>>,
    account_id: &str,
    model: &str,
    client_key: &str,
    usage: Option<&ChatUsage>,
    success: bool,
) {
    let (Some(sink), Some(u)) = (sink, usage) else {
        return;
    };
    let rec = UsageRecord {
        client_key_id: client_key.to_string(),
        account_id: account_id.to_string(),
        model: model.to_string(),
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        cache_read_tokens: u.cache_read_tokens,
        cache_creation_tokens: u.cache_creation_tokens,
        real_cache_read_tokens: u.real_cache_read_tokens,
        metering_credit: u.metering_credit,
        success,
    };
    if let Err(e) = sink.record(rec).await {
        tracing::warn!(account = %account_id, "usage 落库失败(不影响响应): {e}");
    }
}

/// 按客户端 `stream` 标志分发:provider 一律产流,这里决定回 SSE 还是折叠成单个
/// 非流式 Messages 响应(折叠逻辑写一次,见 [`gw_core::fold`])。两条路径都做同一套
/// 收尾(账号生命周期上报 + usage 落库)。
/// 请求日志环形保留条数(最新 N 条;`insert_request_log` 按此裁旧)。
///
/// 原为 2000(最初的用户口径"最新 2000 条")。问题不在这个数字,而在流量长大了:
/// 2026-07-26 实测 **113.6 条/分钟**,2000 条只覆盖 **18 分钟**——用户报障过来时证据
/// 往往已被轮转掉,排查全靠运气(这次定位流卡死,第一次挑中的样本就是在准备重放时
/// 被轮转没的)。按实测单条约 181KB(gzip 后:client_payload 110KB + kiro_payload 69KB
/// + response 2KB)估算,10000 条约 1.7GB、覆盖约 1.5 小时。
///
/// ⚠️ 再调大前**先看磁盘**:这台机 2026-07-26 曾到 93%。若需要更长的留证窗口,砍
/// `kiro_payload`(它是 client_payload 的重渲染,信息冗余却占 38% 体积)比加磁盘划算。
const REQUEST_LOG_CAP: u64 = 10_000;
/// 单条报文(client/kiro)入库前的**文本**体积上限(截断兜底)。报文经 gzip 压缩入库
/// (`gw-store`,文本压 5-10 倍),且图片/文档已抽到去重 blob 表,故**全文存储不再截断**;
/// 此上限抬到 16MiB 仅作防御性护栏——Kiro 报文体积硬上限 ~6.3MB(`DEFAULT_MAX_BODY_BYTES`),
/// 真实报文绝不触顶,只挡住非常规超大输入(防单行无界内存/库占用)。
const MAX_LOG_PAYLOAD_BYTES: usize = 16 * 1024 * 1024;

/// 流式模型回复采集的**累计字节**上限(真正的内存护栏:按 SSE data 序列化字节计,而非按条数——
/// 单个 `input_json_delta`/工具增量可能很大,按条数封顶无法挡住"少量超大事件"撑爆内存)。
/// 与入库截断阈值同量级:缓存超过最终会被截断的体量没有意义。触顶后停止累积(已采集部分仍折叠)。
const RESPONSE_LOG_MAX_BYTES: usize = MAX_LOG_PAYLOAD_BYTES;

/// 一次请求"模型回复"的采集结果,交给落库任务(blocking)折叠/序列化——把重活留在 blocking,
/// 且让非流式路径**复用已折叠的响应体**(不二次折叠,避免与下发客户端的响应产生分歧)。
enum ResponseLog {
    /// 无回复(选号前失败 / 无 message_start)。
    None,
    /// 非流式:已折叠好的 Anthropic Messages(下发客户端的同一份),直接序列化入库。
    Folded(serde_json::Value),
    /// 流式:转发期间按字节预算采集的 SSE 事件,落库任务里折叠成 Messages。
    Events(Vec<SseEvent>),
}

/// 报文入库前处理:**按字段把图片/文档 base64 抽到 blob 列表**(`data`/`bytes` 键的长值,
/// 替换为 `blob:<hash>` 引用;对话正文走 `text` 键,绝不误抽),**再**按字符边界截断文本兜底。
/// 返回(处理后报文文本, 抽出的 blob)。非 JSON(如转换错误串)原样截断、无 blob。
fn prepare_log_payload(raw: String) -> (String, Vec<gw_core::store::LogBlob>) {
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(mut v) => {
            let blobs = gw_core::store::extract_log_blobs(&mut v);
            let text = serde_json::to_string(&v).unwrap_or(raw);
            (truncate_log_payload(text), blobs)
        }
        Err(_) => (truncate_log_payload(raw), Vec::new()),
    }
}

/// 把折叠后的模型回复(Anthropic Messages)序列化入库。超 [`MAX_LOG_PAYLOAD_BYTES`] 时**不**像
/// 普通报文那样硬截成半截非法 JSON(会让详情页"格式化"视图解析失败),而是替换成一条**合法**的
/// 占位 Messages(保留 formatted 渲染 + 明确标注截断,见 `_truncated`)。
fn serialize_response_capped(msg: &serde_json::Value) -> String {
    let s = serde_json::to_string(msg).unwrap_or_default();
    if s.len() <= MAX_LOG_PAYLOAD_BYTES {
        return s;
    }
    serde_json::json!({
        "type": "message",
        "role": "assistant",
        "content": [{
            "type": "text",
            "text": format!("<模型回复过大(约 {} 字节),已省略未入库>", s.len()),
        }],
        "_truncated": true,
    })
    .to_string()
}

/// 按 UTF-8 字符边界安全截断报文(超 [`MAX_LOG_PAYLOAD_BYTES`] 时)。
fn truncate_log_payload(s: String) -> String {
    if s.len() <= MAX_LOG_PAYLOAD_BYTES {
        return s;
    }
    let mut end = MAX_LOG_PAYLOAD_BYTES;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…<已截断,原 {} 字节>", &s[..end], s.len())
}

/// 同步构造并落一条请求日志(环形保留最新 [`REQUEST_LOG_CAP`] 条)。**两份报文都在此序列化**
/// (用户原始 `client_payload` = req.body;发 Kiro 前 `kiro_payload` = gw-kiro 纯渲染助手重渲染,
/// 不跑 cache_sim/体积护栏,无副作用)——故必须经 [`spawn_request_log_blocking`] 丢到 blocking
/// 线程池跑,**绝不在收入热路径**(handler 入口 / SSE async poll)同步执行(审查 Skeptic#1/#3)。
/// `account=None`(选号前即失败)→ account_id/kiro_payload 留空。落库失败仅告警。
#[allow(clippy::too_many_arguments)]
fn write_request_log(
    store: Arc<SqliteStore>,
    req: ChatRequest,
    account: Option<Arc<Account>>,
    client_key: String,
    is_stream: bool,
    success: bool,
    status_code: Option<i64>,
    error_kind: Option<String>,
    duration_ms: Option<i64>,
    ttfb_ms: Option<i64>,
    usage: Option<ChatUsage>,
    response: ResponseLog,
    family: &str,
) {
    let (input, output, cache_read, cache_creation, real_cache_read, metering_credit) = usage
        .as_ref()
        .map(|u| {
            (
                u.input_tokens,
                u.output_tokens,
                u.cache_read_tokens,
                u.cache_creation_tokens,
                u.real_cache_read_tokens,
                u.metering_credit,
            )
        })
        .unwrap_or((0, 0, 0, 0, 0, 0.0));
    // 抽出两份报文里的媒体 blob(图片/文档);合并后入库按 hash 去重(同图复用一行)。
    let (client_payload, mut blobs) =
        prepare_log_payload(serde_json::to_string(&req.body).unwrap_or_default());
    // 守卫依据 **worker family**,而非 account.provider 字段——
    // filter_by_provider 放行 provider 字段为空的账号进 kiro worker,
    // 若按 account.provider 判断会丢掉空-provider kiro 号的报文渲染。
    let (account_id, kiro_payload) = match &account {
        Some(a) if family == "kiro" => {
            let (kp, kb) = prepare_log_payload(gw_kiro::chat::render_kiro_payload(&req, a));
            blobs.extend(kb);
            (a.account_id.clone(), kp)
        }
        Some(a) => (a.account_id.clone(), String::new()),
        None => (String::new(), String::new()),
    };
    // 模型回复:折叠/序列化都在此 blocking 任务内做,不占热路径。失败请求/无回复 → 空串(详情页不展示)。
    let response_payload = match response {
        ResponseLog::None => String::new(),
        // 非流式:复用下发客户端的同一份折叠结果,不二次折叠(避免与客户端响应分歧)。
        ResponseLog::Folded(msg) => serialize_response_capped(&msg),
        ResponseLog::Events(events) if events.is_empty() => String::new(),
        ResponseLog::Events(events) => match gw_core::fold::fold_sse_to_message(&events) {
            Ok(msg) => serialize_response_capped(&msg),
            Err(_) => {
                // 折叠失败(协议违例 / 缺 message_start / 提前断流):无合法回复可存。
                // success 请求却折叠失败值得一条诊断(上游格式变更会在此先冒头)。
                if success {
                    tracing::debug!(account = %account_id, "模型回复折叠失败,response_payload 留空");
                }
                String::new()
            }
        },
    };
    let log = RequestLog {
        client_key_id: client_key,
        account_id,
        model: req.model.clone(),
        stream: is_stream,
        success,
        status_code,
        error_kind,
        duration_ms,
        ttfb_ms,
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: cache_read,
        cache_creation_tokens: cache_creation,
        // "报"口径本次上报总 token(input 已含放大后的 cache_read);前端展示"报 N"。
        reported_tokens: input.saturating_add(output),
        // "真":上游真实命中;"credit":Kiro 原生计费。诊断/优化用,不参与计费。
        real_cache_read_tokens: real_cache_read,
        metering_credit,
        client_payload,
        kiro_payload,
        response_payload,
        blobs,
    };
    if let Err(e) = store.insert_request_log(&log, REQUEST_LOG_CAP) {
        tracing::warn!(error = %e, account = %log.account_id, "请求日志落库失败");
    }
    // 账号累计成功/失败计数(监控用,非计费)。与请求日志同处 blocking 任务,不占热路径;
    // 每次上游调用终态收尾恰好一次(成功/终态失败),account_id 空则内部跳过。
    if let Err(e) = store.bump_account_counters(&log.account_id, success) {
        tracing::warn!(error = %e, account = %log.account_id, "账号计数更新失败");
    }
}

/// 把请求日志落库 detach 到 **blocking 线程池**(两份报文序列化 + 同步 SQLite 写均为阻塞工作),
/// 不占用 async worker 线程、不拖慢活跃 SSE poll(审查 Skeptic#3)。store=None(库降级)或无
/// 运行时上下文则跳过。guard 跟随任务存活:停机排空经 pending_writes.wait_idle 等这批收尾。
#[allow(clippy::too_many_arguments)]
fn spawn_request_log_blocking(
    st: &Arc<WorkerState>,
    req: ChatRequest,
    account: Option<Arc<Account>>,
    client_key: String,
    is_stream: bool,
    success: bool,
    status_code: Option<i64>,
    error_kind: Option<String>,
    duration_ms: Option<i64>,
    ttfb_ms: Option<i64>,
    usage: Option<ChatUsage>,
    response: ResponseLog,
    family: &'static str,
) {
    let Some(store) = st.store.clone() else {
        return;
    };
    let Ok(handle) = tokio::runtime::Handle::try_current() else {
        return;
    };
    let guard = st.pending_writes.enter();
    handle.spawn_blocking(move || {
        let _guard = guard;
        write_request_log(
            store, req, account, client_key, is_stream, success, status_code, error_kind,
            duration_ms, ttfb_ms, usage, response, family,
        );
    });
}

/// 服务端 web search 收尾:跑执行回环(反代自搜 DDG + 注回续跑),把组装好的标准
/// `server_tool_use`+`web_search_tool_result`+text 响应合成为流,再交给 [`finish_response`]
/// 复用全部流式/非流式分发、日志、usage 上报机器。回环失败按上游错误回显(不重试:已开始
/// 消费首轮流,符合 v60 不放大错误契约)。
#[allow(clippy::too_many_arguments)]
async fn finish_web_search_response(
    st: Arc<WorkerState>,
    lease: scheduler::AccountLease,
    first_stream: gw_core::provider::ChatStream,
    req: &ChatRequest,
    client_key: &str,
    ctx: CallCtx,
    spec: crate::websearch::WebSearchSpec,
    started_at: std::time::Instant,
    // 成员视图:续轮的 RPM 准入与选号同口径(见 note_upstream_call)。
    view: Option<scheduler::GroupView>,
    wire: Wire,
) -> axum::response::Response {
    let account = ctx.account.clone();
    let account_id = account.account_id.clone();
    // 定频准入回调:web search 的每一次续轮上游调用都要过该号的有效 RPM 闸
    // (含暖机),达限即停发、优雅降级收尾。
    let st_for_rpm = st.clone();
    let acct_for_rpm = account_id.clone();
    let on_call = move || st_for_rpm.scheduler.note_upstream_call(&acct_for_rpm, view.as_ref());
    match crate::websearch::run_loop(
        st.provider.clone(),
        &ctx,
        req,
        spec,
        first_stream,
        &on_call,
    )
    .await
    {
        Ok((events, usage)) => {
            let synth = crate::websearch::synth_stream(events, usage);
            finish_response(st, lease, synth, req, client_key, account, started_at, wire).await
        }
        Err(e) => {
            tracing::warn!(account = %account_id, kind = ?e.kind, "web search 回环失败: {e}");
            st.scheduler
                .report_failure_with_gen(&account_id, e.kind, lease.suspend_gen);
            drop(lease);
            upstream_error_response_wire(wire, &e)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_response(
    st: Arc<WorkerState>,
    lease: scheduler::AccountLease,
    stream: gw_core::provider::ChatStream,
    req: &ChatRequest,
    client_key: &str,
    account: Arc<Account>,
    started_at: std::time::Instant,
    wire: Wire,
) -> axum::response::Response {
    if req.stream {
        // 流式:返回惰性 SSE 响应,收尾走 StreamCtx::Drop(同步上报 + detach 落库 usage+请求日志)。
        stream_response(
            st,
            lease,
            stream,
            req.clone(),
            client_key.to_string(),
            account,
            started_at,
            wire,
        )
    } else {
        // 非流式:此处即时抽干流、折叠成单个 Messages JSON。
        collect_response(
            &st.scheduler,
            st.usage_sink.as_ref(),
            st.store.clone(),
            Some(st.pending_writes.clone()),
            lease,
            stream,
            req.clone(),
            client_key.to_string(),
            account,
            started_at,
            st.provider.family(),
            wire,
        )
        .await
    }
}

/// 非流式路径:抽干 provider 流,折叠成单个 Anthropic Messages JSON 响应。
///
/// 抽干期间持有 `lease`(占并发槽);抽干完成后按结果上报账号生命周期 + usage 落库
/// (与流式 [`stream_response`] 同口径)。流中出现硬错误 / SSE `error` 事件 → 回上游
/// 错误响应(不重试:已开始消费流,符合 v60 不放大错误契约)。
///
/// 取显式依赖(scheduler / usage_sink)而非整个 WorkerState,便于单测。
#[allow(clippy::too_many_arguments)]
async fn collect_response(
    scheduler: &AccountScheduler,
    usage_sink: Option<&Arc<dyn UsageSink>>,
    store: Option<Arc<SqliteStore>>,
    pending_writes: Option<Arc<PendingWrites>>,
    lease: scheduler::AccountLease,
    mut stream: gw_core::provider::ChatStream,
    req: ChatRequest,
    client_key: String,
    account: Arc<Account>,
    started_at: std::time::Instant,
    family: &'static str,
    wire: Wire,
) -> axum::response::Response {
    /// 非流式抽干的事件数上限(OOM 粗护栏:正常响应 < 数万事件,远低于此;
    /// 超出视为异常上游,回受控错误而非无界吃内存。审查 #3)。
    const MAX_NONSTREAM_EVENTS: usize = 500_000;

    let account_id = lease.account_id().to_string();
    // 出站换形状要的两样东西(`req` 稍后被 move 进落库任务,先取):兜底模型名,
    // 以及 Responses 对象要回显的请求参数。
    let req_model = req.model.clone();
    let req_echo = gw_core::openai::resp_out::RequestEcho::from_anthropic(&req.body);
    let mut events: Vec<SseEvent> = Vec::new();
    let mut last_usage: Option<ChatUsage> = None;
    let mut hard_err: Option<UpstreamError> = None;
    let mut over_cap = false;

    while let Some(item) = stream.next().await {
        match item {
            Ok(StreamItem::Sse(ev)) => {
                if events.len() >= MAX_NONSTREAM_EVENTS {
                    over_cap = true;
                    break;
                }
                events.push(ev);
            }
            Ok(StreamItem::Usage(u)) => {
                last_usage = Some(u);
            }
            // 非流式抽干:掐流信号只对流式主链路的软冷却有意义,这里忽略
            // (响应完整性由下方的 message_stop 折叠校验兜底,行为不变)。
            Ok(StreamItem::UpstreamCut) => {}
            Err(e) => {
                // 对外只发中性文案(见 upstream_error_response),原文必须在这里落日志,
                // 否则这条路径的上游报文就彻底没人记了。
                tracing::warn!(account = %account_id, kind = ?e.kind, "非流式抽干时上游错误: {e}");
                hard_err = Some(e);
                break;
            }
        }
    }

    // 先定结果,再统一收尾(账号生命周期 + usage 落库),保证 success 与真实结果一致
    // (审查 #1:不能在折叠失败前就抢报 success / 记 success=true)。
    enum Outcome {
        Ok(serde_json::Value),
        Upstream(UpstreamError),
        Bad(serde_json::Value),
    }
    let outcome = if let Some(e) = hard_err {
        Outcome::Upstream(e)
    } else if over_cap {
        Outcome::Bad(serde_json::json!({"type":"error","error":{"type":"api_error",
            "message":"非流式响应事件数超上限,已中止"}}))
    } else {
        match gw_core::fold::fold_sse_to_message(&events) {
            Ok(msg) => Outcome::Ok(msg),
            Err(err_data) => Outcome::Bad(err_data),
        }
    };

    let success = matches!(outcome, Outcome::Ok(_));
    // 非流式状态码:成功 200;上游错误与客户端实收同源(见 upstream_status:BadRequest/
    // ModelNotAvailable→400、Overloaded→529、其余 502),折叠失败 502。
    // 详情据此回显,不留空(审查 low#7)。
    let (status_code, error_kind): (Option<i64>, Option<String>) = match &outcome {
        Outcome::Ok(_) => (Some(200), None),
        Outcome::Upstream(e) => (
            Some(upstream_status(e.kind).as_u16() as i64),
            Some(format!("{:?}", e.kind)),
        ),
        Outcome::Bad(_) => (Some(502), Some("bad_gateway".to_string())),
    };
    if success {
        // 非流式折叠成功 = 完整成功(消息体已收齐),可清 suspend 退避进度。
        scheduler.report_success_observed(&account_id, lease.suspend_gen, true);
    } else {
        let kind = match &outcome {
            Outcome::Upstream(e) => e.kind,
            _ => UpstreamErrorKind::ServerError,
        };
        scheduler.report_failure_with_gen(&account_id, kind, lease.suspend_gen);
        // 防御:理论上 INVALID_MODEL_ID 是首包前 400 走主循环,但若上游 mid-stream 冒出
        // 也在此记 (号,模型) 不可用,与主循环口径一致(不禁号 + 后续选号跳过该号)。
        if kind == UpstreamErrorKind::ModelNotAvailable {
            scheduler.mark_model_unavailable(&account_id, &req.model);
        }
    }
    finalize_usage(
        usage_sink,
        &account_id,
        &req.model,
        &client_key,
        last_usage.as_ref(),
        success,
    )
    .await;
    drop(lease); // 释放并发槽(响应已抽干);请求日志在其后 detach,不再占槽(审查 medium#4)。
    // 请求日志落库:detach 到 blocking 线程池(序列化两份报文 + 写库),不延迟非流式响应、
    // 不占用 async worker 线程。store/pending_writes 缺失(测试/降级)→ 跳过。
    if let (Some(store), Some(pw), Ok(handle)) = (
        store,
        pending_writes,
        tokio::runtime::Handle::try_current(),
    ) {
        let guard = pw.enter();
        let duration_ms = Some(started_at.elapsed().as_millis() as i64);
        // 非流式:复用上面已折叠、即将下发客户端的同一份响应体(成功才有),不二次折叠——
        // 保证入库的 response_payload 与客户端实际收到的响应严格一致。失败 → 无回复。
        let response = match &outcome {
            Outcome::Ok(msg) => ResponseLog::Folded(msg.clone()),
            _ => ResponseLog::None,
        };
        handle.spawn_blocking(move || {
            let _guard = guard;
            write_request_log(
                store,
                req,
                Some(account),
                client_key,
                false,
                success,
                status_code,
                error_kind,
                duration_ms,
                None,
                last_usage,
                response,
                family,
            );
        });
    }

    match outcome {
        // 成功体按线缆换形状。**入库的仍是 Anthropic 那份**(上面的 ResponseLog::Folded),
        // 与流式路径同口径 —— 日志/语料链路只认一种形状。
        Outcome::Ok(msg) => {
            let body = match wire {
                Wire::Anthropic => msg,
                Wire::OpenAiChat { .. } => {
                    gw_core::openai::chat_out::fold_completion(&msg, &req_model)
                }
                Wire::OpenAiResponses => gw_core::openai::resp_out::fold_response(
                    &msg,
                    &req_model,
                    &req_echo,
                ),
            };
            (StatusCode::OK, Json(body)).into_response()
        }
        Outcome::Upstream(e) => upstream_error_response_wire(wire, &e),
        // 折叠失败体有两个来源:我方的折叠诊断,以及 `fold_sse_to_message` 对上游 error
        // 事件的**原样回传**(fold.rs:49)。后者是上游报文,不能直发客户 —— 与流式路径
        // 同一个闸门,原文落日志。
        Outcome::Bad(data) => {
            tracing::warn!(account = %account_id, "非流式折叠失败,回中性错误: {data}");
            let neutral = sanitize_upstream_error_payload(&data);
            let body = if wire.is_openai() {
                gw_core::openai::error::from_anthropic_error(&neutral, 502)
            } else {
                neutral
            };
            (StatusCode::BAD_GATEWAY, Json(body)).into_response()
        }
    }
}

/// 把 provider 的 StreamItem 流转成 axum SSE 响应,并在流结束时按结果上报账号生命周期
/// + 把终结 usage 落库(#130)。
///
/// 关键:`lease`(并发许可)被 move 进流的状态,持有到流耗尽才 Drop → 整个响应期间
/// 占用该账号一个并发槽,符合 v52 并发语义。流内出现 error 事件 / Err → 上报失败;
/// 干净结束 → 上报成功。usage 事件不转发客户端(缓存到 `last_usage`,流终态统一落库)。
/// 上游静默多久后开始发保活帧,并按此间隔重复。必须**远小于**下游客户端的空闲判定
/// 阈值:2026-07-26 取证四次卡死的静默时长为 120.5/123.3/124.8/126.0 秒,可推定客户端
/// 阈值约 120s,取 20s 留 6 倍余量。
const STREAM_IDLE_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(20);
/// 上游连续静默的硬上限:超过即主动中止,按 `upstream_idle_abort` 收尾。
///
/// 它的意义是让本机制在**两种尚未证实的情形下都正确**:上游只是慢 → 保活已让客户端
/// 等得起,在本窗口内恢复即救回;上游已经死了 → 客户端拿到明确错误,而不是像现在这样
/// 静默两分钟被客户端自己砍断、库里还记成 200 成功。上线后两类日志的占比,即可回答
/// "慢还是死"——这是不额外消耗上游配额就能拿到该答案的唯一途径。
const STREAM_IDLE_ABORT: std::time::Duration = std::time::Duration::from_secs(300);

/// 当前开着的内容块种类(带块索引)。保活帧必须**贴合当前块类型**:Anthropic SSE 里
/// 往 tool_use 块塞 text_delta 是非法的,客户端解析器会错乱。只跟踪这三种能安全附加
/// 零增量的块;其余(如 redacted_thinking,它没有增量形态)一律视为"无可附着的块"。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum OpenBlock {
    Text(u64),
    Thinking(u64),
    ToolUse(u64),
}

/// 据**已转发给客户端**的事件维护"当前开着哪个块"。只认 content_block_start/stop
/// 与 message_stop,其余事件不改变状态。
fn track_open_block(cur: Option<OpenBlock>, ev: &SseEvent) -> Option<OpenBlock> {
    match ev.event.as_str() {
        "content_block_start" => {
            let Some(idx) = ev.data.get("index").and_then(serde_json::Value::as_u64) else {
                return cur;
            };
            match ev
                .data
                .get("content_block")
                .and_then(|b| b.get("type"))
                .and_then(serde_json::Value::as_str)
            {
                Some("text") => Some(OpenBlock::Text(idx)),
                Some("thinking") => Some(OpenBlock::Thinking(idx)),
                Some("tool_use") => Some(OpenBlock::ToolUse(idx)),
                // 其余块类型没有可安全附加的零增量形态 → 视为无块可附着。
                _ => None,
            }
        }
        "content_block_stop" | "message_stop" => None,
        _ => cur,
    }
}

/// 构造一个语义为 no-op 的保活帧(客户端把增量逐段拼接,拼空串等于没拼)。
///
/// **为什么不发 Anthropic 官方的 `ping`**:本链路下游是 NewAPI,其 Claude→OpenAI 转换器
/// 只识别 message_start / content_block_{start,delta} / message_{delta,stop} 五种事件,
/// 且**没有 default 兜底**(实测 `to_oai_chat_resp.go`)——`ping` 到那里直接消失,不会变成
/// 任何下游事件,客户端照样判定空闲。故改用"空增量":它属于上述五种之一,能被转换成一个
/// 真实的下游 chunk,从而重置客户端的空闲计时。
fn keepalive_frame(open: Option<OpenBlock>) -> SseEvent {
    let delta = match open {
        Some(OpenBlock::ToolUse(i)) => {
            (i, serde_json::json!({"type":"input_json_delta","partial_json":""}))
        }
        Some(OpenBlock::Text(i)) => (i, serde_json::json!({"type":"text_delta","text":""})),
        Some(OpenBlock::Thinking(i)) => {
            (i, serde_json::json!({"type":"thinking_delta","thinking":""}))
        }
        // 没有开着的块时零增量无处附着,只能退回官方 ping。下游可能吃掉它,但此刻
        // 确实没有任何合法帧可发——总比什么都不发强,且对原生 Anthropic 客户端有效。
        None => return SseEvent::new("ping", serde_json::json!({"type":"ping"})),
    };
    SseEvent::new(
        "content_block_delta",
        serde_json::json!({"type":"content_block_delta","index":delta.0,"delta":delta.1}),
    )
}

/// 出站线缆转换器。`None`(= [`Wire::Anthropic`])时整条转发路径**一行都不改**。
///
/// 一条 Anthropic 事件在 OpenAI 侧可能产出 0..N 帧(如 `content_block_start(tool_use)`
/// 会同时开条目和发首个参数帧),所以接口是「进一条、出一个 Vec」,由调用方排队下发。
enum OutConv {
    Chat(gw_core::openai::chat_out::ChatStreamOut),
    Responses(gw_core::openai::resp_out::ResponsesStreamOut),
}

impl OutConv {
    /// 按线缆建转换器;Anthropic 返回 `None`。
    ///
    /// `body` 是**转换后的 Anthropic 请求体**:Responses 的 `Response` 对象要回显
    /// `instructions` / `tools` / `tool_choice` / `parallel_tool_calls`,这些信息在 IR
    /// 里全都还在,反向映射即可 —— 不必把原始 OpenAI 请求一路带下来。
    fn for_wire(wire: Wire, model: &str, body: &serde_json::Value) -> Option<Self> {
        match wire {
            Wire::Anthropic => None,
            Wire::OpenAiChat { include_usage } => Some(Self::Chat(
                gw_core::openai::chat_out::ChatStreamOut::new(model, include_usage),
            )),
            Wire::OpenAiResponses => Some(Self::Responses(
                gw_core::openai::resp_out::ResponsesStreamOut::new(model).with_echo(
                    gw_core::openai::resp_out::RequestEcho::from_anthropic(body),
                ),
            )),
        }
    }

    fn push(&mut self, ev: &SseEvent) -> Vec<WireFrame> {
        match self {
            Self::Chat(c) => c.push(ev),
            Self::Responses(r) => r.push(ev),
        }
    }

    /// 流走到尽头(正常结束 / 上游断流 / 我方中止)时补齐终止序列。幂等。
    fn finish(&mut self) -> Vec<WireFrame> {
        match self {
            Self::Chat(c) => c.finish(),
            Self::Responses(r) => r.finish(),
        }
    }

    /// 硬错误 → 该线缆的失败形状。`data` 须**已中性化**。
    fn fail(&mut self, data: &serde_json::Value) -> Vec<WireFrame> {
        match self {
            Self::Chat(c) => c.fail(data),
            Self::Responses(r) => r.fail(data),
        }
    }

    /// 保活帧。可能多于一帧:Responses 在首个上游事件迟到时要先补 `response.created`。
    fn keepalive(&mut self) -> Vec<WireFrame> {
        match self {
            Self::Chat(c) => vec![c.keepalive()],
            Self::Responses(r) => r.keepalive(),
        }
    }

    /// 终止序列已发过。之后**不得再发保活** —— `[DONE]` / `response.completed`
    /// 之后冒出新帧,严格客户端会当成协议违例。
    fn is_finished(&self) -> bool {
        match self {
            Self::Chat(c) => c.is_finished(),
            Self::Responses(r) => r.is_finished(),
        }
    }
}

/// [`WireFrame`] → axum SSE 事件(gw-core 不认识 axum,转换只此一处)。
fn wire_event(f: WireFrame) -> Event {
    match f.event {
        Some(name) => Event::default().event(name).data(f.data),
        None => Event::default().data(f.data),
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_response(
    st: Arc<WorkerState>,
    lease: scheduler::AccountLease,
    stream: gw_core::provider::ChatStream,
    req: ChatRequest,
    client_key: String,
    account: Arc<Account>,
    started_at: std::time::Instant,
    wire: Wire,
) -> axum::response::Response {
    /// unfold 累积态:lease 持有到流结束;reported 防重复上报;last_usage 缓存终结用量。
    struct StreamCtx {
        st: Arc<WorkerState>,
        account_id: String,
        model: String,
        client_key: String,
        /// 选号时的 suspend 世代快照(见 `_lease`);Drop 上报时原样带回。
        suspend_gen: u64,
        _lease: scheduler::AccountLease,
        inner: gw_core::provider::ChatStream,
        saw_error: bool,
        reported: bool,
        last_usage: Option<ChatUsage>,
        // 请求日志(#③)采集态(client_payload 在 detach 的 blocking 任务里从 req.body 序列化,
        // 不在此持有,避免热路径成本):
        req: ChatRequest,
        account: Arc<Account>,
        started_at: std::time::Instant,
        first_byte_at: Option<std::time::Instant>,
        error_kind: Option<String>,
        // 模型回复采集(#③):流过期间累积转发给客户端的 SSE 事件(克隆一份),收尾时折叠成
        // Anthropic Messages 写入 response_payload。按**累计字节**(resp_bytes)封顶 RESPONSE_LOG_MAX_BYTES
        // 防超长/超大回复无界占内存(触顶后停止累积,折叠仍尽力而为;正常回复远低于此)。
        resp_events: Vec<SseEvent>,
        resp_bytes: usize,
        /// 流是否走到了 `message_stop`(= 客户端拿到了完整响应)。
        saw_message_stop: bool,
        /// 是否因上游静默超过 [`STREAM_IDLE_ABORT`] 被我方主动中止。
        ///
        /// ⚠️ 它与 `saw_error` **必须严格分开**:`saw_error` 驱动账号健康上报
        /// (report_failure → failure_count → 撞 max_failures 就禁号)。而"流没收完"的成因
        /// 既可能是上游静默、也可能是客户端主动断开(用户 Ctrl-C),拿它去扣账号健康会重演
        /// 2026-07-25 那场 35 秒禁光 7 个号的事故——上游抖动时所有账号会一起中招。
        /// 故本标志与 `saw_message_stop` **只**影响请求日志的 success/error_kind,
        /// 绝不参与账号生命周期上报。
        idle_aborted: bool,
        /// provider 显式上报过「上游静默掐流」(`StreamItem::UpstreamCut`,目前仅 Kiro:
        /// 见过真实上游 payload 但未收终止事件就 EOF)。实测是封号前兆。
        ///
        /// ⚠️ 与 `idle_aborted` 同款隔离纪律,且**更严**:Drop 里它走独立分支——
        /// 跳过 `report_success_observed`(它会清 failure_count/429 strikes/paced_until,
        /// 等于改写健康状态,codex 对抗评审#2),也绝不走 report_failure;
        /// 只喂 scheduler 的软冷却(report_upstream_cut)与请求日志 error_kind。
        upstream_cut: bool,
        /// 选号时的软冷却代际快照(随 lease 携带),Drop 上报时原样带回。
        cut_epoch: u64,
        /// 当前开着的内容块(决定保活帧的形态)。
        open_block: Option<OpenBlock>,
        /// 最近一次收到上游事件的时刻(空闲时长的基准)。
        last_event_at: std::time::Instant,
        /// 出站线缆转换器。`None` = 原生 Anthropic,转发路径与引入本字段之前逐字节相同。
        conv: Option<OutConv>,
        /// 待下发帧队列:一条上游事件在 OpenAI 侧可能产出多帧,而 `unfold` 每次只能
        /// 交出一个 —— 多出来的排这里,下一轮先排空再去拉上游。
        pending: std::collections::VecDeque<Event>,
        /// 上游流已耗尽且终止序列已排入 `pending`,排空后即结束。
        drained: bool,
    }

    // 收尾(账号生命周期上报 + usage 落库)统一放 Drop:无论流跑到 None 正常结束,
    // **还是客户端中途断开导致 axum 直接 drop 响应体**(此时 unfold 不再被 poll、永远到不了
    // None 分支),Drop 都会触发,确保 usage 不漏记、账号信号不丢(审查 Skeptic#1/Architect#1)。
    // 生命周期上报是同步的(parking_lot,Drop 内直接做);usage 落库是 async,detach 到运行时
    // 异步执行——既不阻塞 SSE 收尾 poll(审查 Skeptic#2),又不依赖流被读到 EOF。
    impl Drop for StreamCtx {
        fn drop(&mut self) {
            // 账号生命周期上报(一次)。Err 分支可能已按具体 kind 上报过(reported=true)。
            // ⚠️ **仍然只看 `saw_error`**:流没收完(客户端断开 / 上游静默中止)不扣账号健康,
            // 理由见 `idle_aborted` 字段注释——那会在上游抖动时把所有账号一起打死。
            let account_ok = !self.saw_error;
            if !self.reported {
                self.reported = true;
                if self.upstream_cut && account_ok {
                    // upstream_cut 独立分支:与健康/禁用体系完全隔离——
                    // **跳过** report_success_observed(它会清 failure_count/429
                    // strikes/paced_until,等于把掐流当成成功去改写健康状态,
                    // codex 对抗评审#2),也绝不走 report_failure。
                    // 只喂软冷却信号;代际不符(账号已 reset/重启用)由调度器丢弃。
                    self.st
                        .scheduler
                        .report_upstream_cut(&self.account_id, self.cut_epoch);
                } else if account_ok {
                    // suspend 退避清零只认「同世代 + 流走到 message_stop」的完整成功;
                    // 客户端断开/上游静默中止的半流仍清 429 连击等既有口径(方法内分层)。
                    self.st.scheduler.report_success_observed(
                        &self.account_id,
                        self.suspend_gen,
                        self.saw_message_stop,
                    );
                } else {
                    self.st.scheduler.report_failure_with_gen(
                        &self.account_id,
                        UpstreamErrorKind::ServerError,
                        self.suspend_gen,
                    );
                }
            }
            // detach 到当前运行时,做 usage 落库(#130)+ 请求日志落库(#③)。
            // 请求日志**总是**尝试(失败请求也要记),故不像 #130 那样门控 usage/sink;
            // finalize_usage / finalize_request_log 各自对 None 降级。无运行时上下文
            // (理论上不会:SSE body 总在 tokio 内 drop)则跳过。guard 跟随任务存活:
            // 停机排空经 pending_writes.wait_idle 等这批落库收尾(审查 Skeptic#1/Architect#1)。
            // 请求日志的成败判定比账号健康**更严**:上游没报错、**且**流确实走到了
            // message_stop,才算客户端拿到了完整响应。此前这类"中途卡死"一律记成 200 成功,
            // 在 admin UI 和告警里完全隐形——2026-07-26 排查用户报障就栽在这:错误日志
            // 一片干净,故障却真实存在,只能靠翻 `success=1 AND output_tokens=0` 的空流才发现。
            let incomplete = if self.idle_aborted {
                Some("upstream_idle_abort")
            } else if self.upstream_cut {
                // 上游静默掐流(封号前兆):即使 provider 合成了 message_stop 收尾,
                // 也按 upstream_cut 落库(替代 incomplete_stream),供面板聚合分析。
                Some("upstream_cut")
            } else if !self.saw_message_stop {
                Some("incomplete_stream")
            } else {
                None
            };
            let success = account_ok && incomplete.is_none();
            let duration_ms = Some(self.started_at.elapsed().as_millis() as i64);
            let ttfb_ms = self
                .first_byte_at
                .map(|t| t.duration_since(self.started_at).as_millis() as i64);
            if let Some(kind) = incomplete {
                tracing::warn!(
                    account = %self.account_id,
                    model = %self.model,
                    duration_ms = ?duration_ms,
                    kind,
                    "流按未完成口径收尾(**不**影响账号健康)"
                );
            }
            // status_code 记的是**实际发出过的 HTTP 状态**,故仍按 account_ok 判定——
            // 卡死那次的响应头确实是 200,新增信息由 success/error_kind 承载。
            let status_code: Option<i64> = if account_ok { Some(200) } else { None };
            let usage = self.last_usage.take();
            let error_kind = self
                .error_kind
                .take()
                .or_else(|| incomplete.map(str::to_string));
            // usage 落库(#130):async,detach 到运行时(sink.record 是 async)。
            if let (Some(sink), Some(u)) = (self.st.usage_sink.clone(), usage.clone()) {
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let guard = self.st.pending_writes.enter();
                    let account_id = self.account_id.clone();
                    let model = self.model.clone();
                    let client_key = self.client_key.clone();
                    handle.spawn(async move {
                        let _guard = guard;
                        finalize_usage(
                            Some(&sink),
                            &account_id,
                            &model,
                            &client_key,
                            Some(&u),
                            // 计量口径**不动**:仍用 account_ok 而非更严的 success。
                            // 流没收完但上游已产出的 token 该计还得计,把它改成失败会
                            // 悄悄改变历史统计与计费语义,不属于本次改动的范围。
                            account_ok,
                        )
                        .await;
                    });
                }
            }
            // 请求日志落库(#③):detach 到 blocking 线程池(序列化两份报文 + 写库)。失败请求也记,
            // 故不门控 usage/sink;client_payload 在 blocking 任务里从 req.body 序列化(离开热路径)。
            spawn_request_log_blocking(
                &self.st,
                self.req.clone(),
                Some(self.account.clone()),
                self.client_key.clone(),
                true,
                success,
                status_code,
                error_kind,
                duration_ms,
                ttfb_ms,
                usage,
                ResponseLog::Events(std::mem::take(&mut self.resp_events)),
                self.st.provider.family(),
            );
        }
    }

    let account_id = lease.account_id().to_string();
    let model = req.model.clone();
    // 转换器建好再交给结构体:`req` 会在字段列表中途被 move,而字段按书写顺序求值。
    let conv = OutConv::for_wire(wire, &req.model, &req.body);
    let init = StreamCtx {
        st,
        account_id,
        model,
        client_key,
        suspend_gen: lease.suspend_gen,
        cut_epoch: lease.cut_epoch,
        _lease: lease,
        inner: stream,
        saw_error: false,
        reported: false,
        last_usage: None,
        req,
        account,
        started_at,
        first_byte_at: None,
        error_kind: None,
        resp_events: Vec::new(),
        resp_bytes: 0,
        saw_message_stop: false,
        idle_aborted: false,
        upstream_cut: false,
        open_block: None,
        last_event_at: std::time::Instant::now(),
        conv,
        pending: std::collections::VecDeque::new(),
        drained: false,
    };

    let sse = futures::stream::unfold(init, |mut ctx| async move {
        // 单步内循环跳过 usage 事件,直到拿到一个可转发事件或流结束(避免递归类型膨胀)。
        loop {
            // 队列优先:一条上游事件产出的多帧要挨个交出去,期间不去拉上游。
            if let Some(ev) = ctx.pending.pop_front() {
                return Some((Ok::<Event, std::convert::Infallible>(ev), ctx));
            }
            // 终止序列已排空 → 真结束(返回 None 会 drop ctx,收尾走 StreamCtx::drop)。
            if ctx.drained {
                return None;
            }
            // 上一轮已因静默撞上硬上限并发过错误事件 → 结束流。返回 None 会 drop ctx,
            // 连带 drop `inner` 从而真正切断上游连接(不留悬挂请求)。
            if ctx.idle_aborted {
                return None;
            }
            // 取上游事件带空闲超时:静默超过 [`STREAM_IDLE_KEEPALIVE`] 就先给客户端补一个
            // 保活帧再回来继续等。`StreamExt::next` 是取消安全的(元素只在 Poll::Ready 时
            // 取走),超时丢弃这个 future 不会吞掉任何事件。
            let item = match tokio::time::timeout(STREAM_IDLE_KEEPALIVE, ctx.inner.next()).await {
                Ok(item) => {
                    ctx.last_event_at = std::time::Instant::now();
                    item
                }
                Err(_) => {
                    // 转换器已发过终止序列 → 客户端**已经拿到完整响应**,此刻还挂着只是
                    // 在等上游 EOF(为了收 message_stop 之后才到的 usage 事件)。
                    // 这时候既不能发保活(终态之后再冒帧是协议违例),也**不能**判 idle abort ——
                    // 那会把一个已经成功交付的请求在日志里记成 `upstream_idle_abort`
                    // (对抗评审 Architect#5)。静默等 EOF 即可。
                    if ctx.conv.as_ref().is_some_and(OutConv::is_finished) {
                        continue;
                    }
                    let idle = ctx.last_event_at.elapsed();
                    if idle >= STREAM_IDLE_ABORT {
                        // 静默到硬上限:主动中止并给客户端一个**明确错误**,而不是让它继续
                        // 空等、最后自己超时——那条老路我方还会记成 200 成功(见 Drop 注释)。
                        ctx.idle_aborted = true;
                        tracing::warn!(
                            account = %ctx.account_id,
                            model = %ctx.model,
                            idle_secs = idle.as_secs(),
                            "上游静默超过硬上限,主动中止本次流"
                        );
                        let payload = serde_json::json!({"type":"error","error":{
                            "type":"api_error",
                            "message": format!(
                                "upstream stalled: no event for {}s",
                                idle.as_secs()
                            )}});
                        // OpenAI 线缆:转成该协议的失败形状(chat 是 error 帧 + [DONE],
                        // responses 是 response.failed),否则客户端收到一个它读不懂的
                        // Anthropic error 事件,表现和「干等到超时」没区别。
                        if let Some(conv) = ctx.conv.as_mut() {
                            for f in conv.fail(&payload) {
                                ctx.pending.push_back(wire_event(f));
                            }
                            continue;
                        }
                        let out = Event::default().event("error").data(payload.to_string());
                        return Some((Ok::<Event, std::convert::Infallible>(out), ctx));
                    }
                    tracing::debug!(
                        account = %ctx.account_id,
                        model = %ctx.model,
                        idle_secs = idle.as_secs(),
                        "上游静默,发保活帧维持下游空闲计时"
                    );
                    // 保活帧**不**进 resp_events:它不是模型回复内容,且对折叠是 no-op,
                    // 混进去只会平白占掉 RESPONSE_LOG_MAX_BYTES 预算。
                    //
                    // OpenAI 线缆有自己的保活形态(空 delta / response.in_progress),
                    // **不能**把 Anthropic 的零增量喂给转换器 —— 那会在 chat 侧变成一条
                    // 真的空内容增量、在 responses 侧被算进 output_text 的全量文本。
                    if let Some(conv) = ctx.conv.as_mut() {
                        // 已发过终止序列(收到 message_stop 但底层流还没 EOF)→ 静默等 EOF。
                        // 再发保活等于在 [DONE] / response.completed 之后又冒出一帧。
                        // 这里**不能**顺手 drop 上游流:usage 事件常在 message_stop **之后**
                        // 才到(见 gw-cursor),提前切断就是丢计费。
                        if conv.is_finished() {
                            continue;
                        }
                        for f in conv.keepalive() {
                            ctx.pending.push_back(wire_event(f));
                        }
                        continue;
                    }
                    let ka = keepalive_frame(ctx.open_block);
                    let out = Event::default().event(ka.event.clone()).data(ka.data.to_string());
                    return Some((Ok::<Event, std::convert::Infallible>(out), ctx));
                }
            };
            match item {
                Some(Ok(StreamItem::UpstreamCut)) => {
                    // provider 显式上报的「上游静默掐流」前兆:只置标志,**不转发客户端**
                    // (它不是 Anthropic SSE 事件)。收尾在 StreamCtx::drop 的独立分支处理。
                    ctx.upstream_cut = true;
                    continue;
                }
                Some(Ok(StreamItem::Sse(mut ev))) => {
                    // 首个转发事件时刻 = TTFB(请求日志 #③)。
                    if ctx.first_byte_at.is_none() {
                        ctx.first_byte_at = Some(std::time::Instant::now());
                    }
                    if ev.event == "error" {
                        ctx.saw_error = true;
                        if ctx.error_kind.is_none() {
                            ctx.error_kind = Some("stream_error".to_string());
                        }
                        // provider 把上游的 error 帧当**普通 SSE 事件**产出时(dario 直透
                        // Anthropic 流即如此),这条路绕开 UpstreamError,原样转发就等于
                        // 把上游报文发给客户。此处落原文 + 改写载荷,是这条路的唯一闸门。
                        tracing::warn!(
                            account = %ctx.account_id,
                            model = %ctx.model,
                            "上游流内 error 事件: {}", ev.data
                        );
                        ev.data = sanitize_upstream_error_payload(&ev.data);
                    }
                    // 完整性判据:只有真的收到 message_stop 才算客户端拿到完整响应。
                    if ev.event == "message_stop" {
                        ctx.saw_message_stop = true;
                    }
                    // 跟踪当前开着的块,供保活帧选形态(必须在转发前更新:下一轮静默时要用)。
                    ctx.open_block = track_open_block(ctx.open_block, &ev);
                    // OpenAI 线缆:转换后排队下发。请求日志的采集口径**不变** ——
                    // 存的仍是 Anthropic 事件(与 kiro 同口径,yapi 语料链路不用适配)。
                    if let Some(conv) = ctx.conv.as_mut() {
                        let frames = conv.push(&ev);
                        if ev.event != "error" && ctx.resp_bytes < RESPONSE_LOG_MAX_BYTES {
                            let n = ev.data.to_string().len();
                            ctx.resp_bytes = ctx.resp_bytes.saturating_add(n);
                            ctx.resp_events.push(ev);
                        }
                        for f in frames {
                            ctx.pending.push_back(wire_event(f));
                        }
                        continue;
                    }
                    let out = match ev.to_wire() {
                        Ok(_) => {
                            // 转发本就要把 data 序列化成字符串,这里复用它:既当字节预算度量,又喂给下游,
                            // 避免二次 to_string。模型回复采集(#③):error 不入(非回复内容);
                            // 按累计字节封顶(真正的内存护栏,挡少量超大事件),触顶停采。
                            let data = ev.data.to_string();
                            if ev.event != "error" && ctx.resp_bytes < RESPONSE_LOG_MAX_BYTES {
                                ctx.resp_bytes = ctx.resp_bytes.saturating_add(data.len());
                                ctx.resp_events.push(ev.clone());
                            }
                            Event::default().event(ev.event).data(data)
                        }
                        Err(e) => {
                            // 序列化失败也算本次响应损坏 → 收尾按失败上报(审查 Architect#9)。
                            ctx.saw_error = true;
                            Event::default().event("error").data(
                                serde_json::json!({"type":"error","error":{
                                    "type":"api_error",
                                    "message": format!("serialize sse: {e}")}})
                                    .to_string(),
                            )
                        }
                    };
                    return Some((Ok::<Event, std::convert::Infallible>(out), ctx));
                }
                Some(Ok(StreamItem::Usage(u))) => {
                    tracing::debug!(
                        account = %ctx.account_id,
                        input = u.input_tokens,
                        output = u.output_tokens,
                        cache_read = u.cache_read_tokens,
                        "chat usage (缓存待流终态落库 #130)"
                    );
                    // 缓存终结用量,流干净/失败收尾时统一落库(success 以最终状态为准)。
                    ctx.last_usage = Some(u);
                    continue; // 不转发客户端,取下一个。
                }
                Some(Err(e)) => {
                    ctx.saw_error = true; // 硬错误 → 本次响应失败(usage success=false)。
                    // 对外只发中性文案,原文只此一处落日志(首包已出,主循环的 chat 失败
                    // 告警不会再覆盖流中错误)。
                    tracing::warn!(
                        account = %ctx.account_id,
                        model = %ctx.model,
                        kind = ?e.kind,
                        "流中上游错误: {e}"
                    );
                    if ctx.error_kind.is_none() {
                        ctx.error_kind = Some(format!("{:?}", e.kind));
                    }
                    if !ctx.reported {
                        ctx.reported = true;
                        ctx.st.scheduler.report_failure_with_gen(
                            &ctx.account_id,
                            e.kind,
                            ctx.suspend_gen,
                        );
                    }
                    if let Some(conv) = ctx.conv.as_mut() {
                        for f in conv.fail(&sse_error_payload(&e)) {
                            ctx.pending.push_back(wire_event(f));
                        }
                        ctx.drained = true;
                        continue;
                    }
                    return Some((Ok(sse_error_event(&e)), ctx));
                }
                None => {
                    // 流正常结束。收尾(生命周期上报 + usage 落库)由 StreamCtx::drop 统一处理,
                    // 与"客户端中断致响应体被 drop"走同一条路径,避免两处逻辑分叉。
                    //
                    // OpenAI 线缆多一步:补齐终止序列(末帧 / 未收口的条目 / [DONE])。
                    // 上游没发 message_stop 就断掉时,这一步是客户端唯一能拿到收尾的地方;
                    // 已经收过尾则 `finish()` 幂等返回空。
                    if let Some(conv) = ctx.conv.as_mut() {
                        let frames = conv.finish();
                        ctx.drained = true;
                        if !frames.is_empty() {
                            for f in frames {
                                ctx.pending.push_back(wire_event(f));
                            }
                            continue;
                        }
                    }
                    return None;
                }
            }
        }
    });

    Sse::new(sse).into_response()
}

fn load_yaml<T: serde::de::DeserializeOwned>(path: &Path) -> anyhow::Result<T> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("读取 {} 失败: {e}", path.display()))?;
    Ok(serde_yaml::from_str(&text)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gw_core::provider::ChatUsage;
    use gw_core::store::{UsageRecord, UsageSink};
    use std::collections::BTreeMap;

    /// 空 content 必须在入站被拦下:上游对这类请求回含糊的 "Improperly formed request",
    /// 客户查不出是哪条消息;放行还会白占一个账号的并发槽。
    /// (2026-08-02 实测:失败样本命中 8/173,成功样本 0/400 —— 零假阳性。)
    #[test]
    fn empty_message_content_is_rejected_with_index_and_role() {
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": ""}
            ]
        });
        let err = validate_message_contents(&body).expect_err("空 content 应被拒");
        assert!(err.contains("messages[2]"), "应点名下标,实际: {err}");
        assert!(err.contains("role=user"), "应点名角色,实际: {err}");
    }

    #[test]
    fn empty_content_variants_all_rejected() {
        for (label, content) in [
            ("空串", serde_json::json!("")),
            ("纯空白", serde_json::json!("   \n ")),
            ("空数组", serde_json::json!([])),
            ("空 text 块", serde_json::json!([{"type": "text", "text": ""}])),
            (
                "text 块缺 text 字段",
                serde_json::json!([{"type": "text"}]),
            ),
            ("null", serde_json::Value::Null),
        ] {
            let body = serde_json::json!({"messages": [{"role": "user", "content": content}]});
            assert!(
                validate_message_contents(&body).is_err(),
                "{label} 应被判为空 content"
            );
        }
        // content 字段整个缺席
        let body = serde_json::json!({"messages": [{"role": "user"}]});
        assert!(validate_message_contents(&body).is_err(), "缺 content 应被拒");
    }

    #[test]
    fn non_empty_and_non_text_blocks_pass() {
        // 只带图片、没有文字的消息是合法的,不能误伤。
        let img = serde_json::json!({"messages": [{"role": "user", "content": [
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}}
        ]}]});
        assert!(validate_message_contents(&img).is_ok(), "纯图片消息应放行");

        // tool_result / tool_use 同理。
        let tool = serde_json::json!({"messages": [{"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "tu_1", "content": "done"}
        ]}]});
        assert!(validate_message_contents(&tool).is_ok(), "纯 tool_result 应放行");

        // 正常文本 + 图片混排。
        let mixed = serde_json::json!({"messages": [{"role": "user", "content": [
            {"type": "text", "text": "看这张图"},
            {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "iVBOR"}}
        ]}]});
        assert!(validate_message_contents(&mixed).is_ok(), "文本+图片应放行");

        // 没有 messages 字段:不归本校验管,放行交给下游。
        let none = serde_json::json!({"model": "claude-opus-5"});
        assert!(validate_message_contents(&none).is_ok(), "缺 messages 不该在此报错");
    }

    #[test]
    fn switch_cap_benign_descends_full_ladder_risky_capped() {
        // 良性冷却类(限速/额度耗尽):解绑紧上限,沿优先级阶梯下探到组内所有号(cap=total),
        // 使高优先级层全限流时落到低优先级兜底池,而非把限流错误抛回客户端。
        assert_eq!(switch_cap(UpstreamErrorKind::RateLimited, 12, 2), 12);
        assert_eq!(switch_cap(UpstreamErrorKind::QuotaExhausted, 12, 2), 12);
        // ModelNotAvailable 亦良性:遍历组内找到有该模型的号(cap=total)。
        assert_eq!(switch_cap(UpstreamErrorKind::ModelNotAvailable, 12, 2), 12);
        // 风险类换号错误:仍守 general_cap(默认 2),防换号雪崩封号。
        assert_eq!(switch_cap(UpstreamErrorKind::ServerError, 12, 2), 2);
        assert_eq!(switch_cap(UpstreamErrorKind::TokenInvalid, 12, 2), 2);
        assert_eq!(switch_cap(UpstreamErrorKind::Network, 12, 2), 2);
        assert_eq!(switch_cap(UpstreamErrorKind::Other, 12, 2), 2);
        // total=0 兜底 max(1),不产生"0 上限=一次都不换"的退化。
        assert_eq!(switch_cap(UpstreamErrorKind::RateLimited, 0, 2), 1);
    }

    #[test]
    fn region_correction_from_arn_returns_new_region_on_mismatch() {
        let arn = "arn:aws:codewhisperer:eu-central-1:663804501012:profile/YNQNP9NWWVCQ";
        assert_eq!(
            region_correction_from_arn(arn, Some("us-east-1")),
            Some("eu-central-1"),
            "号商导出写死 us-east-1,但 profileArn 揭示真实服务区是 eu-central-1 → 应予修正"
        );
    }

    #[test]
    fn region_correction_from_arn_returns_none_when_matching() {
        let arn = "arn:aws:codewhisperer:us-east-1:881967719131:profile/Q4C9VNXVKREP";
        assert_eq!(
            region_correction_from_arn(arn, Some("us-east-1")),
            None,
            "已一致 → 不该产生多余的修正/持久化"
        );
    }

    #[test]
    fn region_correction_from_arn_ignores_case_of_stored_region() {
        // admin PATCH extra 整块替换不做区域大小写归一,DB 里可能存进混合大小写值;
        // 与已归一的 discovered 值等价时不该误判"不同"触发多余重写(对抗审查 Medium)。
        let arn = "arn:aws:codewhisperer:us-east-1:881967719131:profile/Q4C9VNXVKREP";
        assert_eq!(region_correction_from_arn(arn, Some("US-EAST-1")), None);
        assert_eq!(region_correction_from_arn(arn, Some("Us-East-1")), None);
    }

    #[test]
    fn region_correction_from_arn_returns_none_for_unknown_arn_region() {
        // ap-southeast-1 不在 STANDARD_PROFILE_REGIONS(us-east-1/eu-central-1)清单内,
        // 未知区不该污染已配置的已知区。
        let arn = "arn:aws:codewhisperer:ap-southeast-1:123456789012:profile/UNKNOWNXX";
        assert_eq!(region_correction_from_arn(arn, Some("us-east-1")), None);
    }

    #[test]
    fn region_correction_from_arn_corrects_when_region_missing() {
        // 账号从未设过 region(极简号商导出场景)→ 只要 ARN 区域已知,也该补上。
        let arn = "arn:aws:codewhisperer:eu-central-1:663804501012:profile/YNQNP9NWWVCQ";
        assert_eq!(region_correction_from_arn(arn, None), Some("eu-central-1"));
    }

    #[test]
    fn serialize_response_small_is_passthrough_json() {
        let msg = serde_json::json!({
            "type":"message","role":"assistant",
            "content":[{"type":"text","text":"hi"}]
        });
        let s = serialize_response_capped(&msg);
        // 正常体量:原样序列化,可被重新解析(详情页"格式化"视图据此渲染)。
        let v: serde_json::Value = serde_json::from_str(&s).expect("应是合法 JSON");
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["content"][0]["text"], "hi");
    }

    #[test]
    fn serialize_response_oversized_yields_valid_truncation_marker() {
        // 超 MAX_LOG_PAYLOAD_BYTES:不能存半截非法 JSON,必须是合法占位 + _truncated 标记。
        let huge = "x".repeat(MAX_LOG_PAYLOAD_BYTES + 1024);
        let msg = serde_json::json!({
            "type":"message","role":"assistant",
            "content":[{"type":"text","text": huge}]
        });
        let s = serialize_response_capped(&msg);
        assert!(s.len() <= MAX_LOG_PAYLOAD_BYTES, "占位体量应远小于上限");
        let v: serde_json::Value = serde_json::from_str(&s).expect("占位必须是合法 JSON");
        assert_eq!(v["_truncated"], true);
        assert_eq!(v["role"], "assistant");
    }

    #[test]
    fn jittered_secs_within_bounds_and_degenerate() {
        // 退化区间(min>=max)恒返回 min。
        assert_eq!(jittered_secs(300, 300), 300);
        assert_eq!(jittered_secs(300, 200), 300);
        // 正常区间:多次采样都落在 [min, max] 内。
        for _ in 0..256 {
            let s = jittered_secs(QUOTA_POLL_MIN_SECS, QUOTA_POLL_MAX_SECS);
            assert!(
                (QUOTA_POLL_MIN_SECS..=QUOTA_POLL_MAX_SECS).contains(&s),
                "{s} 越界 [{QUOTA_POLL_MIN_SECS}, {QUOTA_POLL_MAX_SECS}]"
            );
        }
    }

    #[tokio::test]
    async fn pending_writes_wait_idle_tracks_guards() {
        let pw = PendingWrites::new();
        assert!(
            pw.wait_idle(std::time::Duration::from_millis(10)).await,
            "无在途应立即 idle"
        );

        let guard = pw.enter();
        assert!(
            !pw.wait_idle(std::time::Duration::from_millis(50)).await,
            "guard 未释放应超时返回 false"
        );
        drop(guard);
        assert!(pw.wait_idle(std::time::Duration::from_millis(10)).await);

        // 异步任务持有 guard:完成(drop)应唤醒等待者。
        let g = pw.enter();
        let task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            drop(g);
        });
        assert!(
            pw.wait_idle(std::time::Duration::from_secs(2)).await,
            "guard drop 应唤醒 wait_idle"
        );
        task.await.unwrap();
    }

    #[tokio::test]
    async fn flush_dirty_extras_persists_rolled_token_and_clears_dirty() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .create_account(
                "acc-1",
                "G0",
                "kiro",
                1,
                r#"{"refresh_token":"rt-old","region":"us-east-1"}"#,
            )
            .unwrap();
        // 内存里已是刷新后的新 token,但 DB 回写曾失败 → 标脏。
        let mut acc = Account {
            account_id: "acc-1".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra: BTreeMap::new(),
        };
        acc.extra
            .insert("refresh_token".into(), serde_json::Value::String("rt-new".into()));
        let scheduler = AccountScheduler::new(vec![Arc::new(acc)], &Default::default());
        scheduler.mark_extra_dirty("acc-1");

        let locks: RefreshLocks = parking_lot::Mutex::new(std::collections::HashMap::new());
        flush_dirty_extras(&scheduler, &store, &locks, "停机排空").await;

        let row = store.get_account("acc-1").unwrap().unwrap();
        assert!(row.extra.contains("rt-new"), "rolling token 应已落盘: {}", row.extra);
        assert!(row.extra.contains("us-east-1"), "merge 语义:未带字段保留原值: {}", row.extra);
        assert!(scheduler.dirty_accounts().is_empty(), "落盘成功后应清脏位");

        // 并发刷新已自行落库清脏的账号:flush 锁内重查脏位后跳过,不回滚 DB。
        store
            .merge_account_extra("acc-1", r#"{"refresh_token":"rt-newer"}"#)
            .unwrap();
        flush_dirty_extras(&scheduler, &store, &locks, "二次排空").await;
        let row = store.get_account("acc-1").unwrap().unwrap();
        assert!(
            row.extra.contains("rt-newer"),
            "非脏账号不得被旧快照回滚: {}",
            row.extra
        );
    }

    /// 记录到内存的假 sink,断言 finalize_usage 的落库决策。
    struct FakeSink {
        rows: std::sync::Mutex<Vec<UsageRecord>>,
    }
    #[async_trait::async_trait]
    impl UsageSink for FakeSink {
        async fn record(&self, usage: UsageRecord) -> anyhow::Result<()> {
            self.rows.lock().unwrap().push(usage);
            Ok(())
        }
    }
    fn fake_sink() -> Arc<FakeSink> {
        Arc::new(FakeSink {
            rows: std::sync::Mutex::new(vec![]),
        })
    }

    #[tokio::test]
    async fn finalize_usage_records_when_usage_present() {
        let sink = fake_sink();
        let dyn_sink: Arc<dyn UsageSink> = sink.clone();
        let usage = ChatUsage {
            input_tokens: 100,
            output_tokens: 20,
            cache_read_tokens: 50,
            cache_creation_tokens: 7,
            ..Default::default()
        };
        finalize_usage(Some(&dyn_sink), "acct-1", "claude-x", "sk-cust", Some(&usage), true).await;
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].account_id, "acct-1");
        assert_eq!(rows[0].model, "claude-x");
        assert_eq!(rows[0].input_tokens, 100);
        assert_eq!(rows[0].output_tokens, 20);
        assert_eq!(rows[0].cache_read_tokens, 50);
        assert_eq!(rows[0].cache_creation_tokens, 7, "cache_creation 不得丢失");
        assert!(rows[0].success);
        assert_eq!(rows[0].client_key_id, "sk-cust", "客户 key 应归属落库");
    }

    #[tokio::test]
    async fn finalize_usage_skips_when_no_usage() {
        let sink = fake_sink();
        let dyn_sink: Arc<dyn UsageSink> = sink.clone();
        finalize_usage(Some(&dyn_sink), "acct-1", "m", "", None, true).await;
        assert_eq!(sink.rows.lock().unwrap().len(), 0, "无 usage 不应落库");
    }

    #[tokio::test]
    async fn finalize_usage_no_sink_is_noop() {
        // sink=None(库打开失败降级)→ 不 panic、不记录。
        let usage = ChatUsage::default();
        finalize_usage(None, "acct-1", "m", "", Some(&usage), false).await;
    }

    #[tokio::test]
    async fn finalize_usage_propagates_failure_flag() {
        let sink = fake_sink();
        let dyn_sink: Arc<dyn UsageSink> = sink.clone();
        let usage = ChatUsage {
            output_tokens: 5,
            ..Default::default()
        };
        finalize_usage(Some(&dyn_sink), "a", "m", "", Some(&usage), false).await;
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].success, "失败流应记 success=false");
    }

    fn one_account_scheduler() -> AccountScheduler {
        AccountScheduler::new(vec![Arc::new(acct(&[]))], &Default::default())
    }

    fn chat_stream(
        items: Vec<Result<StreamItem, UpstreamError>>,
    ) -> gw_core::provider::ChatStream {
        Box::pin(futures::stream::iter(items))
    }

    /// 请求日志捕获参数下的最小 ChatRequest(model="m";store=None 时不会被实际使用)。
    fn req_model_m() -> ChatRequest {
        ChatRequest::from_anthropic_body(serde_json::json!({"model":"m","messages":[]}))
    }

    async fn body_json(resp: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn collect_response_folds_nonstream_message_and_persists_usage() {
        let sched = one_account_scheduler();
        let lease = sched.acquire(Some("s")).await.unwrap();
        let sink = fake_sink();
        let dyn_sink: Arc<dyn UsageSink> = sink.clone();
        let items = vec![
            Ok(StreamItem::Sse(SseEvent::new(
                "message_start",
                serde_json::json!({"message":{"id":"msg_1","type":"message","role":"assistant","model":"m","content":[],"usage":{"input_tokens":4,"output_tokens":0}}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new(
                "content_block_start",
                serde_json::json!({"index":0,"content_block":{"type":"text","text":""}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new(
                "content_block_delta",
                serde_json::json!({"index":0,"delta":{"type":"text_delta","text":"hi"}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new("content_block_stop", serde_json::json!({"index":0})))),
            Ok(StreamItem::Sse(SseEvent::new(
                "message_delta",
                serde_json::json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new("message_stop", serde_json::json!({})))),
            Ok(StreamItem::Usage(ChatUsage {
                input_tokens: 4,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                ..Default::default()
            })),
        ];
        let resp =
            collect_response(&sched, Some(&dyn_sink), None, None, lease, chat_stream(items), req_model_m(), String::new(), Arc::new(acct(&[])), std::time::Instant::now(), "kiro", Wire::Anthropic).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v = body_json(resp).await;
        assert_eq!(v["content"][0]["text"], "hi", "非流式应折叠成单个 Messages JSON");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["output_tokens"], 2);
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 1, "非流式也应落库 usage");
        assert_eq!(rows[0].output_tokens, 2);
        assert!(rows[0].success);
    }

    #[tokio::test]
    async fn collect_response_maps_sse_error_to_bad_gateway() {
        let sched = one_account_scheduler();
        let lease = sched.acquire(Some("s")).await.unwrap();
        let items = vec![
            Ok(StreamItem::Sse(SseEvent::new(
                "message_start",
                serde_json::json!({"message":{"id":"m","content":[]}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new(
                "error",
                serde_json::json!({"type":"error","error":{"type":"overloaded_error","message":"x"}}),
            ))),
        ];
        let resp = collect_response(&sched, None, None, None, lease, chat_stream(items), req_model_m(), String::new(), Arc::new(acct(&[])), std::time::Instant::now(), "kiro", Wire::Anthropic).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY, "SSE error 应回非流式错误");
        let v = body_json(resp).await;
        assert_eq!(v["error"]["type"], "overloaded_error");
    }

    #[tokio::test]
    async fn collect_response_fold_failure_persists_failure() {
        // 折叠失败(缺 message_start)→ 502,且 usage 必须记 success=false(审查 #1:
        // 不能在折叠失败前抢报成功 / 记 success=true)。
        let sched = one_account_scheduler();
        let lease = sched.acquire(Some("s")).await.unwrap();
        let sink = fake_sink();
        let dyn_sink: Arc<dyn UsageSink> = sink.clone();
        let items = vec![
            Ok(StreamItem::Sse(SseEvent::new("content_block_stop", serde_json::json!({"index":0})))),
            Ok(StreamItem::Usage(ChatUsage {
                input_tokens: 5,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                ..Default::default()
            })),
        ];
        let resp =
            collect_response(&sched, Some(&dyn_sink), None, None, lease, chat_stream(items), req_model_m(), String::new(), Arc::new(acct(&[])), std::time::Instant::now(), "kiro", Wire::Anthropic).await;
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let rows = sink.rows.lock().unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].success, "折叠失败的请求 usage 应记 success=false");
    }

    fn acct(extra: &[(&str, &str)]) -> Account {
        let mut map = BTreeMap::new();
        for (k, v) in extra {
            map.insert((*k).to_string(), serde_json::Value::String((*v).to_string()));
        }
        Account {
            account_id: "a".into(),
            provider: "kiro".into(),
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra: map,
        }
    }

    #[test]
    fn loopback_listen_detection() {
        assert!(is_loopback_listen("127.0.0.1:9000"));
        assert!(is_loopback_listen("localhost:9000"));
        assert!(is_loopback_listen("[::1]:9000"));
        assert!(!is_loopback_listen("0.0.0.0:9000"));
        assert!(!is_loopback_listen("203.0.113.10:9000"));
        assert!(!is_loopback_listen("nonsense"));
    }

    #[test]
    fn parse_rfc3339_known_values() {
        // 与 token.rs format_unix_utc 互逆。
        assert_eq!(parse_rfc3339_unix("2026-06-04T00:00:00Z"), Some(1_780_531_200));
        assert_eq!(parse_rfc3339_unix("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_rfc3339_rejects_garbage() {
        assert_eq!(parse_rfc3339_unix("not-a-date"), None);
        assert_eq!(parse_rfc3339_unix(""), None);
    }

    #[test]
    fn no_token_is_not_fresh() {
        assert!(!has_fresh_token(&acct(&[])));
        assert!(!has_fresh_token(&acct(&[("access_token", "")])));
    }

    #[test]
    fn token_without_expiry_is_fresh() {
        // 无 expires_at → 当作有效(靠 403 兜底)。
        assert!(has_fresh_token(&acct(&[("access_token", "t")])));
    }

    #[test]
    fn needs_profile_discovery_paid_missing_arn() {
        // 付费(非 FREE)+ 无 profile_arn → 需要强制发现真实 ARN。
        assert!(needs_profile_discovery(&acct(&[(
            "subscription_title",
            "KIRO POWER"
        )])));
        // 空白 profile_arn 视同缺失。
        assert!(needs_profile_discovery(&acct(&[
            ("subscription_title", "KIRO PRO"),
            ("profile_arn", "   "),
        ])));
    }

    #[test]
    fn needs_profile_discovery_free_or_present_arn_skips() {
        // 免费号:即便结构同构(带 client_secret、无 arn)也不发现,维持共享 ARN。
        assert!(!needs_profile_discovery(&acct(&[
            ("subscription_title", "KIRO FREE"),
            ("client_secret", "cs"),
        ])));
        // 订阅档未回填(缺 subscription_title)→ 保守跳过,绝不误污染健康免费号。
        assert!(!needs_profile_discovery(&acct(&[("client_secret", "cs")])));
        // 已有真实 profile_arn → 绝不覆盖。
        assert!(!needs_profile_discovery(&acct(&[
            ("subscription_title", "KIRO POWER"),
            (
                "profile_arn",
                "arn:aws:codewhisperer:us-east-1:1:profile/X"
            ),
        ])));
    }


    #[test]
    fn expired_token_is_not_fresh() {
        // 过去时刻 → 需刷新。
        assert!(!has_fresh_token(&acct(&[
            ("access_token", "t"),
            ("expires_at", "2000-01-01T00:00:00Z"),
        ])));
    }

    #[test]
    fn far_future_token_is_fresh() {
        assert!(has_fresh_token(&acct(&[
            ("access_token", "t"),
            ("expires_at", "2099-01-01T00:00:00Z"),
        ])));
    }

    #[test]
    fn api_key_account_is_always_fresh() {
        // apikey 长期有效:即便标了过去的 expires_at,也不触发刷新。
        assert!(has_fresh_token(&acct(&[
            ("kiro_api_key", "ksk_abc"),
            ("access_token", "ksk_abc"),
            ("expires_at", "2000-01-01T00:00:00Z"),
        ])));
    }

    #[test]
    fn prepare_extracts_image_blob_keeps_text() {
        // Anthropic 图片块:source.data 抽到 blob,报文里换 blob 引用;text 正文原样保留。
        let big = "A".repeat(2048);
        let raw = serde_json::json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "看这张图,顺便说下 a+b/c=d 是什么"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": big}}
                ]
            }]
        })
        .to_string();
        let (out, blobs) = prepare_log_payload(raw);
        assert_eq!(blobs.len(), 1, "应抽出 1 个图片 blob");
        assert_eq!(blobs[0].media_type, "image/png");
        assert!(out.contains("看这张图") && out.contains("a+b/c=d"), "正文完整保留");
        assert!(out.contains(&format!("blob:{}", blobs[0].hash)), "data 换成 blob 引用");
        assert!(!out.contains(&"A".repeat(2048)), "原始 base64 不入报文");
    }

    #[test]
    fn prepare_passes_through_non_json() {
        // 非 JSON(如转换错误串)原样保留、无 blob(短串不触顶截断)。
        let raw = "<转换失败: bad request>".to_string();
        let (out, blobs) = prepare_log_payload(raw.clone());
        assert_eq!(out, raw);
        assert!(blobs.is_empty());
    }

    // ──────────────────────────────────────────────────────────────────────
    // Phase 6 regression: write_request_log 守卫按 worker family 而非 account.provider
    // ──────────────────────────────────────────────────────────────────────

    /// 辅助:用内存 SQLite 调 write_request_log,返回落库的 kiro_payload。
    fn log_kiro_payload_with_family(family: &str) -> String {
        use gw_core::store::RequestLogFilter;

        let store = Arc::new(SqliteStore::open_in_memory().unwrap());
        // 空 provider 字段:模拟 filter_by_provider 放行的 kiro 账号(或 dario 账号),
        // 守卫不应依赖此字段。
        let account = Arc::new(Account {
            account_id: "test-acc".into(),
            provider: String::new(), // 故意留空
            max_concurrency: 1,
            disabled: false,
            created_at: 0,
            extra: BTreeMap::new(),
        });
        let req = req_model_m();
        write_request_log(
            store.clone(),
            req,
            Some(account),
            "sk-test".into(),
            false,
            true,
            Some(200),
            None,
            Some(10),
            None,
            None,
            ResponseLog::None,
            family,
        );
        let rows = store
            .list_request_logs(&RequestLogFilter::default(), 10)
            .unwrap();
        assert_eq!(rows.len(), 1, "应落库恰好 1 条");
        let detail = store.get_request_log(rows[0].id).unwrap().unwrap();
        detail.kiro_payload
    }

    #[test]
    fn write_request_log_kiro_family_renders_kiro_payload_even_with_empty_provider() {
        // family="kiro" 时 kiro_payload 必须非空,且 account.provider 为空不影响渲染
        // (防止 filter_by_provider 放行的空-provider kiro 号丢日志)。
        let kp = log_kiro_payload_with_family("kiro");
        assert!(
            !kp.is_empty(),
            "family=kiro 时 kiro_payload 应非空,实际为空"
        );
        // render_kiro_payload 对最简请求(model=m, messages=[]) 应产出合法 JSON
        assert!(
            kp.starts_with('{') || kp.starts_with('<'),
            "kiro_payload 应是 JSON 对象或错误占位,实际: {kp}"
        );
    }

    #[test]
    fn write_request_log_non_kiro_family_skips_kiro_payload() {
        // family="claude-dario" 时 kiro_payload 必须为空串
        // (不应把 Kiro 格式强行渲染给 dario/其他 provider)。
        let kp = log_kiro_payload_with_family("claude-dario");
        assert!(
            kp.is_empty(),
            "family=claude-dario 时 kiro_payload 应为空串,实际: {kp}"
        );
    }

    /// 造一个只用于窗口判定的调度器(不发请求,只读写 model_overloaded)。
    fn window_sched() -> AccountScheduler {
        let acc = Arc::new(Account {
            account_id: "a".into(),
            provider: "kiro".into(),
            max_concurrency: 4,
            disabled: false,
            created_at: 0,
            extra: Default::default(),
        });
        AccountScheduler::new(vec![acc], &gw_core::config::SchedulerConfig::default())
    }

    #[test]
    fn generic_5xx_upgraded_only_inside_overload_window() {
        use UpstreamErrorKind as K;
        let s = window_sched();

        // 窗口外:通用 5xx 必须保持 ServerError(仍换号 + 仍记账号失败),绝不靠猜升级。
        assert_eq!(
            correct_overload_kind(&s, "claude-opus-5", K::ServerError),
            K::ServerError,
            "没有上游显式信号时不得把通用 5xx 当过载"
        );

        // 收到显式过载 → 开窗,且自身仍是 Overloaded。
        assert_eq!(
            correct_overload_kind(&s, "claude-opus-5", K::Overloaded),
            K::Overloaded
        );
        // 窗口内:同一模型的通用 5xx 升级为 Overloaded。
        assert_eq!(
            correct_overload_kind(&s, "claude-opus-5", K::ServerError),
            K::Overloaded,
            "窗口内的通用 5xx 应按过载处理"
        );
        // **窗口是按模型隔离的**:opus-5 过载不能让 opus-4-8 的 5xx 也免罚,
        // 否则一个模型抖动会掩盖另一个模型的真故障。
        assert_eq!(
            correct_overload_kind(&s, "claude-opus-4-8", K::ServerError),
            K::ServerError,
            "过载窗口不得跨模型泄漏"
        );
    }

    #[test]
    fn overload_window_never_touches_other_kinds() {
        use UpstreamErrorKind as K;
        let s = window_sched();
        correct_overload_kind(&s, "m", K::Overloaded); // 开窗
        // 窗口内也只升级 ServerError;别的 kind 各有自己的处置(禁号/冷却/400),不能被吞掉。
        for k in [K::TokenInvalid, K::RateLimited, K::TemporarilyBlocked, K::QuotaExhausted,
                  K::Network, K::BadRequest, K::ModelNotAvailable, K::EmptyResponse, K::Other] {
            assert_eq!(correct_overload_kind(&s, "m", k), k, "{k:?} 不应被窗口改写");
        }
    }

    #[test]
    fn overloaded_maps_to_529_others_unchanged() {
        use UpstreamErrorKind as K;
        assert_eq!(upstream_status(K::Overloaded).as_u16(), 529, "过载须走 Anthropic 的 529");
        // 既有映射一个都不能变。
        assert_eq!(upstream_status(K::BadRequest), StatusCode::BAD_REQUEST);
        assert_eq!(upstream_status(K::ModelNotAvailable), StatusCode::BAD_REQUEST);
        for k in [K::ServerError, K::Network, K::RateLimited, K::QuotaExhausted,
                  K::TokenInvalid, K::TemporarilyBlocked, K::EmptyResponse, K::Other] {
            assert_eq!(upstream_status(k), StatusCode::BAD_GATEWAY, "{k:?} 应保持 502");
        }
    }

    #[tokio::test]
    async fn overloaded_response_carries_anthropic_error_type() {
        async fn body_json(resp: axum::response::Response) -> serde_json::Value {
            let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
            serde_json::from_slice(&bytes).unwrap()
        }

        let e = UpstreamError::new(UpstreamErrorKind::Overloaded, "high load").with_status(500);
        let resp = upstream_error_response(&e);
        assert_eq!(resp.status().as_u16(), 529);
        // 客户端(Claude Code / SDK)按 error.type == "overloaded_error" 判定可重试。
        let v = body_json(resp).await;
        assert_eq!(v["error"]["type"], "overloaded_error");
        assert_eq!(v["type"], "error");

        // ⚠️ **这里原先断言的是「非过载错误不带 error.type」——那条断言把一个 bug 钉住了。**
        //
        // Anthropic 的错误体里 `error.type` 是必填。少了它,按 schema 严格校验的客户端
        // (opencode 用 zod)会在**解析错误体时**就失败,把真正的 message 吞掉,
        // 客户只看到一句 "Invalid input: expected string, received undefined"
        // (2026-08-07 实测)。原来的顾虑是"造错 type 反而误导客户端重试" —— 顾虑本身
        // 合理,但结论反了:根本没法解析比 type 不够精确糟得多。`api_error` 是最保守的
        // 取值(服务端问题、可重试),而且与我们已经返回的 502 状态码一致。
        let se = UpstreamError::new(UpstreamErrorKind::ServerError, "boom").with_status(500);
        let sresp = upstream_error_response(&se);
        assert_eq!(sresp.status(), StatusCode::BAD_GATEWAY);
        let sv = body_json(sresp).await;
        assert_eq!(sv["error"]["type"], "api_error", "502 那一类必须给 api_error");
        assert!(sv["error"]["message"].is_string());
    }

    /// 上游身份指纹**一个字都不许**出现在对外响应里 —— 客户看不出这条渠道背后是谁,
    /// 定价才谈得下去。三条对外出口(非流式响应 / 流内 error 事件 / 选号失败)全覆盖。
    #[tokio::test]
    async fn client_facing_errors_carry_no_upstream_fingerprint() {
        /// 真实报文(逐字取自 139 `caio-worker0` 日志)——脱敏前客户就是收到这一串。
        const REAL: &str = concat!(
            "kiro generateAssistantResponse 失败: 429 ",
            r#"{"message":"Too many requests, please wait before trying again.","#,
            r#""reason":"USER_REQUEST_RATE_EXCEEDED"}"#
        );
        const FINGERPRINTS: [&str; 5] = [
            "kiro",
            "Kiro",
            "generateAssistantResponse",
            "USER_REQUEST_RATE_EXCEEDED",
            "rate_limited", // 内部 kind 标签也别外泄
        ];

        let e = UpstreamError::new(UpstreamErrorKind::RateLimited, REAL).with_status(429);

        // ① 非流式 / 首包前:HTTP 响应体。
        let resp = upstream_error_response(&e);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let http_body = String::from_utf8(bytes.to_vec()).unwrap();

        // ② 流内:SSE error 事件的 data(与 sse_error_event 下发的是同一份)。
        let sse_data = sse_error_payload(&e).to_string();

        // ③ 选号失败:AcquireError 的对外文案。
        let acquire = [
            scheduler::AcquireError::AllDisabled,
            scheduler::AcquireError::AllBusy,
            scheduler::AcquireError::AllRpmLimited,
            scheduler::AcquireError::Empty,
            scheduler::AcquireError::GroupEmpty,
            scheduler::AcquireError::NoModelSupport,
        ]
        .iter()
        .map(|a| a.client_message())
        .collect::<Vec<_>>()
        .join(" | ");

        for (label, text) in [("http", &http_body), ("sse", &sse_data), ("acquire", &acquire)] {
            for fp in FINGERPRINTS {
                assert!(!text.contains(fp), "{label} 出口泄露了 `{fp}`: {text}");
            }
            // 账号池形态同样不外露(分组/账号/worker 这类词是渠道形态的直接线索)。
            for fp in ["账号", "分组", "worker", "订阅"] {
                assert!(!text.contains(fp), "{label} 出口泄露了池形态 `{fp}`: {text}");
            }
        }
        // 反向:运维面必须**仍能**看到原文,否则导号/排查就瞎了。
        let admin = admin_error_response(&e);
        let ab = axum::body::to_bytes(admin.into_body(), 64 * 1024).await.unwrap();
        let admin_body = String::from_utf8(ab.to_vec()).unwrap();
        assert!(
            admin_body.contains("generateAssistantResponse") && admin_body.contains("kiro"),
            "运维面不该脱敏: {admin_body}"
        );
    }

    /// 上游把错误当**普通 SSE 事件**发来时(dario 直透 Anthropic 流),这条路绕开
    /// `UpstreamError`。对抗评审两个镜头都点了它 —— 闸门必须也盖在这里。
    #[test]
    fn upstream_error_event_payload_is_rewritten_but_keeps_retry_type() {
        let leaky = serde_json::json!({
            "type": "error",
            "error": {
                "type": "overloaded_error",
                "message": r#"kiro generateAssistantResponse 失败: {"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}"#
            }
        });
        let out = sanitize_upstream_error_payload(&leaky);
        let s = out.to_string();
        for fp in ["kiro", "generateAssistantResponse", "MODEL_TEMPORARILY_UNAVAILABLE"] {
            assert!(!s.contains(fp), "上游 error 事件仍泄露 `{fp}`: {s}");
        }
        // **type 必须原样保留**:客户端 SDK 按它判可重试,改了就是改重试语义(本次不做)。
        assert_eq!(out["error"]["type"], "overloaded_error");
        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["message"], "服务繁忙,请稍后重试");

        // 上游没给 type 的载荷:**必须补一个**,而不是留空。
        //
        // 这条原先断言"不该凭空造 error.type",理由是"造错反而误导客户端重试"。
        // 实测推翻了它:留空的后果是客户端连错误体都解析不了(见上面
        // `overloaded_response_carries_anthropic_error_type` 里的说明)。补 `api_error`
        // 是保守解 —— 语义是"服务端出问题、可重试",不指认客户的 key/请求有问题。
        let untyped = serde_json::json!({"error":{"message":"kiro boom"}});
        let out2 = sanitize_upstream_error_payload(&untyped);
        assert_eq!(out2["error"]["type"], "api_error", "缺 type 必须补,否则客户端解析不了");
        assert!(!out2.to_string().contains("kiro"));

        // 各 type 映射到语义相符的中性文案(反向:别全塞同一句)。
        let by_type = |t: &str| {
            sanitize_upstream_error_payload(&serde_json::json!({"error":{"type":t,"message":"x"}}))
                ["error"]["message"]
                .as_str()
                .unwrap()
                .to_string()
        };
        assert_eq!(by_type("rate_limit_error"), "请求过于频繁,请稍后重试");
        assert_eq!(by_type("invalid_request_error"), "请求无效,请检查请求体后重试");
        assert_ne!(by_type("rate_limit_error"), by_type("overloaded_error"));
    }

    /// 登记过的本地文案要照发到客户端 —— 脱敏不能把"你请求体太大"这类可自助的信息也吃掉。
    #[tokio::test]
    async fn locally_generated_detail_still_reaches_client() {
        let e = UpstreamError::bad_request_visible("请求体 100 字节超出体积上限 50 字节");
        let resp = upstream_error_response(&e);
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains("超出体积上限"), "本地可见文案被误吞: {body}");
    }

    #[test]
    fn overload_backoff_is_bounded_and_jittered_upward() {
        // 退避梯度必须递增且总时长有界:上界过大会让客户端干等,不如直接吐 529。
        assert!(OVERLOAD_BACKOFF_MS.windows(2).all(|w| w[0] < w[1]), "退避应递增");
        let total: u64 = OVERLOAD_BACKOFF_MS.iter().sum();
        assert!(total <= 4_000, "总退避 {total}ms 超过 4s,客户端体感太差");

        // 抖动:永不小于基值(不能把退避抖没了),且不超过 +MAX_JITTER_PCT%。
        // 取多次样本以覆盖 uuid 熵的分布,同时验证确实产生了不同的值。
        let hi = 1_000 + 1_000 * MAX_JITTER_PCT / 100;
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..200 {
            let d = jittered(1_000).as_millis() as u64;
            assert!((1_000..=hi).contains(&d), "抖动后 {d}ms 越界(应在 1000..={hi})");
            seen.insert(d);
        }
        // 最坏情况总退避(全部打满抖动)也要有界,否则客户端可能干等太久。
        let worst: u64 = OVERLOAD_BACKOFF_MS.iter()
            .map(|b| b + b * MAX_JITTER_PCT / 100)
            .sum();
        assert!(worst <= 5_000, "最坏总退避 {worst}ms 超过 5s");
        assert!(seen.len() > 1, "抖动没起作用(200 次采样只有一个值),并发请求会同步重撞");
    }

    // ───────── 分组头解析 ─────────

    fn hdrs(v: Option<&str>) -> axum::http::HeaderMap {
        let mut h = axum::http::HeaderMap::new();
        if let Some(v) = v {
            h.insert(crate::GROUP_HEADER, v.parse().unwrap());
        }
        h
    }

    /// 头缺席 = 未分组请求:必须是 None,调用方据此走与本次重构之前完全相同的路径
    /// (全量池)。滚动升级期间旧 router 不发这个头,靠的就是这条。
    #[test]
    fn group_header_absent_is_none() {
        assert_eq!(parse_group_header(&hdrs(None)).unwrap(), None);
    }

    /// 正常解析:组名原样取出,首尾空白裁掉。
    #[test]
    fn group_header_parses_name() {
        assert_eq!(parse_group_header(&hdrs(Some("GECO"))).unwrap().as_deref(), Some("GECO"));
        assert_eq!(parse_group_header(&hdrs(Some("  G0 "))).unwrap().as_deref(), Some("G0"));
    }

    /// 空头必须**报错而不是当成缺席** —— 当成缺席等于把一个本该受成员集限制的请求
    /// 以全量池放行,静默提权且无任何痕迹。
    #[test]
    fn group_header_empty_is_error_not_ignored() {
        for bad in ["", "   "] {
            assert!(
                parse_group_header(&hdrs(Some(bad))).is_err(),
                "空分组头 {bad:?} 必须拒绝,绝不能静默按未分组放行"
            );
        }
    }

    /// 会话亲和键必须按分组分命名空间,否则低价流量能改写正常客户的账号钉扎
    /// (`select_id` 的"primary 不合格 → 改选并当场转正、永不迁回")。
    #[test]
    fn affinity_key_is_namespaced_per_group() {
        let plain = group_scoped_affinity_key(None, None, Some("sess-1".into()));
        let scoped = group_scoped_affinity_key(Some("GECO"), None, Some("sess-1".into()));
        assert_eq!(plain.as_deref(), Some("sess-1"), "未分组请求的亲和键必须原样不变");
        assert_ne!(plain, scoped, "同一会话键在两个组下必须落到不同的亲和条目");
        assert_eq!(scoped.as_deref(), Some("GECO\u{0}sess-1"));
        // 两个不同的组也必须互不干扰。
        assert_ne!(
            group_scoped_affinity_key(Some("G0"), None, Some("sess-1".into())),
            scoped,
            "不同分组的同名会话不得共用一个钉扎"
        );
        // 无会话键(无亲和记忆)时都是 None,不得凭空造出一个键。
        assert_eq!(group_scoped_affinity_key(Some("GECO"), None, None), None);
    }

    /// **跨客户串话的闸门。**
    ///
    /// cursor 的 conversation_id 是纯内容派生的,而 `ConvRegistry` 在
    /// (conversation_id, account) 双命中时会走服务端续写 —— 两个不同客户拿到同一个键,
    /// 后来那位就会续在前一位的会话上。所以客户维度必须进命名空间。
    #[test]
    fn cursor_的亲和键按客户_key_再分一层() {
        assert!(affinity_scoped_by_client("cursor"));
        // 没有服务端续写的家族不分层:分了只会无谓打散主链路的缓存亲和。
        for f in ["kiro", "claude-dario", "claude-subprocess", ""] {
            assert!(!affinity_scoped_by_client(f), "{f} 不该按客户分层");
        }

        // 同组、同内容键、**不同客户** → 必须是两个不同的亲和条目。
        let a = group_scoped_affinity_key(Some("CUR"), Some("key-a"), Some("sess-1".into()));
        let b = group_scoped_affinity_key(Some("CUR"), Some("key-b"), Some("sess-1".into()));
        assert_ne!(a, b, "同一开场白的两个客户不得共用 conversation_id");
        // 同一个客户的同一会话必须稳定(否则每轮换 conversation_id,续写全废)。
        assert_eq!(
            a,
            group_scoped_affinity_key(Some("CUR"), Some("key-a"), Some("sess-1".into()))
        );
        // 客户 key 缺席(未分组 / 无 router 头)→ 退回旧行为,不凭空多加分隔符。
        assert_eq!(
            group_scoped_affinity_key(Some("CUR"), Some(""), Some("sess-1".into())).as_deref(),
            Some("CUR\u{0}sess-1")
        );
    }

    /// 保活帧必须**始终**用 `content_block_delta` 这个事件名。
    ///
    /// 这是整个保活机制成立的前提,单独锁一个测试:下游 NewAPI 的 Claude→OpenAI 转换器
    /// 只识别 message_start / content_block_{start,delta} / message_{delta,stop} 五种事件,
    /// **且没有 default 兜底**。谁要是"顺手简化"成官方的 `ping`,保活帧会在 NewAPI 处
    /// 静默消失、一个下游事件都不产生,客户端照样卡死——而且不会有任何报错提示你。
    #[test]
    fn keepalive_frames_use_an_event_newapi_can_convert() {
        for open in [
            OpenBlock::Text(0),
            OpenBlock::Thinking(3),
            OpenBlock::ToolUse(7),
        ] {
            let f = keepalive_frame(Some(open));
            assert_eq!(
                f.event, "content_block_delta",
                "{open:?} 的保活帧事件名必须是 content_block_delta,否则会被下游丢弃"
            );
        }
    }

    #[test]
    fn keepalive_frame_matches_open_block_kind_and_is_a_noop() {
        // 帧形态必须贴合当前块类型:往 tool_use 块塞 text_delta 是非法 SSE,客户端会错乱。
        // 且承载的增量一律是空串 —— 客户端逐段拼接,拼空串等于没拼,对内容零影响。
        let cases = [
            (OpenBlock::Text(0), "text_delta", "text"),
            (OpenBlock::Thinking(3), "thinking_delta", "thinking"),
            (OpenBlock::ToolUse(7), "input_json_delta", "partial_json"),
        ];
        for (open, delta_type, field) in cases {
            let f = keepalive_frame(Some(open));
            let idx = match open {
                OpenBlock::Text(i) | OpenBlock::Thinking(i) | OpenBlock::ToolUse(i) => i,
            };
            assert_eq!(f.data["index"], idx, "保活帧必须打在当前块的索引上");
            assert_eq!(f.data["delta"]["type"], delta_type);
            // 字段必须**存在且为字符串**。NewAPI 的转换器对 input_json_delta 是
            // `*claudeResponse.Delta.PartialJson` 直接解引用、**不做 nil 检查**——
            // 缺字段或给 null 会让下游网关 panic,不是只丢一帧那么简单。
            assert_eq!(
                f.data["delta"][field].as_str(),
                Some(""),
                "{delta_type} 的 {field} 必须是空字符串(不能缺失、更不能是 null)"
            );
        }
    }

    #[test]
    fn keepalive_without_open_block_falls_back_to_ping() {
        // 没有开着的块时零增量无处附着,只能退回官方 ping(对原生 Anthropic 客户端有效)。
        let f = keepalive_frame(None);
        assert_eq!(f.event, "ping");
        assert_eq!(f.data["type"], "ping");
    }

    #[test]
    fn track_open_block_follows_block_lifecycle() {
        let start = |idx: u64, ty: &str| {
            SseEvent::new(
                "content_block_start",
                serde_json::json!({"type":"content_block_start","index":idx,
                    "content_block":{"type":ty}}),
            )
        };
        let plain = |name: &str| SseEvent::new(name, serde_json::json!({"type":name}));

        assert_eq!(
            track_open_block(None, &start(0, "text")),
            Some(OpenBlock::Text(0))
        );
        assert_eq!(
            track_open_block(None, &start(1, "tool_use")),
            Some(OpenBlock::ToolUse(1))
        );
        assert_eq!(
            track_open_block(None, &start(2, "thinking")),
            Some(OpenBlock::Thinking(2))
        );
        // 没有安全零增量形态的块(如 redacted_thinking)→ 视为无块可附着,回退 ping。
        assert_eq!(track_open_block(None, &start(3, "redacted_thinking")), None);
        // 关块 / 整条消息结束都要清状态,否则会往已关闭的块发 delta(非法 SSE)。
        let open = Some(OpenBlock::ToolUse(1));
        assert_eq!(track_open_block(open, &plain("content_block_stop")), None);
        assert_eq!(track_open_block(open, &plain("message_stop")), None);
        // 无关事件不改变状态(delta 期间必须保持当前块)。
        assert_eq!(track_open_block(open, &plain("content_block_delta")), open);
        assert_eq!(track_open_block(open, &plain("message_delta")), open);
    }

    #[test]
    fn idle_thresholds_leave_margin_under_the_observed_client_timeout() {
        // 2026-07-26 取证:四次卡死的静默时长 120.5/123.3/124.8/126.0 秒 → 客户端阈值约 120s。
        const OBSERVED_CLIENT_IDLE_TIMEOUT: std::time::Duration =
            std::time::Duration::from_secs(120);
        assert!(
            STREAM_IDLE_KEEPALIVE * 4 <= OBSERVED_CLIENT_IDLE_TIMEOUT,
            "保活间隔 {:?} 对客户端 {:?} 的阈值余量不足 4 倍,上游抖动稍变就又会被砍",
            STREAM_IDLE_KEEPALIVE,
            OBSERVED_CLIENT_IDLE_TIMEOUT
        );
        // 硬上限必须显著大于保活间隔,否则还没来得及保活就先中止了。
        assert!(
            STREAM_IDLE_ABORT >= STREAM_IDLE_KEEPALIVE * 4,
            "硬上限 {:?} 太接近保活间隔 {:?}",
            STREAM_IDLE_ABORT,
            STREAM_IDLE_KEEPALIVE
        );
        // 上限也要大于观察到的客户端阈值,否则"上游只是慢"的情形永远等不到恢复,
        // 也就永远回答不了"慢还是死"。
        assert!(
            STREAM_IDLE_ABORT > OBSERVED_CLIENT_IDLE_TIMEOUT,
            "硬上限不该早于客户端自己的阈值,那样这次改动等于没做"
        );
    }

    // ───── 审查复审 [高] 回归:profileArn 修复验证被 RPM 拦不得误杀账号 ─────

    /// mock provider:chat 恒 403 TokenInvalid(记录真实发出的调用次数),
    /// refresh 成功(换新 at),强制发现 profileArn 成功。
    struct HealGateMockProvider {
        chat_calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for HealGateMockProvider {
        fn family(&self) -> &'static str {
            "kiro"
        }
        fn account_schema(&self) -> &'static [gw_core::account::FieldSpec] {
            &[]
        }
        async fn list_models(
            &self,
        ) -> Result<Vec<gw_core::model::ModelInfo>, UpstreamError> {
            Ok(vec![])
        }
        async fn chat(
            &self,
            _req: ChatRequest,
            _ctx: &CallCtx,
        ) -> Result<gw_core::provider::ChatStream, UpstreamError> {
            self.chat_calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Err(UpstreamError::new(UpstreamErrorKind::TokenInvalid, "mock 403"))
        }
        async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
            let mut a = account.clone();
            a.extra.insert("access_token".into(), serde_json::json!("at-new"));
            a.extra
                .insert("expires_at".into(), serde_json::json!("2099-01-01T00:00:00Z"));
            Ok(a)
        }
        async fn force_discover_profile_arn(
            &self,
            _account: &Account,
        ) -> Result<Option<String>, UpstreamError> {
            Ok(Some(
                "arn:aws:codewhisperer:us-east-1:123456789012:profile/TESTPROFILE".into(),
            ))
        }
    }

    /// profileArn 发现成功、但修复后的验证调用被 RPM 闸门拦住(暖机一期 RPM=2 的
    /// 确定性路径:首发占 1 格 → 刷新重试占第 2 格 → 验证调用达限):
    /// **不得**把修复前的 e2 按 TokenInvalid 上报 —— 那会把刚修好的号永久误禁
    /// (invalid_refresh_token)。被拦 ≠ 修复失败:换号/透错,不上报、不禁用。
    #[tokio::test]
    async fn heal_retry_blocked_by_rpm_does_not_report_or_disable_account() {
        let mut extra = BTreeMap::new();
        extra.insert("access_token".into(), serde_json::json!("at-old"));
        extra.insert("refresh_token".into(), serde_json::json!("rt-1"));
        extra.insert("expires_at".into(), serde_json::json!("2099-01-01T00:00:00Z"));
        // 付费非 FREE + 无 profile_arn → needs_profile_discovery 成立,走强制发现。
        extra.insert("subscription_title".into(), serde_json::json!("KIRO PRO"));
        // 有效 RPM = 2:首发预留 + 刷新重试各占一格,验证调用必被拦。
        extra.insert("rpm_limit".into(), serde_json::json!(2));
        let acc = Arc::new(Account {
            account_id: "a".into(),
            provider: "kiro".into(),
            max_concurrency: 2,
            disabled: false,
            created_at: 0,
            extra,
        });
        let chat_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn Provider> =
            Arc::new(HealGateMockProvider { chat_calls: chat_calls.clone() });
        let st = Arc::new(WorkerState {
            instance: 0,
            egress_desc: String::new(),
            group: String::new(),
            provider,
            settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
            scheduler: AccountScheduler::new(
                vec![acc],
                &gw_core::config::SchedulerConfig::default(),
            ),
            refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
            usage_sink: None,
            pending_writes: PendingWrites::new(),
            store: None,
            quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
            quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
            quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
            sync_lock: tokio::sync::Mutex::new(()),
            group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
            _client: reqwest::Client::new(),
        });
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 16,
            "stream": true,
            "messages": [{"role": "user", "content": "hi"}],
        });
        let resp = messages(State(st.clone()), HeaderMap::new(), Json(body)).await;
        // 单号池:attempts(1) 已触 switch_cap → 透原始 403 的对外映射(502)。
        // 关键不是状态码,而是下面两条:没发第三次调用、号没被上报禁用。
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(
            chat_calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "首发 + 刷新重试 = 2 次真实调用;被拦的验证调用绝不能发出"
        );
        let snap = st.scheduler.status_snapshot();
        let a = snap.iter().find(|x| x.account_id == "a").unwrap();
        assert!(!a.disabled, "被 RPM 拦 ≠ 修复失败,账号不得被禁用");
        assert_eq!(a.reason, "", "不得按 TokenInvalid 上报(invalid_refresh_token)");
        assert_eq!(a.rpm_used, 2, "被拦的验证调用不得记账");
    }

    /// upstream_cut(静默掐流前兆)收尾纪律:只喂软冷却 + 请求日志 error_kind,
    /// 客户端 finale 原样送达,健康/禁用体系零变化(2026-07-25 事故 + 评审#2)。
    #[tokio::test]
    async fn upstream_cut_stream_feeds_soft_drain_only() {
        let chat_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let provider: Arc<dyn Provider> = Arc::new(HealGateMockProvider { chat_calls });
        let st = Arc::new(WorkerState {
            instance: 0,
            egress_desc: String::new(),
            group: String::new(),
            provider,
            settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
            scheduler: AccountScheduler::new(
                vec![Arc::new(acct(&[]))],
                &gw_core::config::SchedulerConfig::default(),
            ),
            refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
            usage_sink: None,
            pending_writes: PendingWrites::new(),
            store: None,
            quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
            quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
            quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
            sync_lock: tokio::sync::Mutex::new(()),
            group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
            _client: reqwest::Client::new(),
        });
        // 模拟 Kiro 掐流:payload 已出,provider 报 UpstreamCut 后仍合成完整 finale。
        let cut_stream = || {
            chat_stream(vec![
                Ok(StreamItem::Sse(SseEvent::new(
                    "message_start",
                    serde_json::json!({"message":{"id":"m","content":[]}}),
                ))),
                Ok(StreamItem::Sse(SseEvent::new(
                    "content_block_delta",
                    serde_json::json!({"index":0,"delta":{"type":"text_delta","text":"半截"}}),
                ))),
                Ok(StreamItem::UpstreamCut),
                Ok(StreamItem::Sse(SseEvent::new(
                    "message_delta",
                    serde_json::json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
                ))),
                Ok(StreamItem::Sse(SseEvent::new("message_stop", serde_json::json!({})))),
                Ok(StreamItem::Usage(ChatUsage {
                    input_tokens: 4,
                    output_tokens: 2,
                    ..Default::default()
                })),
            ])
        };
        // 第一次掐流:未达阈值(默认 2),不进 draining;客户端仍收到完整 finale。
        let lease = st.scheduler.acquire(Some("s")).await.unwrap();
        let resp = stream_response(
            st.clone(),
            lease,
            cut_stream(),
            req_model_m(),
            String::new(),
            Arc::new(acct(&[])),
            std::time::Instant::now(),
            Wire::Anthropic,
        );
        assert_eq!(resp.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        let body = String::from_utf8_lossy(&bytes).to_string();
        assert!(body.contains("message_stop"), "合成 finale 应照常转发客户端");
        assert!(!body.contains("upstream_cut"), "UpstreamCut 绝不泄漏给客户端");
        assert!(!st.scheduler.is_draining("a"), "单次 cut 未达阈值,不进 draining");
        let snap = st.scheduler.status_snapshot();
        let a = snap.iter().find(|x| x.account_id == "a").unwrap();
        assert!(!a.disabled, "掐流绝不进禁用体系");
        // 第二次掐流:窗口内达阈值 → draining;但单号池 fail-open 仍能拿到 lease。
        let lease = st.scheduler.acquire(Some("s")).await.unwrap();
        let resp = stream_response(
            st.clone(),
            lease,
            cut_stream(),
            req_model_m(),
            String::new(),
            Arc::new(acct(&[])),
            std::time::Instant::now(),
            Wire::Anthropic,
        );
        let _ = axum::body::to_bytes(resp.into_body(), 64 * 1024).await.unwrap();
        assert!(st.scheduler.is_draining("a"), "窗口内两次 cut 应进软冷却");
        let snap = st.scheduler.status_snapshot();
        let a = snap.iter().find(|x| x.account_id == "a").unwrap();
        assert!(!a.disabled, "draining 不等于禁用,健康面板不受影响");
        let lease = st.scheduler.acquire(None).await.unwrap();
        assert_eq!(lease.account_id(), "a", "normal 全空时 fail-open:draining 号仍服务");
    }

    // ───────────────────────── OpenAI 线缆(cursor 专属)─────────────────────────

    #[test]
    fn openai_线缆只挂给_cursor_家族() {
        assert!(mount_openai_wire("cursor"));
        // kiro / ccmax 明确不做:它们的主链路全程 Anthropic、零转换,那是刻意保住的资产。
        for f in ["kiro", "claude-dario", "claude-subprocess", "", "Cursor"] {
            assert!(!mount_openai_wire(f), "{f} 不该挂 OpenAI 入口");
        }
    }

    /// 只为 `/v1/models` 形状测试存在的 provider:家族名可控 + 目录里真有一项。
    struct CatalogMockProvider(&'static str);

    #[async_trait::async_trait]
    impl Provider for CatalogMockProvider {
        fn family(&self) -> &'static str {
            self.0
        }
        fn account_schema(&self) -> &'static [gw_core::account::FieldSpec] {
            &[]
        }
        async fn list_models(&self) -> Result<Vec<gw_core::model::ModelInfo>, UpstreamError> {
            Ok(vec![gw_core::model::ModelInfo {
                display_name: Some("Grok".into()),
                ..gw_core::model::ModelInfo::new("grok-4.5")
            }])
        }
        async fn chat(
            &self,
            _req: ChatRequest,
            _ctx: &CallCtx,
        ) -> Result<gw_core::provider::ChatStream, UpstreamError> {
            Err(UpstreamError::new(UpstreamErrorKind::Other, "未用到"))
        }
        async fn refresh_auth(&self, a: &Account) -> Result<Account, UpstreamError> {
            Ok(a.clone())
        }
    }

    fn catalog_state(family: &'static str) -> Arc<WorkerState> {
        let provider: Arc<dyn Provider> = Arc::new(CatalogMockProvider(family));
        Arc::new(WorkerState {
            instance: 0,
            egress_desc: String::new(),
            group: String::new(),
            provider,
            settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
            scheduler: AccountScheduler::new(
                vec![Arc::new(acct(&[]))],
                &gw_core::config::SchedulerConfig::default(),
            ),
            refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
            usage_sink: None,
            pending_writes: PendingWrites::new(),
            store: None,
            quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
            quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
            quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
            sync_lock: tokio::sync::Mutex::new(()),
            group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
            _client: reqwest::Client::new(),
        })
    }

    /// `/v1/models` 是**共享**端点:kiro / dario 也走它。OpenAI 字段只能加在开了
    /// OpenAI 入口的 worker 上,否则就是为迁就一种协议去改另一种协议的共享响应
    /// (按 schema 严校验的客户端、对响应体做快照的代理都会看到不兼容变化)。
    #[tokio::test]
    async fn models_端点的_openai_字段只出现在_cursor_worker() {
        let kiro = body_json(models(State(catalog_state("kiro"))).await).await;
        assert!(kiro.get("object").is_none(), "非 cursor 家族不该出现 object:list");
        let item = &kiro["data"][0];
        assert_eq!(item["type"], "model", "Anthropic 字段必须还在");
        assert_eq!(item["id"], "grok-4.5");
        assert!(item.get("created_at").is_some());
        for k in ["object", "created", "owned_by"] {
            assert!(item.get(k).is_none(), "非 cursor 家族不该出现 {k}");
        }

        let cursor = body_json(models(State(catalog_state("cursor"))).await).await;
        assert_eq!(cursor["object"], "list", "NewAPI 的获取模型列表认这个键");
        let item = &cursor["data"][0];
        assert_eq!(item["object"], "model");
        assert_eq!(item["owned_by"], "cursor");
        assert!(item["created"].is_i64());
        // 两套字段并存:Anthropic 侧一个都不能少。
        assert_eq!(item["type"], "model");
        assert_eq!(item["display_name"], "Grok");
    }

    /// 一段典型的 Anthropic 回复流:文本 + 用量 + 正常收尾。
    fn text_reply_stream() -> gw_core::provider::ChatStream {
        chat_stream(vec![
            Ok(StreamItem::Sse(SseEvent::new(
                "message_start",
                serde_json::json!({"message":{"model":"grok-4.5","usage":{"input_tokens":7}}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new(
                "content_block_start",
                serde_json::json!({"index":0,"content_block":{"type":"text","text":""}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new(
                "content_block_delta",
                serde_json::json!({"index":0,"delta":{"type":"text_delta","text":"hi"}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new(
                "content_block_stop",
                serde_json::json!({"index":0}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new(
                "message_delta",
                serde_json::json!({"delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":2}}),
            ))),
            Ok(StreamItem::Sse(SseEvent::new("message_stop", serde_json::json!({})))),
        ])
    }

    fn wire_state() -> Arc<WorkerState> {
        let provider: Arc<dyn Provider> = Arc::new(HealGateMockProvider {
            chat_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        Arc::new(WorkerState {
            instance: 0,
            egress_desc: String::new(),
            group: String::new(),
            provider,
            settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
            scheduler: AccountScheduler::new(
                vec![Arc::new(acct(&[]))],
                &gw_core::config::SchedulerConfig::default(),
            ),
            refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
            usage_sink: None,
            pending_writes: PendingWrites::new(),
            store: None,
            quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
            quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
            quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
            sync_lock: tokio::sync::Mutex::new(()),
            group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
            _client: reqwest::Client::new(),
        })
    }

    async fn sse_body(resp: axum::response::Response) -> String {
        let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    #[tokio::test]
    async fn chat_线缆的流式响应以_done_收尾_用量帧在它之前() {
        let st = wire_state();
        let lease = st.scheduler.acquire(Some("s")).await.unwrap();
        let resp = stream_response(
            st.clone(),
            lease,
            text_reply_stream(),
            req_model_m(),
            String::new(),
            Arc::new(acct(&[])),
            std::time::Instant::now(),
            Wire::OpenAiChat { include_usage: true },
        );
        let body = sse_body(resp).await;
        // ChatCompletions 不写 event 行 —— 写了会让只认 data: 的解析器丢帧。
        assert!(!body.contains("event: "), "chat 线缆不该有 event 行:\n{body}");
        assert!(body.contains("chat.completion.chunk"));
        assert!(body.contains("\"content\":\"hi\""));
        let done = body.rfind("data: [DONE]").expect("必须以 [DONE] 收尾");
        let usage = body.rfind("\"usage\"").expect("include_usage=true 必须发用量帧");
        // 顺序反了 NewAPI 读到终止哨兵就收工,这次请求在它那边记 0 用量。
        assert!(usage < done, "用量帧必须在 [DONE] 之前:\n{body}");
        assert!(body.contains("\"prompt_tokens\":7"));
        assert!(body.contains("\"completion_tokens\":2"));
        // Anthropic 的事件名一个都不该漏出去。
        assert!(!body.contains("message_stop"));
        assert!(!body.contains("content_block_delta"));
    }

    #[tokio::test]
    async fn responses_线缆的流式响应以_response_completed_收尾() {
        let st = wire_state();
        let lease = st.scheduler.acquire(Some("s")).await.unwrap();
        let resp = stream_response(
            st.clone(),
            lease,
            text_reply_stream(),
            req_model_m(),
            String::new(),
            Arc::new(acct(&[])),
            std::time::Instant::now(),
            Wire::OpenAiResponses,
        );
        let body = sse_body(resp).await;
        // Responses 每帧都必须带 event 名,客户端按它分发。
        assert!(body.contains("event: response.created"));
        assert!(body.contains("event: response.output_text.delta"));
        assert!(body.contains("event: response.completed"));
        assert!(!body.contains("[DONE]"), "Responses 没有 [DONE] 这个概念");
        assert!(!body.contains("message_stop"));
    }

    #[tokio::test]
    async fn 两种_openai_线缆的非流式响应各是各的形状() {
        let st = wire_state();

        let lease = st.scheduler.acquire(Some("s")).await.unwrap();
        let chat = collect_response(
            &st.scheduler,
            None,
            None,
            None,
            lease,
            text_reply_stream(),
            req_model_m(),
            String::new(),
            Arc::new(acct(&[])),
            std::time::Instant::now(),
            "cursor",
            Wire::OpenAiChat { include_usage: false },
        )
        .await;
        assert_eq!(chat.status(), StatusCode::OK);
        let v = body_json(chat).await;
        assert_eq!(v["object"], "chat.completion");
        assert_eq!(v["choices"][0]["message"]["content"], "hi");
        assert_eq!(v["choices"][0]["finish_reason"], "stop");
        assert_eq!(v["usage"]["prompt_tokens"], serde_json::json!(7));

        let lease = st.scheduler.acquire(Some("s")).await.unwrap();
        let responses = collect_response(
            &st.scheduler,
            None,
            None,
            None,
            lease,
            text_reply_stream(),
            req_model_m(),
            String::new(),
            Arc::new(acct(&[])),
            std::time::Instant::now(),
            "cursor",
            Wire::OpenAiResponses,
        )
        .await;
        let v = body_json(responses).await;
        assert_eq!(v["object"], "response");
        assert_eq!(v["status"], "completed");
        assert_eq!(v["output"][0]["content"][0]["text"], "hi");
        assert_eq!(v["usage"]["input_tokens"], serde_json::json!(7));
    }

    #[tokio::test]
    async fn anthropic_线缆的非流式响应一个字节都没变() {
        let st = wire_state();
        let lease = st.scheduler.acquire(Some("s")).await.unwrap();
        let resp = collect_response(
            &st.scheduler,
            None,
            None,
            None,
            lease,
            text_reply_stream(),
            req_model_m(),
            String::new(),
            Arc::new(acct(&[])),
            std::time::Instant::now(),
            "kiro",
            Wire::Anthropic,
        )
        .await;
        let v = body_json(resp).await;
        // 还是 Anthropic Messages:没有 object/choices,有 content 数组与 stop_reason。
        assert!(v.get("object").is_none());
        assert!(v.get("choices").is_none());
        assert_eq!(v["content"][0]["text"], "hi");
        assert_eq!(v["stop_reason"], "end_turn");
    }

    #[tokio::test]
    async fn 入站转换失败回_openai_形状的_400_并点名字段() {
        let st = wire_state();
        // 缺 messages:不占账号、不打上游,直接在门口挡掉。
        let resp = chat_completions(
            State(st.clone()),
            HeaderMap::new(),
            axum::body::Bytes::from(serde_json::json!({"model": "grok-4.5"}).to_string()),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["error"]["type"], "invalid_request_error");
        assert_eq!(v["error"]["param"], "messages");
        // Anthropic 的错误外壳不该出现。
        assert!(v.get("type").is_none());

        let resp = responses(
            State(st),
            HeaderMap::new(),
            axum::body::Bytes::from(
                serde_json::json!({"model":"m","input":"hi","previous_response_id":"resp_x"})
                    .to_string(),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let v = body_json(resp).await;
        assert_eq!(v["error"]["param"], "previous_response_id");
    }

    #[tokio::test]
    async fn 选号失败在_openai_线缆上也换成_openai_错误体() {
        // 空池 → AcquireError,走的是 `error_response` 那条自算状态码的路。
        let provider: Arc<dyn Provider> = Arc::new(HealGateMockProvider {
            chat_calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        });
        let st = Arc::new(WorkerState {
            instance: 0,
            egress_desc: String::new(),
            group: String::new(),
            provider,
            settings_sync: parking_lot::RwLock::new(SettingsSync::default()),
            scheduler: AccountScheduler::new(
                Vec::new(),
                &gw_core::config::SchedulerConfig::default(),
            ),
            refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
            usage_sink: None,
            pending_writes: PendingWrites::new(),
            store: None,
            quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
            quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
            quota_sem: Arc::new(tokio::sync::Semaphore::new(1)),
            sync_lock: tokio::sync::Mutex::new(()),
            group_views: parking_lot::RwLock::new(std::collections::HashMap::new()),
            _client: reqwest::Client::new(),
        });
        let resp = chat_completions(
            State(st.clone()),
            HeaderMap::new(),
            axum::body::Bytes::from(
                serde_json::json!({
                    "model": "grok-4.5",
                    "messages": [{"role": "user", "content": "hi"}]
                })
                .to_string(),
            ),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v = body_json(resp).await;
        assert_eq!(v["error"]["type"], "server_error");
        assert!(v.get("type").is_none(), "不能漏出 Anthropic 的错误外壳");

        // 同一条路径在 Anthropic 线缆上必须保持旧形状。
        let resp = messages(
            State(st),
            HeaderMap::new(),
            Json(serde_json::json!({
                "model": "grok-4.5",
                "messages": [{"role": "user", "content": "hi"}]
            })),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
        let v = body_json(resp).await;
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "overloaded_error");
    }
}
