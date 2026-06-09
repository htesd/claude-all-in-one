# kiro-gw 架构设计

> Kiro 账号反代网关 · 全新重写 · 多进程模型
> 状态:设计稿 v0.1 · 2026-06-04
> 取代:`kiro.rs`(26k 行单 crate,已劣化)。旧项目继续生产,新项目第二 IP 灰度,稳定后全切。

---

## 0. TL;DR

- **一个 Rust 二进制**,通过启动参数跑成两种角色:`router`(前置路由)和 `worker`(实际反代)。
- **多进程**:每个 worker 进程绑定一个**固定出口 IP** + 管理**一组固定账号**。同一个账号永远从同一个 IP 出去。
- **只做 Kiro 一个上游**,但 provider 抽象成 trait + registry,未来加别的上游只改三处(借鉴 ALLinOne)。
- **内部 IR = Anthropic Messages**(不是 OpenAI),保护 thinking 签名透传 / cache_read 计费 / eventstream 解析这些旧项目辛苦攒的资产。
- **4 个 crate**:`gw-core`(契约) / `gw-kiro`(Kiro 实现) / `gw-store`(持久化) / `gw-app`(二进制)。
- 从旧 `kiro.rs` 搬运(不重写)的血泪资产:cache_sim、converter、thinking 签名(v42)、billing(v53)、empty-response fallback(v58)、空 content 400 兜底(v41/v54)、session affinity。

---

## 1. 为什么多进程(核心设计决策)

### 1.1 动机:防关联封号

封号根因(见 memory `rewrite-recon-findings`):Kiro/AWS 风控盯 **设备指纹(machineId 嵌 UA)+ 出口 IP + TLS 指纹 + 认证流类型**。其中"24 个号挤同一个出口 IP"是强关联信号,一锅端风险高。

**单进程方案的死穴**:要在一个 async 进程里让每个账号走不同出口 IP,只能靠 `reqwest` 的 `local_address` 绑定或 per-account 代理。`local_address` 绑定要改 HTTP client 构造、对 IPv6/双栈处理复杂,且一个进程崩全员挂。

**多进程方案的优势**(你的直觉正确):
- 进程 = 隔离边界。一个 worker 绑一个出口 IP(通过 `local_address` 或独立代理),**进程内所有请求天然同 IP**,不需要 per-request 绑定。
- 一个 worker 崩,只影响它那组号,其他 worker 不受影响。
- 契合双 IPv4 灰度:worker-A 绑 `139.180.152.158`,worker-B 绑 `45.32.106.167`,各跑各的。
- 未来"一 IP 一组号"防关联:加 IP/代理就是加 worker,水平扩展。

### 1.2 进程拓扑

```
                      ┌─────────────────────────┐
   client (Claude     │      router 进程         │   --mode=router
   Code / NewAPI) ───►│  :8990 对外唯一入口      │   绑主 IP
                      │  - API key 鉴权          │
                      │  - session → worker 亲和 │
                      │  - 转发(反向代理)       │
                      └───────────┬─────────────┘
              ┌───────────────────┼───────────────────┐
              ▼                   ▼                   ▼
       ┌─────────────┐    ┌─────────────┐    ┌─────────────┐
       │ worker-0    │    │ worker-1    │    │ worker-N    │  --mode=worker
       │ :9000       │    │ :9001       │    │ :900N       │  --instance N
       │ 出口=IP_A   │    │ 出口=IP_B   │    │ 出口=代理_C │
       │ 账号组 G0   │    │ 账号组 G1   │    │ 账号组 GN   │
       │ 各自:       │    │             │    │             │
       │ token刷新   │    │             │    │             │
       │ cache_sim   │    │             │    │             │
       │ scheduler   │    │             │    │             │
       └──────┬──────┘    └──────┬──────┘    └──────┬──────┘
              └───────────────────┼───────────────────┘
                                  ▼
                       ┌─────────────────────┐
                       │   gw-store (SQLite)  │  账号/key/组/usage
                       │   WAL 模式,多进程读 │  共享只读为主
                       └─────────────────────┘
```

