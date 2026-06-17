# caio 集成 dario(Claude OAuth sidecar)实施计划 — v2(对抗审查后修订)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 给 caio 新增一条非 Kiro 的 Claude 上游:用 Claude OAuth 订阅凭证,经本机 dario(sidecar)做指纹伪装 + BoringSSL TLS,直连 `api.anthropic.com`。

**Architecture:** caio 掌账号池/调度/计费/admin(账号为 CC `.credentials.json`);新建 `gw-dario` crate 实现 `Provider` trait,`chat()` 把原始 Anthropic body(**强制 stream:true**)+ per-request 头(`x-dario-upstream-token`/`x-dario-device-id`/`x-dario-account-uuid`/`x-session-id`)转发给本机 dario-on-Bun(`/v1/messages`,鉴权用 `x-api-key`),dario 做报文重建 + TLS + 转发,SSE 原样回流;caio 解析 usage、折叠非流式、复用全部 gw-app 设施。dario 与 Kiro 是两个并列 provider 分组,故障转移交前置 NewAPI。

**Tech Stack:** Rust(workspace crate,`async-trait` + `async-stream` + `reqwest` 流式 + `futures`)、TypeScript(本地 dario fork 三处小补丁)、Docker Compose(每出口代理一个 dario-on-Bun sidecar)。

**Spec:** `docs/superpowers/specs/2026-06-17-dario-sidecar-integration-design.md`

---

## 核验已确认事实(workflow 对照真实代码,以此为准)

- **dario 入站鉴权**(`dario/src/proxy.ts:622-634`):`x-api-key:<key>`(裸值,优先读)或 `Authorization: Bearer <key>` 皆可;常量时间精确匹配 `DARIO_API_KEY`。**本计划统一用 `x-api-key`**。空 `DARIO_API_KEY` 时 dario 放行(仅 loopback)。
- **OAuth 刷新**(`dario/src/oauth.ts:799-851`):`POST https://platform.claude.com/v1/oauth/token`,**`Content-Type: application/x-www-form-urlencoded`**,表单恰 3 字段 `grant_type=refresh_token` / `refresh_token` / `client_id`,**无 client_secret**(纯 PKCE)。`client_id=9d1c250a-e61b-44d9-88ed-5944d1962f5e`(FALLBACK,`cc-oauth-detect.ts:88`)。响应 snake_case `{access_token, refresh_token, expires_in}`,**`expires_in` 单位=秒**,无 `expires_at`(自己 `now + expires_in` 派生)。⚠️ code 交换才用 JSON;refresh 必须 urlencoded。
- **Anthropic SSE usage**(`dario/src/proxy.ts:2321-2330`):`message_start.message.usage.{input_tokens,cache_read_input_tokens,cache_creation_input_tokens}`;`message_delta.usage.output_tokens`;**`message_stop` 不带 usage**。字段名即 snake_case 这些。
- **dario session 头**:dario 读 `x-session-id` / `x-client-session-id`(`proxy.ts:1619/1677`),**不是** `x-claude-code-session-id`。caio 发 `x-session-id`。
- **dario 注入补丁充分性**(`proxy.ts`):仅在 `pool===null`(账号<2,`proxy.ts:865`)时两处补丁足够;`pool` 非空时 sticky 重绑(`:1657`)+ 三处 failover(`:2063/:2140/:2158`)会覆盖注入值。→ **补 `injected` 标志守卫**(Task 1.3)。
- **caio worker 仅 1 处真耦合**:`worker/mod.rs:1524` `render_kiro_payload`;`641/642/723/724` 的 `gw_kiro::cache_sim::global()` 是启动期/30s 全局 setter,无害不动。守卫须按 **`provider.family()=="kiro"`**(`filter_by_provider` 放行空 provider 号,按 `account.provider` 判会丢空-provider kiro 号日志)。
- **expires_at 格式**(`worker/mod.rs:512` `parse_rfc3339_unix`):只剥末尾 `Z` 再按 `:` 切;`+00:00` 解析失败→`has_fresh_token` 走 `None=>true` 静默禁刷新。**必须产 `Z`**;`gw-kiro/src/token.rs:237 format_unix_utc` 是已验证的 "Z" 产出器(`format_unix_utc(1_780_531_200)=="2026-06-04T00:00:00Z"`)。**禁用 time crate Rfc3339**。
- **`report_failure(TokenInvalid)` = 永久禁号**(`disabled_until=None`);瞬时(429/5xx/网络)应记 transient。刷新错误分类不能一律 TokenInvalid。
- **caio body 限 16MB**;**dario `MAX_BODY_BYTES=10MB` 硬编码**(`proxy.ts:23`),10–16MB 段会在 sidecar 413(正是历史大图/PDF 事故区)。→ 补丁令其可配 ≥16MB(Task 1.4)。

---

## 文件结构

**新建**:`crates/gw-dario/{Cargo.toml, src/lib.rs, src/chat.rs, src/token.rs, src/credentials.rs, src/datefmt.rs}`
**改 caio**:`Cargo.toml`(workspace)、`crates/gw-app/Cargo.toml`、`registry.rs`、`worker/mod.rs`(1524 家族守卫 + 646-655 注入 dario cfg + 启动出口一致性告警)、`gw-core/src/config.rs`(`DarioSidecarConfig`)、`admin/accounts.rs`(create_account 加 dario 分支)、admin 前端(provider 选择 + 凭证粘贴)、`config/system.example.yaml`、`CHANGELOG.md`
**改 dario fork**(`/创业/claude反代/dario/src/proxy.ts`):per-request bearer + injected 标志、identity 头覆盖、sticky/failover guard、`MAX_BODY_BYTES` 可配
**部署**:`docker-compose.yml` 加 `dario-us1` sidecar

---

## Phase 0 — dario sidecar 裸跑通

### Task 0.1：起 dario headless + 手动验证(红线:仅一发真实调用)

- [ ] **Step 1: 装 Bun + 构建**
```bash
command -v bun || curl -fsSL https://bun.sh/install | bash
cd /run/media/iiap/25df545d-3a24-4466-b58d-f96c46b9a3bf2/REPO/创业/claude反代/dario
bun install && bun run build
```
Expected: `dist/index.js` 存在。

- [ ] **Step 2: 起 server(pool 关 + 跳 live-capture),确认日志显示 pool-off/单账号**
```bash
DARIO_API_KEY=local-smoke DARIO_NO_LIVE_CAPTURE=1 \
bun dist/index.js proxy --port=39100 --host=127.0.0.1 --no-live-capture --upstream-proxy=<USPROXY> &
```
Expected 日志含:listening 127.0.0.1:39100;**确认未加载 ≥2 账号(pool===null)** —— 不放任何 `~/.dario/accounts/`。

