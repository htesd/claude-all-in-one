# 设计稿:caio 集成 dario(Claude OAuth sidecar 引擎)

- 日期:2026-06-17
- 状态:设计已与用户对齐,待落实施计划(writing-plans)
- 背景关联:`MEMORY.md` 中 [[anti-ban-ambient-and-toolwire-research]]、[[kiro-gw-new-project]]、[[caio-us-egress-proxy-vultr]]、[[kiro-anti-ban-wire]]

## 1. 目标与背景

Kiro 上游频繁封号(2026-06 一次性封掉 4 个号含付费客户号,判断为 Kiro 侧风控、非反代问题)。决定**新增一条非 Kiro 的 Claude 上游**:用 Claude Pro/Max 订阅的 OAuth 凭证直连 `api.anthropic.com`,把成熟的本地反代 **dario** 当作"报文指纹 + TLS"引擎复用。

**核心定位**:dario 为主、Kiro 降为备用。但"故障转移交给前置 NewAPI"——caio 只把 dario 暴露成一个独立分组/channel,与现有 Kiro 分组并列;NewAPI 在前面按优先级路由(dario 优先,挂了落 Kiro 或别的 channel)。**caio 内部不做跨 provider 自动 failover**。

## 2. 关键决策(已与用户确认)

| # | 决策 | 取舍 |
|---|------|------|
| D1 | dario 为主,Kiro 备用 | 赌 Claude OAuth 订阅号比 Kiro 耐封 |
| D2 | 备用 = 独立分组 + NewAPI 优先级路由 | caio 热路径零改动;不在 caio 内合流两套计费/调度 |
| D3 | 账号形态 = CC `.credentials.json`(内含 OAuth token) | 导入解析即用,caio 端持有 refresh_token 做刷新 |
| D4 | 架构 = dario 当 sidecar 引擎(方案 A) | 继承 dario 全套防封含 TLS,不在 Rust 重写;最快验证赌注 |
| D5 | TLS = dario 跑在 Bun 上(BoringSSL) | 最逼近 CC;优于 caio 用 Rust rustls(第三种指纹) |
| D6 | 代理 = 每出口代理一个 dario 实例,caio 是分配事实源 | 进程级隔离,互不影响;dario 只支持 HTTP/HTTPS 全局代理 |

## 3. 整体拓扑

```
NewAPI(前置,按优先级路由)
   ├─ caio dario channel(优先)
   │     └─ caio worker(dario 组) ─ DarioProvider
   │            └HTTP(loopback)→ dario sidecar #k (Bun, 绑代理k) ─[HTTPS CONNECT 代理k]→ api.anthropic.com
   └─ caio kiro channel(备用)
         └─ caio worker(kiro 组) ─ KiroProvider ─→ Kiro 上游
```

- dario 与 Kiro = caio 内两个并列 provider 分组。
- dario sidecar 实例数 = caio 出口代理数(非账号数);账号按其分配代理映射到对应实例。

## 4. dario sidecar 形态与补丁

### 4.1 运行形态
- 命令:`bun run <dario>/dist/cli.js proxy --port=<N> --host=127.0.0.1 --no-live-capture`
- 必须 **Bun**(拿 BoringSSL TLS);`--no-live-capture` 用 bundled 快照(`dist/cc-template-data.json`),**不需要本机 claude 二进制**。
- pool 关闭(不放 `~/.dario/accounts/`)→ dario 不自己选账号,自动旁路 429/auth failover 循环。
- 设 `DARIO_API_KEY` 保护监听口(caio 调用时带上)。
- 每实例 `--upstream-proxy=<对应 HTTPS 代理>` 固定一个出口。
- 以 docker compose 服务常驻(每出口代理一个服务)。

### 4.2 需要给 dario 打的补丁(本地 fork,规模小,集中在 `proxy.ts`)
1. **per-request bearer 注入**(`proxy.ts` 账号选择块 ~1467-1484):新增分支读请求头 `x-dario-upstream-token` → 作上游 OAuth bearer、`poolAccount=null`、跳过 pool/getAccessToken。`upstreamAuthHeaders()` 已能用 accessToken 拼 `Authorization: Bearer`,无需改。
2. **per-account 身份头**(`proxy.ts` bodyIdentity 构造 ~1686-1688):新增读 `x-dario-device-id` / `x-dario-account-uuid` → 透到 `buildCCRequest` 的 identity,写入出站 `metadata.user_id`。

