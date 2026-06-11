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
use gw_core::provider::{
    AccountQuota, CallCtx, ChatRequest, ChatUsage, Provider, SseEvent, StreamItem,
};
use gw_core::store::{UsageRecord, UsageSink};
use gw_store::SqliteStore;

use crate::egress;
use crate::registry::Registry;
use scheduler::AccountScheduler;

struct WorkerState {
    instance: u32,
    egress_desc: String,
    group: String,
    provider: Arc<dyn Provider>,
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
    /// worker 的 egress client(provider 已持有同一个;此处保留供诊断)。
    _client: reqwest::Client,
}

/// 配额缓存 TTL:被查看时每账号 ≤1 次/分钟打上游(只读 getUsageLimits,安全)。
const QUOTA_TTL: std::time::Duration = std::time::Duration::from_secs(60);
/// 配额刷新最大并发(stampede 护栏):上百账号同时被查看也只 N 个在打上游。
const QUOTA_MAX_CONCURRENCY: usize = 3;

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

    /// 强制刷新该账号一次(用于 chat 返回 TokenInvalid 时的同号 refresh-then-retry):
    /// 即便当前 token 看似"新",也走单飞锁刷新(上游已判定其失效)。
    async fn force_refresh(
        &self,
        account: Arc<Account>,
    ) -> Result<Arc<Account>, gw_core::error::UpstreamError> {
        self.refresh_locked(account).await
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
        let fresh = cached
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

    /// 拉一次配额(可能触发 token 刷新)。Ok(None) = 账号已不在/上游无配额。
    async fn try_fetch_quota(
        self: &Arc<Self>,
        account_id: &str,
    ) -> anyhow::Result<Option<AccountQuota>> {
        let Some(account) = self.scheduler.account(account_id) else {
            return Ok(None);
        };
        let account = self
            .ensure_credentialed(account)
            .await
            .map_err(|e| anyhow::anyhow!("ensure token: {e}"))?;
        // 企业/IdC 号 getUsageLimits 同样要求 profileArn:缺则先发现+持久化。
        let account = self.ensure_profile_arn(account).await;
        self.provider
            .account_quota(&account)
            .await
            .map_err(|e| anyhow::anyhow!("getUsageLimits: {e}"))
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
        }),
        None => serde_json::Value::Null,
    }
}