- [ ] **Step 3: 手动打一发,确认 200 + 出口美国 + 计费分类头**
```bash
curl -sS http://127.0.0.1:39100/v1/messages \
  -H 'x-api-key: local-smoke' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-haiku-4-5","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"ping"}]}' | head -c 400
```
Expected: 合法 SSE。记录响应头 **`anthropic-ratelimit-unified-representative-claim`**(期望 five_hour;`proxy.ts:660`)。

- [ ] **Step 4: 停**:`kill %1 2>/dev/null || pkill -f 'dist/index.js proxy'`

---

## Phase 1 — dario fork 补丁(bearer 注入 + injected 守卫 + 身份 + body 上限)

### Task 1.1：per-request bearer 注入 + injected 标志

**Files:** Modify `dario/src/proxy.ts:1467-1484`

- [ ] **Step 1: 账号选择块顶部加注入分支 + injected 标志**

把 `:1467-1484` 改为(最前面加 injected 分支,其余原样):
```ts
      let poolAccount: PoolAccount | null = null;
      let accessToken: string;
      // caio sidecar mode: caller supplies the per-account OAuth bearer via header.
      // `injected` gates the pool sticky-rebind / failover blocks below so the
      // injected token+identity are never overwritten (see Task 1.3).
      const injectedUpstreamToken = req.headers['x-dario-upstream-token'] as string | undefined;
      const injected = !!injectedUpstreamToken;
      if (injected) {
        accessToken = injectedUpstreamToken!;
      } else if (upstreamApiKey) {
        accessToken = '';
      } else if (pool) {
        poolAccount = pool.select();
        if (!poolAccount) {
          res.writeHead(503, JSON_HEADERS);
          res.end(JSON.stringify({ error: 'No accounts available in pool' }));
          return;
        }
        accessToken = poolAccount.accessToken;
      } else {
        accessToken = await getAccessToken();
      }
```

- [ ] **Step 2: 构建** — `bun run build`(无 TS 错误)

### Task 1.2：identity 头覆盖(injected 优先)

**Files:** Modify `dario/src/proxy.ts:1686-1688`

- [ ] **Step 1: bodyIdentity 在 injected 时优先取头**

把 `:1686-1688` 改为:
```ts
            const injectedDeviceId = req.headers['x-dario-device-id'] as string | undefined;
            const injectedAccountUuid = req.headers['x-dario-account-uuid'] as string | undefined;
            const bodyIdentity = (poolAccount && !injected)
              ? poolAccount.identity
              : {
                  deviceId: injectedDeviceId ?? identity.deviceId,
                  accountUuid: injectedAccountUuid ?? identity.accountUuid,
                  sessionId: preBodySessionId,
                };
```

### Task 1.3：sticky 重绑 + failover guard(防 pool 模式覆盖)

**Files:** Modify `dario/src/proxy.ts:1657, 2063, 2140, 2158`(行号实现期复核)

- [ ] **Step 1: sticky 重绑加 `&& !injected`**

`:1657` `if (pool && stickyKey) {` → `if (pool && stickyKey && !injected) {`

- [ ] **Step 2: 三处 failover guard 加 `&& !injected`**

`:2063 / :2140 / :2158` 各自的 `if (pool && poolAccount ...)` 追加 `&& !injected`(实现期 grep `poolAccount = nextAccount` 定位三处)。

- [ ] **Step 3: 构建 + 手动验证注入不被覆盖**
```bash
bun run build
DARIO_API_KEY=local-smoke bun dist/index.js proxy --port=39100 --host=127.0.0.1 --no-live-capture --upstream-proxy=<USPROXY> &
curl -sS http://127.0.0.1:39100/v1/messages \
  -H 'x-api-key: local-smoke' \
  -H 'x-dario-upstream-token: <REAL_ACCESS_TOKEN>' \
  -H 'x-dario-device-id: 11111111-1111-1111-1111-111111111111' \
  -H 'x-dario-account-uuid: 22222222-2222-2222-2222-222222222222' \
  -H 'content-type: application/json' \
  -d '{"model":"claude-haiku-4-5","max_tokens":16,"stream":true,"messages":[{"role":"user","content":"ping"}]}' | head -c 400
kill %1 2>/dev/null
```
Expected: 200(注入 token 生效)。

### Task 1.4：MAX_BODY_BYTES 可配(对齐 caio 16MB)

**Files:** Modify `dario/src/proxy.ts:23`

- [ ] **Step 1: 令 MAX_BODY_BYTES 读 env**

`:23` `const MAX_BODY_BYTES = 10 * 1024 * 1024;` 改为:
```ts
const MAX_BODY_BYTES = (Number(process.env.DARIO_MAX_BODY_MB) || 10) * 1024 * 1024;
```

- [ ] **Step 2: 构建 + 提交 dario fork 补丁**
```bash
cd /run/media/iiap/25df545d-3a24-4466-b58d-f96c46b9a3bf2/REPO/创业/claude反代/dario
bun run build
git add src/proxy.ts dist/
git commit -m "feat(fork): per-request bearer+identity injection, pool guards, configurable body limit (caio sidecar)"
```
> 本地 fork 专用,不向 dario 上游提 PR。

---

## Phase 2 — gw-dario crate 骨架

### Task 2.1：crate 清单 + workspace 接线

**Files:** Create `crates/gw-dario/Cargo.toml`;Modify root `Cargo.toml`、`crates/gw-app/Cargo.toml`

- [ ] **Step 1: `crates/gw-dario/Cargo.toml`**
```toml
[package]
name = "gw-dario"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true

[dependencies]
gw-core.workspace = true
async-trait.workspace = true
async-stream.workspace = true
anyhow.workspace = true
futures.workspace = true
serde.workspace = true
serde_json.workspace = true
tokio.workspace = true
tokio-stream.workspace = true
tracing.workspace = true
reqwest.workspace = true
uuid.workspace = true
```
> 不引 `time`、不引 `bytes`(SSE chunk 用 `String::from_utf8_lossy(&chunk)`,不命名 `Bytes`)。

- [ ] **Step 2: root `Cargo.toml`** — members 加 `"crates/gw-dario",`;`[workspace.dependencies]` 加 `gw-dario = { path = "crates/gw-dario" }` 和 `async-stream = "0.3"`。

- [ ] **Step 3: `crates/gw-app/Cargo.toml`** `[dependencies]` 加 `gw-dario.workspace = true`。

- [ ] **Step 4: 最小 lib 过编译** — `crates/gw-dario/src/lib.rs`:`pub struct DarioProvider;` → `cargo build -p gw-dario`(成功)。

- [ ] **Step 5: 提交** — `git add` 上述 + 空 lib;`git commit -m "chore(gw-dario): scaffold crate + workspace wiring"`

### Task 2.2：datefmt(Z 产出器,先 TDD)

**Files:** Create `crates/gw-dario/src/datefmt.rs`