> 不打补丁的部分(保留):`buildCCRequest` 报文重建、静态头+billing tag+session、`orderHeadersForOutbound` 头排序、`fetch(targetBase)` + SSE 透传。

### 4.3 为什么必须打 1+2
- dario 现状只从自己 pool/env/keychain 取上游 token,无 caller 注入口(`ANTHROPIC_UPSTREAM_API_KEY` 仅全局 env 且走 x-api-key 非 bearer)。
- `.credentials.json` **不含** device_id/account_uuid;dario 缺失时发空串,**会被 Anthropic 当三方流量按 overage 计费(而非 five_hour 订阅池)**。故 caio 必须 per-account 生成稳定值并下发。

## 5. 出口代理 + TLS(锁死)

- **caio 是代理分配唯一事实源**:`account.extra.proxy` > `default_proxy` > `egress_pool`(沿用现有 EgressResolver 语义)。
- **每个出口代理 → 一个 dario-on-Bun 实例**(独立端口/独立 DARIO_API_KEY/独立 `--upstream-proxy`)。进程级隔离 = 互不影响。
- caio 配置维护 `出口代理URL → dario sidecar URL` 映射;`DarioProvider` 按账号分配 proxy 查表,转发到对应实例。
- **TLS 端到端**:dario 经 HTTPS CONNECT 代理连 Anthropic,TLS 握手仍是 dario(Bun BoringSSL),代理仅转字节。
- **约束**:
  - dario 只支持 HTTP/HTTPS 代理,**不支持 SOCKS5**。美国 tinyproxy(HTTPS)可用;microsocks(SOCKS5)不可直用(需 HTTP 前端或不分配给 dario 账号)。
  - 无代理账号 → 设一个 "direct" dario 实例兜底,或强制所有 dario 账号分配一个 HTTPS 代理(推荐后者,保 geo)。
  - 代理增删 = 加/删一个 compose 服务 + 一行映射(静态;内存够,实例轻)。
- **MVP 简化**:可先起 1 个美国 sidecar 给所有 dario 号;要 per-account IP 粘滞再按代理扩实例。

## 6. caio 侧改动

### 6.1 新建 crate `crates/gw-dario`
- `DarioProvider impl Provider`:
  - `family() = "claude-dario"`
  - `account_schema()`:`access_token` / `refresh_token` / `expires_at` / `device_id` / `account_uuid` / `proxy`
  - `validate_account()`:access_token 或 refresh_token 非空
  - `list_models()`:Anthropic 模型静态表
  - `chat()`:把 `req.body` + 头(`x-dario-upstream-token`=access_token、`x-dario-device-id`、`x-dario-account-uuid`、`x-claude-code-session-id`/session)POST 给"账号代理对应的 dario sidecar";解析回传 SSE → `StreamItem::Sse`,末尾 `message_delta` 解析 usage → `StreamItem::Usage`。
  - `refresh_auth()`:Claude OAuth `refresh_token` 流程(`grant_type=refresh_token`,`Content-Type: application/x-www-form-urlencoded`),返回带新 access_token/expires_at 的 Account。
  - `affinity_key()`:可从 body 派生稳定会话键(利于 Anthropic prompt cache 同号命中);MVP 可先 `None`。
  - `account_quota()`/`discover_profile_arn()` → 默认(None)。

### 6.2 接线 / 改共享代码
- workspace `Cargo.toml` 加 `crates/gw-dario`;`gw-app/Cargo.toml` 依赖之。
- `gw-app/src/registry.rs` 加一行:`reg.register("claude-dario", gw_dario::DarioProvider::from_config);`
- **修 `gw-app/src/worker/mod.rs:1524`**:`render_kiro_payload` 当前对所有 provider 执行;加 `if account.provider == "kiro"` 守卫,非 kiro 时 `kiro_payload` 留空串(请求日志 client_payload 仍记原始 Anthropic body)。
- (可选技术债)把 `gw-core/src/model.rs` 的 `MachineIdentity` 迁入 `gw-kiro`(此时有第二个 provider 验证它非通用);不阻塞本期。