### 1.3 router ↔ worker 契约

- **协议**:router 与 worker 之间走 HTTP(worker 暴露和对外一样的 `/v1/messages`,只是绑在 localhost 高位端口)。简单、可独立测试、可单独 curl worker 调试。
- **路由键**:router 用 `session_id`(从 Anthropic `metadata.user_id` 提取,见 cache 锚定逻辑)选 worker。同一 session 永远命中同一 worker → 同一组号 → 缓存与 IP 都稳定。
- **账号归属是静态的**:哪些号归哪个 worker,由配置(`instances.yaml`)决定,不动态迁移。这是 [[affinity-rubber-band-v52]] 橡皮筋问题的结构性根治——账号不会跨实例横跳。
- **新 session 分配**:router 按 worker 当前负载(活跃 session 数)选最空的 worker。
- **worker 不可用**:router 标记该 worker 冷却,新 session 不再分配给它;已绑定的 session 返回错误让客户端重试(或可选 failover 到其他 worker,代价是换组号→缓存冷启动)。

### 1.4 单机部署形态(你的双 IP 场景)

```
vultr 单机:
  router   进程  绑 139.180.152.158:8990  (对外,nginx/直接暴露)
  worker-0 进程  出口 139.180.152.158      账号 1-12
  worker-1 进程  出口 45.32.106.167        账号 13-24
```

由 systemd 或 docker-compose 起 3 个进程(1 router + 2 worker),同一个二进制 + 不同 `--mode/--instance`。灰度期:旧 kiro.rs 继续在主 IP:38990 服务,新 worker-1 绑第二 IP 试跑,互不干扰。

---

## 2. Crate 划分(4 个,不照搬 static_flow 的 16 个)

```
kiro-gw/
├── Cargo.toml              # workspace,统一依赖版本
├── crates/
│   ├── gw-core/            # 契约层:纯类型 + trait,无 I/O
│   │   src/
│   │     provider.rs       #   Provider trait, ChatRequest, ChatStream, CallCtx
│   │     model.rs          #   ModelInfo, MachineIdentity
│   │     error.rs          #   UpstreamError { kind, retryable_pre_stream, ... }
│   │     routing.rs        #   RoutingContext, session/cache key 派生
│   │     account.rs        #   Account, FieldSpec(驱动表单+YAML)
│   │     store.rs          #   ControlStore / UsageSink trait(抽象,实现在 gw-store)
│   │     config.rs         #   配置 DTO(instances/accounts/models/system)
│   │
│   ├── gw-kiro/            # Kiro provider:实现 gw-core::Provider
│   │   src/
│   │     lib.rs            #   KiroProvider, 注册入口
│   │     wire/             #   AWS eventstream 帧解析(frame/header/crc/decoder)★搬运
│   │     converter/        #   normalize / validate / identity / tools / session ★搬运+强化
│   │     cache_sim/        #   prefix 模拟 + conversationId 锚定 ★搬运
│   │     stream/           #   SSE 重写, thinking 签名透传(v42), empty fallback(v58)★搬运
│   │     token.rs          #   token 刷新(走本进程出口 IP/代理)
│   │     machine_id.rs     #   machineId 派生 ★搬运
│   │     fingerprint.rs    #   多字段设备画像(借鉴 xkiro,这次真接线)
│   │     billing.rs        #   cache_read 计费模拟(v53)★搬运
│   │     scheduler.rs      #   组内账号调度(affinity/lease/health)
│   │
│   ├── gw-store/           # 持久化:SQLite(WAL) 控制面 + usage
│   │   src/
│   │     control.rs        #   账号/key/组 CRUD,实现 gw-core::ControlStore
│   │     usage.rs          #   usage 记录,实现 UsageSink
│   │     request_cache.rs  #   报文存档(可选,gated)
│   │     migrations.rs     #   schema 版本
│   │
│   └── gw-app/             # 二进制:进程角色 + HTTP + 装配
│       src/
│         main.rs           #   --mode router|worker, --instance N 解析
│         router/           #   router 角色:鉴权/session→worker/转发
│         worker/           #   worker 角色:axum /v1/messages,调 provider
│         admin/            #   管理 API + 静态前端
│         registry.rs       #   provider 名 → 工厂函数(加 provider 改这里)
│         egress.rs         #   出口绑定:local_address / 代理(per-worker)
```