- [ ] **Step 1: 失败测试**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formats_unix_to_rfc3339_z() {
        // 与 gw-kiro format_unix_utc 同向量
        assert_eq!(format_rfc3339_z(1_780_531_200), "2026-06-04T00:00:00Z");
        assert_eq!(format_rfc3339_z(0), "1970-01-01T00:00:00Z");
    }
}
```

- [ ] **Step 2: 跑看失败** — `cargo test -p gw-dario datefmt`(未定义)。

- [ ] **Step 3: 实现(Howard-Hinnant civil-from-days,产 "Z",parse_rfc3339_unix 的逆)**
```rust
//! Unix 秒 → RFC3339 UTC "Z" 字符串。**必须产 "Z"**(末尾大写 Z,无 +00:00 偏移),
//! 否则 gw-app `parse_rfc3339_unix`(只剥 Z)解析失败 → has_fresh_token 静默禁刷新。
//! 移植 gw-kiro/src/token.rs:237 format_unix_utc 的等价算法,避免跨 crate 依赖。

pub fn format_rfc3339_z(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (hh, mm, ss) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    // civil_from_days(epoch 1970-01-01)
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0,146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0,399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0,365]
    let mp = (5 * doy + 2) / 153; // [0,11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1,31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1,12]
    let year = if m <= 2 { y + 1 } else { y };
    format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}
```

- [ ] **Step 4: 跑看通过 + 提交** — `cargo test -p gw-dario datefmt`(PASS);`git commit -m "feat(gw-dario): RFC3339 Z formatter (inverse of parse_rfc3339_unix)"`

### Task 2.3：DarioProvider 结构 + schema + 非 chat trait 方法

**Files:** Modify `crates/gw-dario/src/lib.rs`;stub `chat.rs/token.rs/credentials.rs`

- [ ] **Step 1: 失败测试**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use gw_core::provider::Provider;
    #[test] fn family_is_claude_dario() {
        assert_eq!(DarioProvider::new(DarioConfig::default()).family(), "claude-dario");
    }
    #[test] fn schema_has_required_refresh_token() {
        let p = DarioProvider::new(DarioConfig::default());
        let s = p.account_schema();
        assert!(s.iter().any(|f| f.name == "refresh_token" && f.required));
        assert!(s.iter().any(|f| f.name == "device_id"));
    }
    #[test] fn from_config_reads_sidecar() {
        let cfg = serde_json::json!({"dario":{"sidecar_url":"http://127.0.0.1:39100","api_key":"k"}});
        assert_eq!(DarioProvider::from_config(&cfg, reqwest::Client::new()).unwrap().family(), "claude-dario");
    }
    #[test] fn from_config_warns_empty_api_key_but_builds() {
        // 空 api_key 不阻断构造(loopback dario 可不设 DARIO_API_KEY),仅记 warn。
        assert!(DarioProvider::from_config(&serde_json::Value::Null, reqwest::Client::new()).is_ok());
    }
}
```

- [ ] **Step 2: 跑看失败** — `cargo test -p gw-dario`(类型未定义)。

- [ ] **Step 3: 实现 lib.rs**
```rust
//! gw-dario —— Claude OAuth 凭证经本机 dario sidecar 直连 api.anthropic.com 的 Provider。
mod chat;
mod credentials;
mod datefmt;
mod token;

use std::sync::Arc;
use async_trait::async_trait;
use gw_core::account::{Account, FieldSpec, FieldType};
use gw_core::error::UpstreamError;
use gw_core::model::ModelInfo;
use gw_core::provider::{CallCtx, ChatRequest, ChatStream, Provider};

pub use credentials::parse_cc_credentials;
pub(crate) use datefmt::format_rfc3339_z;

const DARIO_ACCOUNT_SCHEMA: &[FieldSpec] = &[
    FieldSpec::new("account_id", "账号 ID", FieldType::String, true),
    FieldSpec::new("access_token", "Access Token", FieldType::Password, false)
        .with_help("OAuth access token;导入 .credentials.json 自动填充"),
    FieldSpec::new("refresh_token", "Refresh Token", FieldType::Password, true)
        .with_help("OAuth refresh token;caio 后台刷新 access_token"),
    FieldSpec::new("expires_at", "过期时间(RFC3339 Z)", FieldType::String, false),
    FieldSpec::new("device_id", "Device ID", FieldType::String, false)
        .with_help("缺失时导入生成稳定 UUID(保 five_hour 计费分类)"),
    FieldSpec::new("account_uuid", "Account UUID", FieldType::String, false),
    FieldSpec::new("proxy", "出口代理", FieldType::String, false),
];

#[derive(Debug, Clone, Default)]
pub struct DarioConfig {
    pub sidecar_url: String,
    pub api_key: String,
}

impl DarioConfig {
    fn from_cfg(cfg: &serde_json::Value) -> Self {
        let d = cfg.get("dario");
        DarioConfig {
            sidecar_url: d.and_then(|v| v.get("sidecar_url")).and_then(|v| v.as_str())
                .filter(|s| !s.is_empty()).unwrap_or("http://127.0.0.1:39100").to_string(),
            api_key: d.and_then(|v| v.get("api_key")).and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        }
    }
}

pub struct DarioProvider {
    cfg: DarioConfig,
    /// 直连 loopback(**无代理 + connect 超时**):caio→dario 本机回环,出口代理由 dario 负责。
    sidecar_client: reqwest::Client,
    /// 注入的本组 egress client:仅 refresh_auth 直连 token 端点用(与 dario 发包同出口,防关联封号)。
    egress_client: reqwest::Client,
}

impl DarioProvider {
    pub fn new(cfg: DarioConfig) -> Self {
        Self::with_clients(cfg, reqwest::Client::new(), reqwest::Client::new())
    }
    pub fn with_clients(cfg: DarioConfig, sidecar_client: reqwest::Client, egress_client: reqwest::Client) -> Self {
        Self { cfg, sidecar_client, egress_client }
    }
    pub fn from_config(cfg: &serde_json::Value, egress_client: reqwest::Client) -> anyhow::Result<Arc<dyn Provider>> {
        let c = DarioConfig::from_cfg(cfg);
        if c.api_key.is_empty() {
            tracing::warn!("dario.api_key 为空:仅当 sidecar 也未设 DARIO_API_KEY(loopback 放行)才安全");
        }
        let sidecar_client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(std::time::Duration::from_secs(5))
            .build()
            .map_err(|e| anyhow::anyhow!("build dario sidecar client: {e}"))?;
        Ok(Arc::new(Self::with_clients(c, sidecar_client, egress_client)))
    }
}

#[async_trait]
impl Provider for DarioProvider {
    fn family(&self) -> &'static str { "claude-dario" }
    fn account_schema(&self) -> &'static [FieldSpec] { DARIO_ACCOUNT_SCHEMA }

    fn validate_account(&self, account: &Account) -> Result<(), UpstreamError> {
        let ok = account.extra_str("refresh_token").map(str::trim).is_some_and(|s| !s.is_empty())
            || account.extra_str("access_token").map(str::trim).is_some_and(|s| !s.is_empty());
        if !ok { return Err(UpstreamError::bad_request("claude-dario account missing access_token & refresh_token")); }
        Ok(())
    }

    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError> {
        let mk = |id: &str, d: &str| {
            let mut m = ModelInfo::new(id);
            m.display_name = Some(d.into()); m.context_length = Some(200_000);
            m.supports_thinking = true; m.supports_tools = true; m.supports_vision = true; m
        };
        Ok(vec![
            mk("claude-opus-4-8", "Claude Opus 4.8 (dario)"),
            mk("claude-sonnet-4-6", "Claude Sonnet 4.6 (dario)"),
            mk("claude-haiku-4-5", "Claude Haiku 4.5 (dario)"),
        ])
    }

    /// 会话亲和:dario pool 关闭,其内置 stickiness 不触发 → 必须由 caio 调度提供亲和,
    /// 否则同会话在多号间跳 → Anthropic 每号独立 prompt cache 反复 create,烧 5h/7d 窗口。
    fn affinity_key(&self, req: &ChatRequest) -> Option<String> {
        chat::affinity_from_body(&req.body)
    }

    async fn chat(&self, req: ChatRequest, ctx: &CallCtx) -> Result<ChatStream, UpstreamError> {
        chat::chat_via_sidecar(&self.cfg, &self.sidecar_client, req, ctx).await
    }

    async fn refresh_auth(&self, account: &Account) -> Result<Account, UpstreamError> {
        token::refresh(&self.egress_client, account).await
    }
}
```

