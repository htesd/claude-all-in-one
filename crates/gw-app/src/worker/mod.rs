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
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use gw_core::account::Account;
use gw_core::config::{AccountsConfig, InstancesConfig, SystemConfig};
use gw_core::error::UpstreamErrorKind;
use gw_core::provider::{CallCtx, ChatRequest, Provider, StreamItem};

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
    /// worker 的 egress client(provider 已持有同一个;此处保留供诊断)。
    _client: reqwest::Client,
}

impl WorkerState {
    /// 取该账号的 per-account 刷新锁(单飞:同账号同时只一个刷新)。
    fn refresh_lock(&self, account_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut map = self.refresh_locks.lock();
        map.entry(account_id.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
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
        // 二次检查:拿到锁后,scheduler 里可能已是别的请求刷新好的新账号。
        if let Some(fresh) = self.scheduler.account(&account.account_id) {
            if has_fresh_token(&fresh) {
                return Ok(fresh);
            }
        }
        let refreshed = Arc::new(self.provider.refresh_auth(&account).await?);
        // 回写 scheduler:带新 access_token / rolling refresh_token 的副本进入选号池
        // (单一事实来源;无独立 creds 缓存,避免两份凭证发散)。
        self.scheduler.update_account(refreshed.clone());
        Ok(refreshed)
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
) -> anyhow::Result<()> {
    let instances: InstancesConfig = load_yaml(instances_path)?;
    let accounts_cfg: AccountsConfig = load_yaml(accounts_path)?;
    let system: SystemConfig = load_yaml(system_path).unwrap_or_default();

    let wcfg = instances
        .worker(instance)
        .ok_or_else(|| anyhow::anyhow!("instances.yaml 中无 instance={instance}"))?
        .clone();

    let group = accounts_cfg
        .group(&wcfg.account_group)
        .ok_or_else(|| anyhow::anyhow!("accounts.yaml 中无组 '{}'", wcfg.account_group))?;

    let registry = Registry::with_builtins();
    tracing::debug!(providers = ?registry.families(), "已注册 provider");
    // 先按本 worker 的固定出口构造 egress client,注入 provider——
    // 保证该 provider 所有上游请求走同一出口 IP(防关联封号)。
    let client = egress::build_client(&wcfg.egress)?;
    let egress_desc = egress::describe(&wcfg.egress);
    // provider 工厂 cfg:注入 system.cache(缓存计费 multiplier/cap/floor)。序列化失败
    // 退回 Null(provider 各自回退默认参数,不致命)。
    let provider_cfg = serde_json::to_value(&system.cache).unwrap_or(serde_json::Value::Null);
    let provider = registry.build(&group.provider, &provider_cfg, client.clone())?;
    let accounts: Vec<Arc<Account>> = accounts_cfg
        .group_accounts_with_provider(&wcfg.account_group)
        .unwrap_or_default()
        .into_iter()
        .map(Arc::new)
        .collect();

    tracing::info!(
        instance,
        listen = %wcfg.listen,
        egress = %egress_desc,
        group = %wcfg.account_group,
        accounts = accounts.len(),
        provider = provider.family(),
        "worker 就绪"
    );

    let state = Arc::new(WorkerState {
        instance,
        egress_desc,
        group: wcfg.account_group.clone(),
        provider,
        scheduler: AccountScheduler::new(accounts),
        refresh_locks: parking_lot::Mutex::new(std::collections::HashMap::new()),
        _client: client,
    });

    let app = Router::new()
        .route("/v1/messages", post(messages))
        .route("/health", get(health))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&wcfg.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health(State(st): State<Arc<WorkerState>>) -> impl IntoResponse {
    Json(serde_json::json!({
        "role": "worker",
        "instance": st.instance,
        "egress": st.egress_desc,
        "group": st.group,
        "provider": st.provider.family(),
        "accounts": st.scheduler.total(),
        "status": "ok"
    }))
}

async fn messages(
    State(st): State<Arc<WorkerState>>,
    Json(body): Json<serde_json::Value>,
) -> axum::response::Response {
    let req = ChatRequest::from_anthropic_body(body);
    // 会话亲和键 = provider 派生的 conversationId(Kiro)。None → 无亲和按负载选号。
    let affinity_key = st.provider.affinity_key(&req);

    // 选号 + 发起 chat 的重试循环:token 失效(403/401)时刷新该号并对账号生命周期上报,
    // 换号重试;首包前的可重试错误最多走 total 个账号。committed(首包已出)后不重试。
    let total = st.scheduler.total().max(1);
    let mut attempts = 0;

    loop {
        attempts += 1;
        // 1. 按会话亲和取并发租约(持有到流结束)。
        let lease = match st.scheduler.acquire(affinity_key.as_deref()).await {
            Ok(l) => l,
            Err(e) => {
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
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
            Ok(stream) => return stream_response(st.clone(), lease, stream),
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
                            Ok(stream) => return stream_response(st.clone(), lease, stream),
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

/// 把 provider 的 StreamItem 流转成 axum SSE 响应,并在流结束时按结果上报账号生命周期。
///
/// 关键:`lease`(并发许可)被 move 进流的状态,持有到流耗尽才 Drop → 整个响应期间
/// 占用该账号一个并发槽,符合 v52 并发语义。流内出现 error 事件 / Err → 上报失败;
/// 干净结束 → 上报成功。usage 事件不转发客户端(仅记日志,#130 接 UsageSink)。
fn stream_response(
    st: Arc<WorkerState>,
    lease: scheduler::AccountLease,
    stream: gw_core::provider::ChatStream,
) -> axum::response::Response {
    /// unfold 累积态:lease 持有到流结束;reported 防重复上报。
    struct StreamCtx {
        st: Arc<WorkerState>,
        account_id: String,
        _lease: scheduler::AccountLease,
        inner: gw_core::provider::ChatStream,
        saw_error: bool,
        reported: bool,
    }

    let account_id = lease.account_id().to_string();
    let init = StreamCtx {
        st,
        account_id,
        _lease: lease,
        inner: stream,
        saw_error: false,
        reported: false,
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
                        "chat usage (P1: 暂不入库,#130 接 UsageSink)"
                    );
                    continue; // 不转发客户端,取下一个。
                }
                Some(Err(e)) => {
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
                    // 流正常结束:无 error 事件 → 上报成功;有 error → 上报失败。
                    if !ctx.reported {
                        ctx.reported = true;
                        if ctx.saw_error {
                            ctx.st
                                .scheduler
                                .report_failure(&ctx.account_id, UpstreamErrorKind::ServerError);
                        } else {
                            ctx.st.scheduler.report_success(&ctx.account_id);
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
    use std::collections::BTreeMap;

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