依赖方向(铁律):
```
gw-core  ←  gw-kiro
gw-core  ←  gw-store
gw-core + gw-kiro + gw-store  ←  gw-app
```

**禁止**(防止重新劣化为旧 kiro.rs):
- `gw-store` 反向依赖 `gw-app`
- `gw-kiro` import `axum` / HTTP 框架类型
- `gw-app` 的 handler 直接操作 token 刷新 / cache_sim 内部状态
- 任何 `stream` 逻辑直接访问 DB
- 单文件超 800 行(旧项目 token_manager 3500 行是反面教材)

### 2.1 为什么是 4 个不是 16 个

static_flow 拆 16 crate 是因为它是多 provider(kiro/codex)+ 多存储(postgres/duckdb)+ 全栈商业产品 + 团队协作。你是单人 + 单 provider + 单机。4 个 crate 拿到 90% 的边界收益(编译期强制分层、依赖不能乱指),不背 16 crate 的协调成本。未来真要加第二个 provider,再从 `gw-kiro` 模式复制出 `gw-codex` 即可。

---

## 3. Provider trait(ALLinOne 的"扩展一处"缝)

```rust
// gw-core/src/provider.rs
#[async_trait]
pub trait Provider: Send + Sync {
    /// 家族标识,如 "kiro"
    fn family(&self) -> &'static str;

    /// 账号字段定义:驱动 admin 前端表单 + accounts.yaml schema
    fn account_schema(&self) -> &'static [FieldSpec];

    /// 列出该 provider 支持的模型(catalog 用)
    async fn list_models(&self) -> Result<Vec<ModelInfo>, UpstreamError>;

    /// 核心:吃 Anthropic-native 请求,吐 SSE 事件流
    async fn chat(&self, req: ChatRequest, ctx: &CallCtx)
        -> Result<ChatStream, UpstreamError>;

    /// token/凭据刷新(实现内部决定走不走代理——但同进程出口已固定)
    async fn refresh_auth(&self, acct: &Account)
        -> Result<Account, UpstreamError>;

    /// 设备指纹(machineId/UA/...),防封一致性预留
    fn machine_identity(&self, acct: &Account) -> MachineIdentity;
}

pub type ChatStream =
    Pin<Box<dyn Stream<Item = Result<SseEvent, UpstreamError>> + Send>>;
```

### 3.1 关键决策:内部 IR = Anthropic Messages(不抄 ALLinOne 的 OpenAI)

ALLinOne 内部 IR 是 OpenAI chunk,对外用 anthropic_adapter 转回。**我们不这么做**,理由:

| | ALLinOne(OpenAI IR) | kiro-gw(Anthropic IR) |
|---|---|---|
| 客户端 | 多种,OpenAI 为主 | Claude Code(Anthropic) |
| 主上游 | 多个免费 LLM | Kiro(Anthropic 家族) |
| 主链路转换 | 双向转换 | **近乎零转换** |
| thinking 签名 | 转换中丢失 | **无损透传(保住 v42)** |
| cache_read 计费 | 难注入 | **原生保留(保住 v53)** |

主链路 `Claude Code (Anthropic) → kiro-gw → Kiro (Anthropic)` 全程 Anthropic,不脱衣服。未来若加 OpenAI 客户端入口或 Codex 上游,**在边界做适配**(入口 adapter 或 provider 内部转换),不污染主链路。这是唯一一处刻意偏离 ALLinOne 的设计,目的是保护旧项目最值钱的资产。