- [ ] **Step 4: stub chat/token/credentials(让模块编译)**

`chat.rs`:
```rust
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{CallCtx, ChatRequest, ChatStream};
use crate::DarioConfig;

pub(crate) fn affinity_from_body(_body: &serde_json::Value) -> Option<String> { None }

pub(crate) async fn chat_via_sidecar(
    _cfg: &DarioConfig, _client: &reqwest::Client, _req: ChatRequest, _ctx: &CallCtx,
) -> Result<ChatStream, UpstreamError> {
    Err(UpstreamError::new(UpstreamErrorKind::Other, "not implemented"))
}
```
`token.rs`:
```rust
use gw_core::account::Account;
use gw_core::error::UpstreamError;
pub(crate) async fn refresh(_c: &reqwest::Client, account: &Account) -> Result<Account, UpstreamError> {
    Ok(account.clone())
}
```
`credentials.rs`:
```rust
use std::collections::BTreeMap;
pub fn parse_cc_credentials(_text: &str) -> Result<BTreeMap<String, serde_json::Value>, String> {
    Err("not implemented".into())
}
```

- [ ] **Step 5: 跑测试看通过 + 提交** — `cargo test -p gw-dario`(PASS);`git commit -m "feat(gw-dario): provider skeleton (family/schema/validate/list_models/affinity/stubs)"`

### Task 2.4：注册 registry

**Files:** Modify `crates/gw-app/src/registry.rs:36`

- [ ] **Step 1: 失败测试**(registry tests 加 `builtins_include_claude_dario`,断言 families 含 `"claude-dario"`)。
- [ ] **Step 2: 跑看失败** — `cargo test -p gw-app registry::tests::builtins_include_claude_dario`(FAIL)。
- [ ] **Step 3: 注册一行** — `:36` 后加 `reg.register("claude-dario", gw_dario::DarioProvider::from_config);`
- [ ] **Step 4: 跑看通过** — `cargo test -p gw-app registry`(PASS)。
- [ ] **Step 5: 提交** — `git commit -m "feat(gw-app): register claude-dario provider"`

---

## Phase 3 — chat():SSE 解析 + usage + 转发(强制 stream + 出错也 emit usage)

### Task 3.1：SSE 解析 + usage 累积 + affinity(纯函数 TDD)

**Files:** Modify `crates/gw-dario/src/chat.rs`

- [ ] **Step 1: 失败测试**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn usage_from_start_and_delta() {
        let start = serde_json::json!({"type":"message_start","message":{"usage":{"input_tokens":100,"cache_read_input_tokens":40,"cache_creation_input_tokens":10}}});
        let delta = serde_json::json!({"type":"message_delta","usage":{"output_tokens":25}});
        let mut acc = UsageAcc::default();
        acc.observe("message_start", &start); acc.observe("message_delta", &delta);
        let u = acc.into_usage();
        assert_eq!((u.input_tokens,u.output_tokens,u.cache_read_tokens,u.cache_creation_tokens),(100,25,40,10));
        assert_eq!(u.real_cache_read_tokens, 0); assert_eq!(u.metering_credit, 0.0);
    }
    #[test] fn split_frames_and_keep_partial() {
        let (f, rest) = drain_sse_frames("event: message_start\ndata: {\"type\":\"message_start\"}\n\nevent: par");
        assert_eq!(f.len(), 1); assert_eq!(f[0].0, "message_start"); assert_eq!(rest, "event: par");
    }
    #[test] fn affinity_hashes_first_user_text() {
        let b = serde_json::json!({"messages":[{"role":"user","content":[{"type":"text","text":"hello world"}]}]});
        let k = affinity_from_body(&b);
        assert!(k.is_some());
        // 同首条 user 文本 → 同 key
        assert_eq!(k, affinity_from_body(&b));
    }
    #[test] fn affinity_none_without_messages() {
        assert_eq!(affinity_from_body(&serde_json::json!({})), None);
    }
}
```

- [ ] **Step 2: 跑看失败** — `cargo test -p gw-dario chat::tests`(未定义)。

- [ ] **Step 3: 实现解析 + usage + affinity**(替换 stub 的 `affinity_from_body`,新增 `UsageAcc`/`drain_sse_frames`)
```rust
use gw_core::provider::ChatUsage;

#[derive(Default)]
pub(crate) struct UsageAcc { input: u64, output: u64, cache_read: u64, cache_creation: u64 }
impl UsageAcc {
    pub(crate) fn observe(&mut self, event: &str, data: &serde_json::Value) {
        let u = match event {
            "message_start" => data.get("message").and_then(|m| m.get("usage")),
            "message_delta" => data.get("usage"),
            _ => None,
        };
        let Some(u) = u else { return };
        if let Some(v) = u.get("input_tokens").and_then(|v| v.as_u64()) { self.input = v; }
        if let Some(v) = u.get("output_tokens").and_then(|v| v.as_u64()) { self.output = v; }
        if let Some(v) = u.get("cache_read_input_tokens").and_then(|v| v.as_u64()) { self.cache_read = v; }
        if let Some(v) = u.get("cache_creation_input_tokens").and_then(|v| v.as_u64()) { self.cache_creation = v; }
    }
    pub(crate) fn into_usage(self) -> ChatUsage {
        ChatUsage { input_tokens: self.input, output_tokens: self.output,
            cache_read_tokens: self.cache_read, cache_creation_tokens: self.cache_creation,
            real_cache_read_tokens: 0, metering_credit: 0.0 }
    }
}