### 6.3 直接复用的 gw-app/gw-core 设施(零改动)
- token 生命周期:`ensure_credentialed` / `refresh_locked` / CAS `refresh_after_rejection` / `do_refresh_and_persist`(全基于 `extra.access_token`/`expires_at`,provider 无关)。
- usage 落库 `finalize_usage` → `UsageSink`(SQLite);per-key 计费统计。
- 非流式折叠 `gw_core::fold::fold_sse_to_message`(标准 Anthropic SSE,provider 无关)。
- 请求日志环形落库(client_payload 记原始 Anthropic body)。
- 换号重试硬上限 / 防雪崩;会话亲和调度;账号 30s DB 同步;admin 导入验活;配额缓存框架。

## 7. 账号导入 + 身份字段

- 导入 `.credentials.json`:解析 `claudeAiOauth.{accessToken, refreshToken, expiresAt}` → 写入 `account.extra` 的 `access_token` / `refresh_token` / `expires_at`(RFC3339 "Z")。
- **导入时为每个号生成并存库稳定的 `device_id` / `account_uuid`**(UUID),填补凭证文件缺口,保住 five_hour 订阅计费分类。后续每次调用由 caio 下发,dario 不生成、不持久化。
- session_id:caio 端按会话生成稳定 UUID(随账号/会话),与 `x-claude-code-session-id` 一致下发。

## 8. 计费 / usage

- 走 Anthropic 真实回报的 `input_tokens`(message_start)/ `output_tokens`(message_delta)/ `cache_read_input_tokens` / `cache_creation_input_tokens`。
- **不用 Kiro 的 cache_sim**(那是 Kiro 不报真实缓存的补偿);dario 路径直接用真实缓存 token,更准更简单。
- `ChatUsage` 的 `real_cache_read_tokens` / `metering_credit`(Kiro 专属诊断字段)对 dario 恒置默认。

## 9. 明确不做(YAGNI)

- caio 内部跨 provider 自动 failover(交前置 NewAPI 优先级路由)。
- dario per-request 代理补丁(用"每代理一实例"代替,进程隔离更稳)。
- live-capture(用 bundled 快照;若快照随 CC 升级过时,再单独跑一次 live-capture 刷新快照)。
- 在 Rust 里重写 dario 指纹套件(方案 B,留作未来可选,不在本期)。

## 10. 主要风险 / 待验证

1. **核心赌注**:Claude OAuth 订阅号在我们这种池化高频用法下到底耐不耐封——上线后用真实流量(只读配额 + 少量真实客户调用)观察,**不做主动 chat 压测(封号红线)**。
2. **TLS 上限**:sidecar(Bun)只能逼近 CC 的 BoringSSL,非 100% CC TLS(只有 shim 模式才是);先接受,观察是否够。
3. **身份字段**:device_id/account_uuid 为 caio 生成的合成值(非真实 CC 设备),对 five_hour 分类是否足够,需上线观察 `representative-claim` 响应头。
4. **bundled 快照新鲜度**:billing seed / beta flag / body 字段序随 CC 演进会变;dario 快照过时则指纹失真,需定期 live-capture 刷新机制。
5. **SOCKS5 出口**:现有 Singapore SOCKS5 dario 用不了,dario 账号出口先限 HTTPS(美国 tinyproxy)。

## 11. 验证

- 单测:`gw-dario`(refresh_auth 解析 token 端点响应、chat 把 SSE 折成 StreamItem、账号→sidecar 映射选址);`gw-app`(registry 注册 claude-dario、worker:1524 非 kiro 留空串)。
- 集成:起一个 dario-on-Bun sidecar(--no-live-capture + pool off + DARIO_API_KEY),caio 用一个真实 `.credentials.json` 走通一次 `/v1/messages`,核对:出站经指定代理(出口 IP 正确)、`metadata.user_id` 非空、响应 `representative-claim` 是否 five_hour、usage 落库正确。
- 红线:只读配额 + 极少量真实调用验证,**不主动 chat 压测**。