/// 账号是否持有未过期(且非临近过期)的 access_token。
///
/// 无 access_token → false(需刷新)。有 token 但无 expires_at → 视为有效(无从判断,
/// 沿用旧行为;真过期会被上游 403 触发 force_refresh 兜底)。有 expires_at → 距现在
/// < 60s 视为临近过期需提前刷新(对齐 kiro.rs cred_expiring_soon)。
fn has_fresh_token(account: &Account) -> bool {
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
    let system: SystemConfig = load_yaml(system_path).unwrap_or_default();

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
            .load_group_accounts(&wcfg.account_group)?
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
    let client = egress::build_client(&wcfg.egress)?;
    let egress_desc = egress::describe(&wcfg.egress);
    // cache_sim 全局 store 的 TTL/容量从 system.cache 同步(否则恒用编译期默认 300s/4096)。
    gw_kiro::cache_sim::global().set_ttl_secs(system.cache.sim_ttl_secs);
    gw_kiro::cache_sim::global().set_max_sessions(system.cache.max_sessions);

    // provider 工厂 cfg:注入 system.cache(缓存计费 multiplier/cap/floor)+ system.image
    // (图像压缩,嵌为 "image" 子对象)。序列化失败退回 Null/缺省(provider 各自回退,不致命)。
    let mut provider_cfg = serde_json::to_value(&system.cache).unwrap_or(serde_json::Value::Null);
    if let serde_json::Value::Object(map) = &mut provider_cfg {
        if let Ok(img) = serde_json::to_value(&system.image) {
            map.insert("image".into(), img);
        }
    }
    let provider = registry.build(&provider_family, &provider_cfg, client.clone())?;

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

    let state = Arc::new(WorkerState {
        instance,
        egress_desc,
        group: wcfg.account_group.clone(),
        provider,
        scheduler: AccountScheduler::new(accounts, &system.scheduler),
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        usage_sink,
        pending_writes: PendingWrites::new(),
        store: store.clone(),
        quota_cache: parking_lot::Mutex::new(std::collections::HashMap::new()),
        quota_inflight: parking_lot::Mutex::new(std::collections::HashSet::new()),
        quota_sem: Arc::new(tokio::sync::Semaphore::new(QUOTA_MAX_CONCURRENCY)),
        _client: client,
    });

    // 账号配置 sync:30s 从 DB 重读组内账号集,admin 增删改无需重启 worker 即生效。
    // (翻转语义见 scheduler::sync_accounts;读库失败跳过本轮,不影响服务。)
    if let Some(store) = store {
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // 首跳立即触发,跳过(启动时刚加载过)。
            loop {
                tick.tick().await;
                // 先重试上轮回写失败的 extra(脏账号),失败下轮再试。
                flush_dirty_extras(&st.scheduler, &store, &st.refresh_locks, "sync 重试").await;
                match store.load_group_accounts(&st.group) {
                    Ok(accs) => {
                        let accs = filter_by_provider(
                            accs.into_iter().map(Arc::new).collect(),
                            st.provider.family(),
                        );
                        let out = st.scheduler.sync_accounts(accs);
                        if out.added + out.removed > 0 {
                            tracing::info!(added = out.added, removed = out.removed,
                                "账号集已按 DB 同步");
                            // 新进账号若缺订阅档位,预热配额查询补齐(模型过滤数据源)。
                            st.warm_subscription_titles();
                        }
                    }
                    Err(e) => tracing::warn!("账号 sync 读库失败,跳过本轮: {e}"),
                }
            }
        });
    }

    // 启动预热:缺订阅档位的账号后台拉一次配额(getUsageLimits 只读,安全),
    // 让模型能力过滤(FREE 不给 opus)冷启动即有数据源,而非等 /health 被动触发。
    state.warm_subscription_titles();

    let mut app = Router::new()
        .route("/v1/messages", post(messages))
        .route("/v1/models", get(models))
        .route("/health", get(health));

    // worker 不做对外鉴权、且信任 router 注入的 X-Gw-Client-Key;必须只绑 loopback,
    // 否则客户端可直连 worker 绕过 router 鉴权并伪造用量归属(审查 #2)。
    let loopback = is_loopback_listen(&wcfg.listen);
    if loopback {
        // 内网管理端点(无鉴权,信任同机 router):人工救号。**仅 loopback 才挂载**——
        // 非 loopback 误配下暴露它等于把"清禁用保护"开给整个网络(审查②R 共识 high)。
        app = app.route("/accounts/{id}/reset", post(reset_account));
    } else {
        tracing::warn!(
            listen = %wcfg.listen,
            "⚠️ worker 绑定到非 loopback 地址:这会让客户端绕过 router 鉴权并伪造 client_key 归属;\
             reset 管理端点已禁用。请改绑 127.0.0.1,或为 router→worker 内网跳加共享密钥。"
        );
    }
    let app = app.with_state(state.clone());

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
        // usage 是否在落库:库打开失败时为 false(降级,usage 不入库),便于运维发现。
        "usage_persist": st.usage_sink.is_some(),
        "status": "ok"
    }))
}

/// `POST /accounts/{id}/reset` —— 内网管理:人工救号。清运行时禁用/冷却/失败计数
/// (配置层 disabled 不动,那走 PATCH)。由 admin(router 进程)扇出调用;
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

/// `GET /v1/models` —— 暴露 provider 的模型目录(Anthropic 线缆格式)。
/// provider 一处实现 `list_models`,框架在此映射成对外响应(写一次,各 provider 共享)。
async fn models(State(st): State<Arc<WorkerState>>) -> axum::response::Response {
    // created_at 占位:Kiro 不提供每模型创建时间(见 kiro-model-versioning 记忆),
    // 但 Anthropic /v1/models 条目带 created_at,严格类型客户端会校验——给个固定占位值
    // 以保证兼容(非真实日期,仅占位)。
    const MODEL_CREATED_AT: &str = "2025-01-01T00:00:00Z";
    match st.provider.list_models().await {
        Ok(list) => {
            let data: Vec<serde_json::Value> = list
                .iter()
                .map(|m| {
                    serde_json::json!({
                        "type": "model",
                        "id": m.id,
                        "display_name": m.display_name.clone().unwrap_or_else(|| m.id.clone()),
                        "created_at": MODEL_CREATED_AT,
                    })
                })
                .collect();
            let first = list.first().map(|m| m.id.clone());
            let last = list.last().map(|m| m.id.clone());
            Json(serde_json::json!({
                "data": data,
                "has_more": false,
                "first_id": first,
                "last_id": last,
            }))
            .into_response()
        }
        Err(e) => upstream_error_response(&e),
    }
}