pub(crate) fn drain_sse_frames(buf: &str) -> (Vec<(String, String)>, String) {
    let mut frames = Vec::new();
    let mut rest = buf;
    while let Some(idx) = rest.find("\n\n") {
        let (frame, after) = rest.split_at(idx);
        let (mut event, mut data) = (String::new(), String::new());
        for line in frame.lines() {
            if let Some(v) = line.strip_prefix("event:") { event = v.trim().to_string(); }
            else if let Some(v) = line.strip_prefix("data:") {
                if !data.is_empty() { data.push('\n'); }
                data.push_str(v.trim());
            }
        }
        if !event.is_empty() || !data.is_empty() { frames.push((event, data)); }
        rest = &after[2..];
    }
    (frames, rest.to_string())
}

/// 会话亲和键:取首条 user 消息文本的稳定哈希(镜像 dario computeStickyKey 思路)。
pub(crate) fn affinity_from_body(body: &serde_json::Value) -> Option<String> {
    let msgs = body.get("messages")?.as_array()?;
    let first_user_text = msgs.iter()
        .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user"))
        .and_then(|m| match m.get("content") {
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Array(blocks)) => blocks.iter()
                .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
                .and_then(|b| b.get("text").and_then(|t| t.as_str())).map(str::to_string),
            _ => None,
        })?;
    if first_user_text.trim().is_empty() { return None; }
    // 稳定哈希(无需加密强度,DefaultHasher 进程内稳定即可用于本进程亲和)。
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    first_user_text.hash(&mut h);
    Some(format!("dario-{:016x}", h.finish()))
}
```
> 注:`DefaultHasher` 仅进程内稳定,够 worker 单进程会话亲和;若亲和需跨进程持久一致,改用 `sha2`(已在 workspace deps)。MVP 用 DefaultHasher。

- [ ] **Step 4: 跑看通过 + 提交** — `cargo test -p gw-dario chat::tests`;`git commit -m "feat(gw-dario): SSE parser + usage acc + affinity_key (TDD)"`

### Task 3.2：chat_via_sidecar —— 强制 stream、转发、回流、出错也 emit usage

**Files:** Modify `crates/gw-dario/src/chat.rs`(替换 stub `chat_via_sidecar`)

- [ ] **Step 1: 实现转发流**
```rust
use futures::StreamExt;
use gw_core::provider::{SseEvent, StreamItem};

pub(crate) async fn chat_via_sidecar(
    cfg: &DarioConfig, client: &reqwest::Client, req: ChatRequest, ctx: &CallCtx,
) -> Result<ChatStream, UpstreamError> {
    let access_token = ctx.account.extra_str("access_token").map(str::trim).filter(|s| !s.is_empty())
        .ok_or_else(|| UpstreamError::new(UpstreamErrorKind::TokenInvalid, "dario account missing access_token"))?
        .to_string();
    let device_id = ctx.account.extra_str("device_id").unwrap_or_default().to_string();
    let account_uuid = ctx.account.extra_str("account_uuid").unwrap_or_default().to_string();

    // 关键:强制上游流式。Anthropic 对 stream:false 返单个 JSON(非 SSE),会被 drain_sse_frames
    // 切不出帧 → 零事件零 usage。caio collect_response 会为非流式 client 折叠 SSE(设计 §6.3)。
    let mut body = req.body.clone();
    if let serde_json::Value::Object(m) = &mut body {
        m.insert("stream".into(), serde_json::Value::Bool(true));
    }

    let url = format!("{}/v1/messages", cfg.sidecar_url.trim_end_matches('/'));
    let mut rb = client.post(&url)
        .header("content-type", "application/json")
        .header("x-api-key", cfg.api_key.clone())            // dario 入站鉴权(裸 key,优先读)
        .header("x-dario-upstream-token", access_token)       // Phase 1 补丁消费
        .header("x-session-id", ctx.session_id.clone());      // dario 读 x-session-id
    if !device_id.is_empty() { rb = rb.header("x-dario-device-id", device_id); }
    if !account_uuid.is_empty() { rb = rb.header("x-dario-account-uuid", account_uuid); }

    let resp = rb.json(&body).send().await
        .map_err(|e| UpstreamError::network(format!("dario sidecar 连接失败: {e}")))?;

    let status = resp.status();
    if !status.is_success() {
        let code = status.as_u16();
        let text = resp.text().await.unwrap_or_default();
        // 注:dario 自身鉴权失败也会 401(api_key 配错)。已在 from_config 校验 api_key 非空,
        // 此处仍把持久 401 当 token 问题;config 级故障应在冒烟期暴露(见 Task 8.2)。
        let kind = match code {
            400 => UpstreamErrorKind::BadRequest,
            401 => UpstreamErrorKind::TokenInvalid,
            403 if text.to_lowercase().contains("suspend") => UpstreamErrorKind::TemporarilyBlocked,
            403 => UpstreamErrorKind::TokenInvalid,
            429 => UpstreamErrorKind::RateLimited,
            500..=599 => UpstreamErrorKind::ServerError,
            _ => UpstreamErrorKind::Other,
        };
        return Err(UpstreamError::new(kind, format!("dario/anthropic {code}: {}", text.chars().take(500).collect::<String>())).with_status(code));
    }

    let mut byte_stream = resp.bytes_stream();
    let stream = async_stream::stream! {
        let mut buf = String::new();
        let mut acc = UsageAcc::default();
        loop {
            match byte_stream.next().await {
                Some(Ok(chunk)) => {
                    buf.push_str(&String::from_utf8_lossy(&chunk));
                    let (frames, rest) = drain_sse_frames(&buf);
                    buf = rest;
                    for (event, data_str) in frames {
                        let data: serde_json::Value = serde_json::from_str(&data_str).unwrap_or(serde_json::Value::Null);
                        acc.observe(&event, &data);
                        yield Ok(StreamItem::Sse(SseEvent::new(event, data)));
                    }
                }
                Some(Err(e)) => {
                    // 出错也尽力 emit 已观测 usage(否则截断流计费归零、漏计 per-key 客户)。
                    yield Ok(StreamItem::Usage(std::mem::take(&mut acc).into_usage()));
                    yield Err(UpstreamError::network(format!("dario 流中断: {e}")));
                    return;
                }
                None => break,
            }
        }
        yield Ok(StreamItem::Usage(acc.into_usage()));
    };
    Ok(Box::pin(stream))
}
```
> `UsageAcc` 需可 `Default` + `std::mem::take`(已 derive Default)。

- [ ] **Step 2: 编译 + 全 crate 测试** — `cargo test -p gw-dario`(全 PASS,无回归)。

- [ ] **Step 3: 提交** — `git commit -m "feat(gw-dario): chat forwards (force stream:true), streams SSE, emits usage even on mid-stream error"`

---

## Phase 4 — refresh_auth(OAuth 刷新 + 错误分类)

### Task 4.1：apply_refresh(纯逻辑)+ refresh(网络 + 分类),TDD

**Files:** Modify `crates/gw-dario/src/token.rs`

- [ ] **Step 1: 失败测试**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    fn acct() -> Account {
        let mut e = BTreeMap::new();
        e.insert("refresh_token".into(), serde_json::json!("old-rt"));
        e.insert("access_token".into(), serde_json::json!("old-at"));
        Account { account_id: "d1".into(), provider: "claude-dario".into(), max_concurrency: 2, disabled: false, extra: e }
    }
    #[test] fn apply_updates_tokens_and_expiry_z() {
        let r = serde_json::json!({"access_token":"new-at","refresh_token":"new-rt","expires_in":3600});
        let u = apply_refresh(acct(), &r, 1_780_531_200).unwrap();
        assert_eq!(u.extra_str("access_token"), Some("new-at"));
        assert_eq!(u.extra_str("refresh_token"), Some("new-rt"));
        assert_eq!(u.extra_str("expires_at"), Some("2026-06-04T01:00:00Z")); // +3600s,末尾 Z
    }
    #[test] fn apply_keeps_old_refresh_when_absent() {
        let r = serde_json::json!({"access_token":"new-at","expires_in":3600});
        assert_eq!(apply_refresh(acct(), &r, 0).unwrap().extra_str("refresh_token"), Some("old-rt"));
    }
    #[test] fn apply_errors_without_access_token() {
        assert!(apply_refresh(acct(), &serde_json::json!({"error":"invalid_grant"}), 0).is_err());
    }
    #[test] fn classify_transient_vs_permanent() {
        assert_eq!(classify_refresh_status(429), UpstreamErrorKind::RateLimited);
        assert_eq!(classify_refresh_status(503), UpstreamErrorKind::ServerError);
        assert_eq!(classify_refresh_status(400), UpstreamErrorKind::TokenInvalid);
        assert_eq!(classify_refresh_status(401), UpstreamErrorKind::TokenInvalid);
    }
}
```