### 3.2 新增 provider 仍只改三处

1. `crates/gw-<name>/` —— 实现 `Provider` trait
2. `gw-app/src/registry.rs` —— 注册一行 `reg.add("name", NameProvider::from_config)`
3. `accounts.yaml` —— 加该 provider 的账号池

Registry 用编译期静态函数表(不用 Python 的动态发现):
```rust
type ProviderFactory = fn(&ProviderConfig) -> anyhow::Result<Arc<dyn Provider>>;
pub struct Registry { map: HashMap<&'static str, ProviderFactory> }
```

<!-- PART2_END -->

---

## 4. 请求数据流(完整时序)

```
1. client → router :8990   POST /v1/messages (Anthropic body)
2. router: API key 鉴权(查 gw-store)
3. router: 从 metadata.user_id 提 session_id → 派生 RoutingContext
4. router: session_id 查亲和表
     命中 → 既有 worker
     未命中 → 选活跃 session 最少的 worker,写亲和表
5. router → worker :900N   转发原始 body(加内部头 X-Gw-Session)
6. worker: 选组内账号(scheduler: session affinity → cache affinity → 健康+并发)
7. worker: converter 归一化(normalize/validate/identity)→ Kiro ConversationState
8. worker: cache_sim 锚定 conversationId + 算 prefix 命中
9. worker → Kiro 上游   (出口 IP = 本 worker 绑定的 IP/代理)
        UA 带 machineId(本账号固定),thinking 签名透传
10. worker: 解析 eventstream → SSE,注入 cache_read 计费,empty fallback 兜底
11. worker → router → client   流式 SSE
12. worker: 成功后回写 cache_sim(resume anchor)、usage、健康状态
```

关键点:
- **第 4 步的 worker 选择是 session 级、稳定的**——同 session 永远同 worker。
- **第 6 步的账号选择是 worker 组内的**——账号永不跨 worker,从结构上消除橡皮筋([[affinity-rubber-band-v52]])。
- **第 9 步出口 IP 由进程决定**,不是 per-request——同账号同 worker 同 IP,激活/发包/刷新一致(防封核心)。

---

## 5. 配置(配置驱动,借鉴 ALLinOne 的 YAML 形态)

```yaml
# instances.yaml —— 进程拓扑(新增,多进程核心)
router:
  listen: "0.0.0.0:8990"
workers:
  - instance: 0
    listen: "127.0.0.1:9000"
    egress: { mode: local_ip, address: "139.180.152.158" }
    account_group: "G0"
  - instance: 1
    listen: "127.0.0.1:9001"
    egress: { mode: local_ip, address: "45.32.106.167" }
    account_group: "G1"
    # 或 egress: { mode: proxy, url: "socks5://...", username: ..., password: ... }

# accounts.yaml —— 账号(按组分配到 worker)
groups:
  G0:
    provider: kiro
    accounts: [ {account_id: k1, refresh_token: ...}, ... ]   # 1-12
  G1:
    provider: kiro
    accounts: [ ... ]                                          # 13-24

# system.yaml —— 运行开关(沿用旧项目热调参数)
cache:
  read_multiplier: 1.0
  cap_ratio: 0.9
  floor_ratio: 0.1
empty_response:
  buffered_fallback: true        # v58
```

`egress` 是新架构最关键的新增配置:每个 worker 显式声明出口。`local_ip` 模式用 reqwest `local_address` 绑定本机 IP(单机双 IPv4);`proxy` 模式走外部代理(未来一 IP 一组号 / 固定 IP 代理池)。

---

## 6. 从旧 kiro.rs 迁移清单(搬运,非重写)

血泪资产,逐个搬进新 crate 边界,**不重写逻辑、只调整封装**:

| 旧位置 | 新位置 | 资产 | memory |
|---|---|---|---|
| `kiro/` eventstream 解析 | `gw-kiro/wire/` | AWS 帧/CRC/protobuf 解码 | — |
| `anthropic/` converter | `gw-kiro/converter/` | 归一化/校验/身份 | [[empty-content-400-poisoning]] |
| `kiro/cache_sim` | `gw-kiro/cache_sim/` | prefix + conversationId 锚定 | [[real-cache-hit-research]] |
| thinking 签名透传 | `gw-kiro/stream/` | v42 protobuf 签名 | [[thinking-signature-passthrough]] |
| billing 模拟 | `gw-kiro/billing.rs` | v53 统一模拟器 | [[cache-billing-v53-unified-sim]] |
| empty fallback | `gw-kiro/stream/` | v58 buffered fallback | [[empty-response-transient-retry]] |
| 空 content 兜底 | `gw-kiro/converter/` | v41/v54 三/五处兜底 | [[media-without-text-400]] |
| session affinity | `gw-kiro/scheduler.rs` + router | v52 落号即认 | [[session-affinity-scheduler]] |
| machine_id 派生 | `gw-kiro/machine_id.rs` | sha256 派生(已对齐上游) | — |

迁移原则:**先让新 crate 编译通过 + 测试搬运过来一起绿**,再考虑强化。强化项(可选,迁移后):converter 三级 system 分流、tool 生态兼容(orphan prune / duplicate id rewrite)、fingerprint 真接线、prompt_filter。

---

## 7. 分阶段实施路线

**Phase 0 — 骨架**:workspace + 4 crate 空壳 + Provider trait + 进程角色解析(`--mode`)。能 `cargo build`,router/worker 能启动握手。

**Phase 1 — 单 worker 直通**:gw-kiro 搬运 wire + converter + token,实现 Provider::chat 最小可用。单 worker 能反代一个真实 Kiro 请求(非流式)。

**Phase 2 — 流式 + 资产搬运**:搬 stream/cache_sim/billing/thinking/empty-fallback,流式打通,旧测试搬过来绿。

**Phase 3 — 多进程**:router 角色 + session→worker 亲和 + egress 绑定。双 worker 双 IP 跑通。

**Phase 4 — 存储 + admin**:gw-store SQLite + admin API + 前端(参考旧 kiro.rs UI)。

**Phase 5 — 灰度**:第二 IP 部署 worker,小流量验证(尤其防封:同号同 IP 激活/发包)。稳定后全切。

每个 Phase 结束走对抗审查(adversarial-review),与旧项目同口径验证。

---

## 8. 风险与已知权衡

- **多进程多一跳延迟**:router→worker 走 localhost HTTP,增加一次内网往返(<1ms)。可接受;换来隔离与 IP 固定。
- **SQLite 多进程写竞争**:WAL 模式 + 控制面写少读多。usage 写可走每 worker 本地 journal 再汇总(借鉴 static_flow),Phase 4 再定。
- **router 是单点**:router 崩则全挂。但 router 极薄(只鉴权+转发),崩溃面小;可加 systemd 自动重启。
- **failover 缓存代价**:worker 挂后 session 转移到别的 worker = 换组号 = 缓存冷启动 + 可能触发 IP 漂移风控。默认不自动 failover,返回错误让客户端重试同 worker(等其恢复);是否 failover 作为配置开关。
- **egress local_ip 的坑**:reqwest `local_address` 绑定需确认对 Kiro 上游(纯 IPv4,见侦察)生效;IPv6 段对 Kiro 无用(上游无 AAAA)。Phase 3 实测。

---

## 9. 待确认 / 开放问题

- router↔worker 用 HTTP 还是 Unix socket?(倾向 HTTP,可调试)
- usage 跨进程汇总:每 worker 写本地后台汇总,还是都写同一 SQLite?(Phase 4 定)
- admin 面板:router 统一展示全 worker 状态,还是每 worker 自己一个?(倾向 router 聚合)
- 账号分组策略:手动配 vs 按某规则自动均分到 worker?(先手动)