async fn messages(
    State(st): State<Arc<WorkerState>>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let req = ChatRequest::from_anthropic_body(body);
    // 客户 key 归属:router 鉴权后经内网头透传(对外 Authorization 不到 worker)。
    let client_key = headers
        .get(crate::CLIENT_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    // 会话亲和键 = provider 派生的 conversationId(Kiro)。None → 无亲和按负载选号。
    let affinity_key = st.provider.affinity_key(&req);

    // 选号 + 发起 chat 的重试循环:token 失效(403/401)时刷新该号并对账号生命周期上报,
    // 换号重试;首包前的可重试错误最多走 total 个账号。committed(首包已出)后不重试。
    let total = st.scheduler.total().max(1);
    let mut attempts = 0;

    loop {
        attempts += 1;
        // 1. 按会话亲和取并发租约(持有到流结束)。合格账号须支持本次模型
        //    (FREE 订阅不支持 opus,过滤掉避免 403 误杀,对齐 kiro.rs supports_opus)。
        let lease = match st
            .scheduler
            .acquire_where(affinity_key.as_deref(), |a| {
                st.provider.account_supports_model(a, &req.model)
            })
            .await
        {
            Ok(l) => l,
            Err(e) => {
                // NoModelSupport 是客户侧可解(换模型/升级订阅),给 400;其余是池子状态,503。
                let code = if e == scheduler::AcquireError::NoModelSupport {
                    StatusCode::BAD_REQUEST
                } else {
                    StatusCode::SERVICE_UNAVAILABLE
                };
                return (
                    code,
                    Json(serde_json::json!({"type":"error","error":{"message": e.to_string()}})),
                )
                    .into_response();
            }
        };
        let account_id = lease.account_id().to_string();

        // 2. 确保该号有未过期 access_token(按需刷新,带 expires_at 检查 + 单飞)。
        //    刷新失败按 kind 处理:invalid_grant(TokenInvalid)永久禁用;transient
        //    (网络/5xx/429)只记 transient 失败、换号重试,不永久打死健康号。
        let account = match st.ensure_credentialed(lease.account.clone()).await {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!(account = %account_id, kind = ?e.kind, "凭证刷新失败: {e}");
                st.scheduler.report_failure(&account_id, e.kind);
                drop(lease);
                if attempts >= total {
                    return upstream_error_response(&e);
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
        //    - 其他可重试错误:上报失败、换号重试。
        match st.provider.chat(req.clone(), &ctx).await {
            Ok(stream) => {
                return finish_response(st.clone(), lease, stream, &req, &client_key).await
            }
            Err(e) if e.kind == UpstreamErrorKind::TokenInvalid => {
                tracing::info!(account = %account_id, "chat 403 token 失效,尝试同号刷新后重试");
                match st.force_refresh(ctx.account.clone()).await {
                    Ok(refreshed) => {
                        let retry_ctx = CallCtx {
                            account: refreshed,
                            session_id: affinity_key.clone().unwrap_or_default(),
                            cache_key: affinity_key.clone().unwrap_or_default(),
                        };
                        match st.provider.chat(req.clone(), &retry_ctx).await {
                            Ok(stream) => {
                                return finish_response(st.clone(), lease, stream, &req, &client_key)
                                    .await
                            }
                            Err(e2) => {
                                // 刷新后仍失败:这次才上报失败 + 换号。
                                tracing::warn!(account = %account_id, kind = ?e2.kind, "刷新后重试仍失败: {e2}");
                                st.scheduler.report_failure(&account_id, e2.kind);
                                drop(lease);
                                if e2.kind == UpstreamErrorKind::BadRequest || attempts >= total {
                                    return upstream_error_response(&e2);
                                }
                                continue;
                            }
                        }
                    }
                    Err(re) => {
                        // 刷新失败:invalid_grant→永久禁用;transient→换号重试。
                        tracing::warn!(account = %account_id, kind = ?re.kind, "同号刷新失败: {re}");
                        st.scheduler.report_failure(&account_id, re.kind);
                        drop(lease);
                        if attempts >= total {
                            return upstream_error_response(&re);
                        }
                        continue;
                    }
                }
            }
            Err(e) => {
                let kind = e.kind;
                tracing::warn!(account = %account_id, kind = ?kind, "chat 失败: {e}");
                st.scheduler.report_failure(&account_id, kind);
                drop(lease);
                if kind == UpstreamErrorKind::BadRequest || attempts >= total {
                    return upstream_error_response(&e);
                }
                continue;
            }
        }
    }
}

/// 把 [`UpstreamError`] 映射为对外 HTTP 响应(BadRequest→400,其余→502)。
fn upstream_error_response(e: &gw_core::error::UpstreamError) -> axum::response::Response {
    let code = if e.kind == UpstreamErrorKind::BadRequest {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::BAD_GATEWAY
    };
    (
        code,
        Json(serde_json::json!({"type":"error","error":{"message": e.to_string()}})),
    )
        .into_response()
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
        success,
    };
    if let Err(e) = sink.record(rec).await {
        tracing::warn!(account = %account_id, "usage 落库失败(不影响响应): {e}");
    }
}

/// 按客户端 `stream` 标志分发:provider 一律产流,这里决定回 SSE 还是折叠成单个
/// 非流式 Messages 响应(折叠逻辑写一次,见 [`gw_core::fold`])。两条路径都做同一套
/// 收尾(账号生命周期上报 + usage 落库)。
async fn finish_response(
    st: Arc<WorkerState>,
    lease: scheduler::AccountLease,
    stream: gw_core::provider::ChatStream,
    req: &ChatRequest,
    client_key: &str,
) -> axum::response::Response {
    if req.stream {
        // 流式:返回惰性 SSE 响应,收尾走 StreamCtx::Drop(同步上报 + detach 落库)。
        stream_response(st, lease, stream, req.model.clone(), client_key.to_string())
    } else {
        // 非流式:此处即时抽干流、折叠成单个 Messages JSON。
        collect_response(
            &st.scheduler,
            st.usage_sink.as_ref(),
            lease,
            stream,
            req.model.clone(),
            client_key.to_string(),
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
async fn collect_response(
    scheduler: &AccountScheduler,
    usage_sink: Option<&Arc<dyn UsageSink>>,
    lease: scheduler::AccountLease,
    mut stream: gw_core::provider::ChatStream,
    model: String,
    client_key: String,
) -> axum::response::Response {
    /// 非流式抽干的事件数上限(OOM 粗护栏:正常响应 < 数万事件,远低于此;
    /// 超出视为异常上游,回受控错误而非无界吃内存。审查 #3)。
    const MAX_NONSTREAM_EVENTS: usize = 500_000;

    let account_id = lease.account_id().to_string();
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
            Ok(StreamItem::Usage(u)) => last_usage = Some(u),
            Err(e) => {
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
    if success {
        scheduler.report_success(&account_id);
    } else {
        let kind = match &outcome {
            Outcome::Upstream(e) => e.kind,
            _ => UpstreamErrorKind::ServerError,
        };
        scheduler.report_failure(&account_id, kind);
    }
    finalize_usage(
        usage_sink,
        &account_id,
        &model,
        &client_key,
        last_usage.as_ref(),
        success,
    )
    .await;
    drop(lease); // 释放并发槽(响应已抽干)。

    match outcome {
        Outcome::Ok(msg) => (StatusCode::OK, Json(msg)).into_response(),
        Outcome::Upstream(e) => upstream_error_response(&e),
        Outcome::Bad(data) => (StatusCode::BAD_GATEWAY, Json(data)).into_response(),
    }
}

/// 把 provider 的 StreamItem 流转成 axum SSE 响应,并在流结束时按结果上报账号生命周期
/// + 把终结 usage 落库(#130)。
///
/// 关键:`lease`(并发许可)被 move 进流的状态,持有到流耗尽才 Drop → 整个响应期间
/// 占用该账号一个并发槽,符合 v52 并发语义。流内出现 error 事件 / Err → 上报失败;
/// 干净结束 → 上报成功。usage 事件不转发客户端(缓存到 `last_usage`,流终态统一落库)。
fn stream_response(
    st: Arc<WorkerState>,
    lease: scheduler::AccountLease,
    stream: gw_core::provider::ChatStream,
    model: String,
    client_key: String,
) -> axum::response::Response {
    /// unfold 累积态:lease 持有到流结束;reported 防重复上报;last_usage 缓存终结用量。
    struct StreamCtx {
        st: Arc<WorkerState>,
        account_id: String,
        model: String,
        client_key: String,
        _lease: scheduler::AccountLease,
        inner: gw_core::provider::ChatStream,
        saw_error: bool,
        reported: bool,
        last_usage: Option<ChatUsage>,
    }

    // 收尾(账号生命周期上报 + usage 落库)统一放 Drop:无论流跑到 None 正常结束,
    // **还是客户端中途断开导致 axum 直接 drop 响应体**(此时 unfold 不再被 poll、永远到不了
    // None 分支),Drop 都会触发,确保 usage 不漏记、账号信号不丢(审查 Skeptic#1/Architect#1)。
    // 生命周期上报是同步的(parking_lot,Drop 内直接做);usage 落库是 async,detach 到运行时
    // 异步执行——既不阻塞 SSE 收尾 poll(审查 Skeptic#2),又不依赖流被读到 EOF。
    impl Drop for StreamCtx {
        fn drop(&mut self) {
            // 账号生命周期上报(一次)。Err 分支可能已按具体 kind 上报过(reported=true)。
            if !self.reported {
                self.reported = true;
                if self.saw_error {
                    self.st
                        .scheduler
                        .report_failure(&self.account_id, UpstreamErrorKind::ServerError);
                } else {
                    self.st.scheduler.report_success(&self.account_id);
                }
            }
            // usage 落库(Drop 仅执行一次,无需额外去重标记)。
            let (Some(sink), Some(usage)) = (self.st.usage_sink.clone(), self.last_usage.take())
            else {
                return;
            };
            let account_id = self.account_id.clone();
            let model = self.model.clone();
            let client_key = self.client_key.clone();
            let success = !self.saw_error;
            // detach 到当前运行时;无运行时上下文(理论上不会:SSE body 总在 tokio 内 drop)则跳过。
            // guard 跟随任务存活:停机排空经 pending_writes.wait_idle 等这批落库收尾。
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let guard = self.st.pending_writes.enter();
                handle.spawn(async move {
                    let _guard = guard;
                    finalize_usage(
                        Some(&sink),
                        &account_id,
                        &model,
                        &client_key,
                        Some(&usage),
                        success,
                    )
                    .await;
                });
            }
        }
    }

    let account_id = lease.account_id().to_string();
    let init = StreamCtx {
        st,
        account_id,
        model,
        client_key,
        _lease: lease,
        inner: stream,
        saw_error: false,
        reported: false,
        last_usage: None,
    };

    let sse = futures::stream::unfold(init, |mut ctx| async move {
        // 单步内循环跳过 usage 事件,直到拿到一个可转发事件或流结束(避免递归类型膨胀)。
        loop {
            match ctx.inner.next().await {
                Some(Ok(StreamItem::Sse(ev))) => {
                    if ev.event == "error" {
                        ctx.saw_error = true;
                    }
                    let out = match ev.to_wire() {
                        Ok(_) => Event::default().event(ev.event).data(ev.data.to_string()),
                        Err(e) => {
                            // 序列化失败也算本次响应损坏 → 收尾按失败上报(审查 Architect#9)。
                            ctx.saw_error = true;
                            Event::default().event("error").data(
                                serde_json::json!({"type":"error","error":{"message": format!("serialize sse: {e}")}})
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
                    if !ctx.reported {
                        ctx.reported = true;
                        ctx.st.scheduler.report_failure(&ctx.account_id, e.kind);
                    }
                    let out = Event::default().event("error").data(
                        serde_json::json!({"type":"error","error":{"message": e.to_string()}})
                            .to_string(),
                    );
                    return Some((Ok(out), ctx));
                }
                None => {
                    // 流正常结束。收尾(生命周期上报 + usage 落库)由 StreamCtx::drop 统一处理,
                    // 与"客户端中断致响应体被 drop"走同一条路径,避免两处逻辑分叉。
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
            })),
        ];
        let resp =
            collect_response(&sched, Some(&dyn_sink), lease, chat_stream(items), "m".into(), String::new()).await;
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
        let resp = collect_response(&sched, None, lease, chat_stream(items), "m".into(), String::new()).await;
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
            })),
        ];
        let resp =
            collect_response(&sched, Some(&dyn_sink), lease, chat_stream(items), "m".into(), String::new()).await;
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
}