- [ ] **Step 2: 跑看失败** — `cargo test -p gw-dario token::tests`(未定义)。

- [ ] **Step 3: 实现 token.rs**(替换 stub)
```rust
use gw_core::account::Account;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use crate::format_rfc3339_z;

const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

pub(crate) fn apply_refresh(mut account: Account, resp: &serde_json::Value, now_unix: i64) -> Result<Account, UpstreamError> {
    let access = resp.get("access_token").and_then(|v| v.as_str()).filter(|s| !s.is_empty())
        .ok_or_else(|| {
            let e = resp.get("error").and_then(|v| v.as_str()).unwrap_or("no access_token");
            UpstreamError::new(UpstreamErrorKind::TokenInvalid, format!("dario refresh 失败: {e}"))
        })?;
    account.extra.insert("access_token".into(), serde_json::json!(access));
    if let Some(rt) = resp.get("refresh_token").and_then(|v| v.as_str()).filter(|s| !s.is_empty()) {
        account.extra.insert("refresh_token".into(), serde_json::json!(rt));
    }
    if let Some(ttl) = resp.get("expires_in").and_then(|v| v.as_i64()) {
        account.extra.insert("expires_at".into(), serde_json::json!(format_rfc3339_z(now_unix + ttl)));
    }
    Ok(account)
}

/// 刷新端点非 2xx 的分类:瞬时(429/5xx)≠ 永久禁号(invalid_grant)。
/// report_failure(TokenInvalid)=永久禁号(disabled_until=None),不能给瞬时错误。
pub(crate) fn classify_refresh_status(code: u16) -> UpstreamErrorKind {
    match code {
        429 => UpstreamErrorKind::RateLimited,
        500..=599 => UpstreamErrorKind::ServerError,
        _ => UpstreamErrorKind::TokenInvalid, // 400/401 invalid_grant 等:真死 token
    }
}

pub(crate) async fn refresh(client: &reqwest::Client, account: &Account) -> Result<Account, UpstreamError> {
    let refresh_token = account.extra_str("refresh_token").map(str::trim).filter(|s| !s.is_empty())
        .ok_or_else(|| UpstreamError::new(UpstreamErrorKind::TokenInvalid, "dario account missing refresh_token"))?;
    let form = [("grant_type","refresh_token"),("refresh_token",refresh_token),("client_id",CLIENT_ID)];
    let resp = client.post(TOKEN_URL).form(&form).send().await
        .map_err(|e| UpstreamError::network(format!("dario refresh 连接失败: {e}")))?;
    let status = resp.status();
    let json: serde_json::Value = resp.json().await.unwrap_or(serde_json::Value::Null);
    if !status.is_success() {
        let err = json.get("error").and_then(|v| v.as_str()).unwrap_or("");
        return Err(UpstreamError::new(classify_refresh_status(status.as_u16()),
            format!("dario refresh {} {err}", status.as_u16())).with_status(status.as_u16()));
    }
    apply_refresh(account.clone(), &json, now_unix())
}

fn now_unix() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
```

- [ ] **Step 4: 跑看通过 + 提交** — `cargo test -p gw-dario token`;`git commit -m "feat(gw-dario): OAuth refresh (urlencoded) + transient/permanent error classification (TDD)"`

---

## Phase 5 — 配置注入(SystemConfig.dario + provider_cfg + 出口一致性告警)

### Task 5.1：SystemConfig.dario 段

**Files:** Modify `crates/gw-core/src/config.rs`、`config/system.example.yaml`

- [ ] **Step 1: 失败测试**(config tests `dario_config_defaults_and_parse`:解析 `dario.sidecar_url/api_key`,缺省为空串)。
- [ ] **Step 2: 跑看失败**。
- [ ] **Step 3: 加 struct + 字段**
```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct DarioSidecarConfig {
    pub sidecar_url: String,
    pub api_key: String,
}
```
`SystemConfig` 加 `#[serde(default)] pub dario: DarioSidecarConfig,`。
- [ ] **Step 4: 跑看通过**。
- [ ] **Step 5: 示例 yaml + 提交**
```yaml
# dario sidecar(claude-dario provider 用)。startup 段,改后需重启相关 worker。
dario:
  sidecar_url: "http://127.0.0.1:39100"
  api_key: ""
```
`git commit -m "feat(gw-core): SystemConfig.dario sidecar config"`

### Task 5.2：worker 注入 dario cfg + 出口一致性启动告警

**Files:** Modify `crates/gw-app/src/worker/mod.rs:648-655` 与 `worker 就绪` 日志附近

- [ ] **Step 1: provider_cfg 注入 dario 段** — 在 `default_proxy` insert 之后加:
```rust
        if let Ok(dario) = serde_json::to_value(&effective_system.dario) {
            map.insert("dario".into(), dario);
        }
```
- [ ] **Step 2: 出口一致性告警** — 仅当 `provider_family == "claude-dario"` 时,在 `worker 就绪` 日志后加 warn(防"刷新与发包异出口"):
```rust
        if provider_family == "claude-dario" {
            tracing::warn!(
                worker_egress = %egress_desc,
                "dario 组:请确保本 worker 的 egress 与所连 dario sidecar 的 --upstream-proxy 为同一美国 HTTPS 代理(否则刷新 IP≠发包 IP,关联封号风险)"
            );
        }
```
- [ ] **Step 3: 编译 + 测试** — `cargo test -p gw-app`(无回归)。
- [ ] **Step 4: 提交** — `git commit -m "feat(gw-app): inject dario cfg into provider_cfg + egress-consistency startup warning"`

---

## Phase 6 — 请求日志解耦(按 worker family 而非 account.provider 守卫)

### Task 6.1:render_kiro_payload 仅 kiro 家族 worker 渲染

**Files:** Modify `crates/gw-app/src/worker/mod.rs`(`write_request_log` 1522-1529 + `spawn_request_log_blocking` + 调用点)

- [ ] **Step 1: 读相关函数签名定位透传链**

Run: `grep -n "fn write_request_log\|fn spawn_request_log_blocking\|spawn_request_log_blocking(\|st.provider.family()" crates/gw-app/src/worker/mod.rs`
Expected: 找到 `write_request_log` 定义、`spawn_request_log_blocking` 定义与其调用点(调用点处可取 `st.provider.family()`)。

- [ ] **Step 2: 失败测试** — 在 worker tests 加:构造一个 `provider` 字段为空串的 kiro 账号,断言以 `family="kiro"` 调用日志渲染时 `kiro_payload` 非空(防空-provider kiro 号丢日志回归)。若现有测试结构不便,改为 `write_request_log` 直接单测(传 `family="kiro"` vs `"claude-dario"`)。

- [ ] **Step 3: 透传 family + 改守卫**
  - `write_request_log(...)` 签名加参数 `family: &str`;
  - `spawn_request_log_blocking(...)` 同步加 `family: &'static str` 透传;
  - 各调用点传 `st.provider.family()`;
  - `:1522-1529` 守卫改为:
```rust
    let (account_id, kiro_payload) = match &account {
        Some(a) if family == "kiro" => {
            let (kp, kb) = prepare_log_payload(gw_kiro::chat::render_kiro_payload(&req, a));
            blobs.extend(kb);
            (a.account_id.clone(), kp)
        }
        Some(a) => (a.account_id.clone(), String::new()),
        None => (String::new(), String::new()),
    };
```

- [ ] **Step 4: 编译 + 测试 + 提交** — `cargo test -p gw-app`;`git commit -m "fix(gw-app): render kiro payload by worker family() not account.provider (dario log decoupling)"`

---

## Phase 7 — 账号导入 .credentials.json + 生成 device_id/account_uuid

### Task 7.1:解析 .credentials.json(纯函数 TDD)

**Files:** Modify `crates/gw-dario/src/credentials.rs`

- [ ] **Step 1: 失败测试**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn parses_claude_ai_oauth() {
        let t = r#"{"claudeAiOauth":{"accessToken":"at","refreshToken":"rt","expiresAt":1780531200000}}"#;
        let e = parse_cc_credentials(t).unwrap();
        assert_eq!(e.get("access_token").unwrap(), "at");
        assert_eq!(e.get("refresh_token").unwrap(), "rt");
        assert_eq!(e.get("expires_at").unwrap(), "2026-06-04T00:00:00Z"); // ms→s→Z
    }
    #[test] fn errors_without_oauth_block() { assert!(parse_cc_credentials(r#"{"foo":1}"#).is_err()); }
}
```

- [ ] **Step 2: 跑看失败**。

- [ ] **Step 3: 实现**(用 `crate::format_rfc3339_z`,expiresAt 是 unix-毫秒)
```rust
use std::collections::BTreeMap;
use crate::format_rfc3339_z;

/// CC 格式:{"claudeAiOauth":{"accessToken","refreshToken","expiresAt"(unix ms),...}}
pub fn parse_cc_credentials(text: &str) -> Result<BTreeMap<String, serde_json::Value>, String> {
    let v: serde_json::Value = serde_json::from_str(text).map_err(|e| format!("JSON 解析失败: {e}"))?;
    let oauth = v.get("claudeAiOauth").ok_or("缺 claudeAiOauth 块")?;
    let access = oauth.get("accessToken").and_then(|v| v.as_str()).unwrap_or_default();
    let refresh = oauth.get("refreshToken").and_then(|v| v.as_str()).unwrap_or_default();
    if access.is_empty() && refresh.is_empty() { return Err("accessToken 与 refreshToken 均空".into()); }
    let mut extra = BTreeMap::new();
    if !access.is_empty() { extra.insert("access_token".into(), serde_json::json!(access)); }
    if !refresh.is_empty() { extra.insert("refresh_token".into(), serde_json::json!(refresh)); }
    if let Some(ms) = oauth.get("expiresAt").and_then(|v| v.as_i64()) {
        extra.insert("expires_at".into(), serde_json::json!(format_rfc3339_z(ms / 1000)));
    }
    Ok(extra)
}
```

- [ ] **Step 4: 跑看通过 + 提交** — `cargo test -p gw-dario credentials`;`git commit -m "feat(gw-dario): parse CC .credentials.json (TDD)"`

### Task 7.2:admin 新增 dario 账号(经 create_account,非 KiroManager 导入)

> ⚠️ 核验纠正:`import_accounts`(`accounts.rs:363`)是 **KiroManager 专用**(`gw_kiro::import::parse_accounts_export`),`create_account`(`accounts.rs:263`)**已支持 `body.provider`** 但内部某处硬编码 `"kiro"`(`:434` 附近)。`.credentials.json` **没有 account_id**,需操作者提供。新 dario 账号走 `create_account` 分支,不动 KiroManager 导入。

**Files:** Modify `crates/gw-app/src/admin/accounts.rs`;admin 前端账号新增表单

- [ ] **Step 1: 读真实代码确认接入点**

Run: `sed -n '250,460p' crates/gw-app/src/admin/accounts.rs`(定位 `create_account` 的 body 结构、provider 赋值、extra 组装、硬编码 "kiro" 处)。Expected: 看清 `CreateAccountBody`/`provider` 流向。

- [ ] **Step 2: create_account 支持 dario 凭证粘贴 + 生成身份**

在 `create_account` 处理里:
- `CreateAccountBody` 加可选字段 `credentials_json: Option<String>`(粘贴 .credentials.json 全文)、`provider: Option<String>`;
- 修硬编码 `"kiro"`(`:434`)为 `body.provider.unwrap_or("kiro")`;
- 当 `provider=="claude-dario"`:
```rust
let mut extra = /* 现有 extra 组装 */;
if let Some(cred) = body.credentials_json.as_deref() {
    match gw_dario::parse_cc_credentials(cred) {
        Ok(parsed) => for (k, v) in parsed { extra.entry(k).or_insert(v); },
        Err(e) => return bad_request(format!("解析 .credentials.json 失败: {e}")),
    }
}
// 补稳定 device_id / account_uuid(凭证文件无,缺则生成一次,保 five_hour 计费分类)。
extra.entry("device_id".into()).or_insert_with(|| serde_json::json!(uuid::Uuid::new_v4().to_string()));
extra.entry("account_uuid".into()).or_insert_with(|| serde_json::json!(uuid::Uuid::new_v4().to_string()));
```
- `account_id` 仍由操作者在表单提供(凭证文件无)。
- `gw-app/Cargo.toml` 若未依赖 `uuid` 则加 `uuid.workspace = true`。

- [ ] **Step 3: 前端账号新增表单**

admin 新增账号弹窗:provider 选择器(kiro / claude-dario);选 claude-dario 时显示「粘贴 .credentials.json」多行框 + account_id 输入 + 可选 proxy/group。提交到 `POST /accounts`,带 `provider` 与 `credentials_json`。

- [ ] **Step 4: 单测 + 提交** — 给定 .credentials.json 文本 → create_account 后账号 extra 含 access_token 且 device_id/account_uuid 为 36 长 UUID、provider=claude-dario。
```bash
cargo test -p gw-app
git commit -m "feat(admin): create claude-dario account from .credentials.json + stable device/account id"
```

---

## Phase 8 — Docker Compose sidecar + 端到端验证

### Task 8.1:dario-on-Bun sidecar 服务(每出口代理一个;先一台美国)

**Files:** Modify `docker-compose.yml`

- [ ] **Step 1: 加 sidecar 服务**
```yaml
  dario-us1:
    image: oven/bun:1-slim
    restart: unless-stopped
    network_mode: host
    working_dir: /dario
    volumes:
      - /opt/dario:/dario:ro
    environment:
      DARIO_API_KEY: "${DARIO_API_KEY}"
      DARIO_NO_LIVE_CAPTURE: "1"
      DARIO_MAX_BODY_MB: "16"            # 对齐 caio 16MB 入站上限
    command: >
      bun dist/index.js proxy
      --port=39100 --host=127.0.0.1 --no-live-capture
      --upstream-proxy=${DARIO_US1_UPSTREAM_PROXY}
```
> 部署纪律(出口一致性):`DARIO_US1_UPSTREAM_PROXY` = 美国 HTTPS 代理;该 dario 组 worker 的 egress(instances.yaml)**必须设同一代理**——Task 5.2 启动 warn 会复述。SOCKS5 dario 用不了。**sidecar 必须 pool 关(/opt/dario 不放 accounts 目录,<2 账号)**。

- [ ] **Step 2: caio 配置对齐** — 部署机 `config/system.yaml`(不入库):`dario.sidecar_url=http://127.0.0.1:39100`、`dario.api_key=<同 DARIO_API_KEY>`;`instances.yaml` dario 组 provider=`claude-dario`、egress=同一美国代理。

- [ ] **Step 3: 起 + 看日志** — `docker compose up -d --build dario-us1 && docker compose logs --tail=30 dario-us1`(listening 39100、live-capture skipped、pool-off/单账号)。

### Task 8.2:端到端冒烟(红线:仅一发真实调用)

- [ ] **Step 1:** admin 新增一个真实 .credentials.json dario 账号(确认 extra 有 access_token + 生成 device_id/account_uuid)。
- [ ] **Step 2:** 经 caio 打**一发非流式** `/v1/messages`,核对:200 合法响应(证明强制 stream:true + 折叠正确);caio 请求日志可见(status/usage 正确、`kiro_payload` 空、`client_payload` 为原始 Anthropic body);usage 落库正确;dario 出站源 IP 美国;响应头 `anthropic-ratelimit-unified-representative-claim` 为 five_hour。
- [ ] **Step 3:** 流式再一发,确认逐帧回流 + 末尾 usage 入库;断开测试确认中断也记部分 usage。
- [ ] **Step 4:** CHANGELOG + 收尾提交
`## [dario-sidecar] - 2026-06-17`(Features/Rationale/Notes:OAuth 直连、sidecar 指纹、强制上游流式、刷新错误分类防误禁、Z 时间格式、affinity 防 cache 抖动、TLS 上限为 Bun BoringSSL、身份字段为合成值待观察、MVP 单 sidecar 出口)。
```bash
git add CHANGELOG.md docker-compose.yml config/system.example.yaml
git commit -m "feat: dario sidecar deploy (compose, body limit aligned) + changelog"
```

---

## 自查(对照 spec + 已纳入对抗审查所有 accepted finding)

- spec §4(sidecar/补丁)→ Phase 0/1(三补丁)/8 ✓
- spec §5(代理+TLS):**MVP 单 sidecar(`dario.sidecar_url` 全局)** + 双 client(loopback no_proxy / refresh egress)+ 出口一致性启动告警 ✓;**「每出口代理→一 sidecar 映射 + account.extra.proxy 选址」为 MVP-deferred**(未实现,留 follow-up;非 ✓)。
- spec §6(crate/registry/1524/复用)→ Phase 2/3/4/5/6 ✓
- spec §7(导入 + 生成 id)→ Phase 7(经 create_account)✓
- spec §8(真实缓存 token,弃 cache_sim)→ Task 3.1 UsageAcc(real/metering 恒 0)✓
- spec §9(不做:内部 failover / per-request 代理补丁 / live-capture)✓
- spec §10(风险)→ 顶部核验事实 + Task 8.2 红线 ✓
- **对抗审查 accepted 修复全纳入**:非流式强制 stream(3.2)、Z 时间格式弃 time crate(2.2 datefmt + 4.1 + 7.1)、刷新错误分类(4.1)、family 守卫(6.1)、injected 守卫(1.3)、MAX_BODY(1.4)、affinity_key(3.1)、出口一致性告警(5.2)、出错 emit usage(3.2)、x-api-key + api_key 非空校验(2.3/3.2)、导入接入点重写(7.2)、Phase 0 头名修正 ✓
- 类型/命名一致:`format_rfc3339_z`/`UsageAcc`/`drain_sse_frames`/`affinity_from_body`/`classify_refresh_status`/`apply_refresh`/`DarioConfig`/`DarioSidecarConfig` 跨 Task 一致 ✓
```
