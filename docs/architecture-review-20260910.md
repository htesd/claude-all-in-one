# 对照初始设计的架构审查（2026-09-10）

## 目标与结论

对照 `docs/ARCHITECTURE.md` 的 v0.1 设计，检查多 provider 演进后哪些边界需要整理。
本次运行了三个实际 `codex exec --ephemeral --sandbox read-only` 审查：Skeptic、
Architect、Minimalist，均产出报告；主审再核对关键调用链。没有执行大范围重构。

**Verdict: REJECT（当前架构边界需要整改）**。两名 reviewer 均指出 router 的不确定送达
重放问题，主审核对后采纳。其余主要问题集中在 worker 凭据/恢复编排及 provider 配置边界。
这份结论的对象是现有整体架构；GrokBot 的功能与部署验证另见客户端验收报告。

主体分层仍然成立：router/worker 多进程、provider 独立 crate、Anthropic IR、SQLite
由 `gw-store` 管理。原设计明确允许增加 provider，因此 crate 数量增长不是需要推倒重写的理由。
大文件中包含大量内联测试，不能仅按总行数决定拆分。

## 优先顺序

| 优先级 | 要整理的边界 | 当前具体问题 | 最小改动方向 |
|---|---|---|---|
| P0 | router 的送达与重放判断 | `send()` 所有错误都会触发换 worker；对方可能已经收到 POST 并提交上游 | 将确定未连接、明确无协议入口、送达未知分开；未知状态不自动重放 |
| P1 | 凭据与账号 extra 的持久化 | 脏数据重试写回完整旧 extra；token CAS 又可能连带丢掉普通字段更新 | 独立凭据更新/普通 patch；保留待落库的字段增量，避免旧配置覆盖新配置 |
| P1 | worker 的重试上下文 | 同步错误、非流式、流式首包三套恢复逻辑传递的成员视图与重试状态不同 | 提取内部恢复协调器和重试上下文，统一准入、刷新、已尝试次数 |
| P2 | provider 运行时配置 | 启动/轮询同时调用 Provider 接口，又直接修改 Kiro/Cursor cache 和策略 | 启动与热更新共用入口，provider 内部配置归已有 `apply_hot_settings` |
| P2 | Cursor 各协议的共享能力 | sandchat/box 从 inference 借用混有协议解码的 Folder、TextEmu 和传输配置 | 在 `gw-cursor` 内提取工具文本、输出折叠、账号传输策略；保留不同协议的生命周期 |

## 采纳的具体发现

### 1. 不确定送达不能当成未送达（高）

- 证据：`crates/gw-app/src/router/mod.rs` 的 `proxy_messages`，转发循环对所有
  `Err(e)` 进入 failover；注释假设无总超时就基本只有 connect 错误。
- 与原设计 §1.3、§8 的“默认保持绑定、不自动迁移”存在偏差。
- 可构造场景：worker A 收到完整 POST、提交云端消息，然后在响应头前断开；
  router 将同一请求送到 B，导致重复云端执行和计费。无总超时不能排除此场景。
- 回归方式：本地 fake worker A 读完 POST 后断开，B 记录接收次数，断言 B 没有
  收到重放；另测连接拒绝、协议 404 和组隔离。没有向生产故意制造此故障。
- Lens：Architect、Skeptic；原则：边界纪律、验证送达事实。

### 2. extra 更新需要独立一致性规则（中）

- `worker/mod.rs::flush_dirty_extras` 把整个 `acc.extra` 再传给 `merge_account_extra`。
  首次落库失败后，管理员新改的 proxy/region 可能被旧内存快照覆盖。
- `gw-cursor/src/lib.rs::poll_token_updates` 把 `sand_agent_id` 合进 token delta；
  `worker/mod.rs::adopt_provider_token_updates` 因 token 相同或过旧而 `continue` 时，
  该普通字段也被丢掉。这个问题不能由 SQLite 事务原子性解决。
- 提取 `worker/credentials` 或等价内部模块，保存待写增量；凭据 CAS 与普通 extra
  合并分别判定。保留单飞锁、锁内重读、防旧 token 覆盖和失败重试。
- 回归场景：落库失败后管理员修改 proxy；旧/相同 token 与新 agent id 同时到达。
- Lens：Architect、Skeptic；原则：共享状态的串行更新、明确数据所有权。

### 3. 重试状态必须沿整个请求传递（中）

- `worker/mod.rs` 的同步错误路径、`collect_nonstream` 和 `stream_response` 各自处理
  token 刷新。流式/非流式支路的 RPM 重试准入传 `None`，同步支路传成员视图；
  同一账号在分组低优视图下的暖机豁免判断因路径而不同。
- 同步刷新后进入 `finish_response`，后续响应支路的 `token_retried` 又从 false 开始，
  可能多尝试一次推理。刷新冷却只能约束刷新，不能代替请求级的推理重试计数。
- 先共享一个小的 `RetryContext` 和恢复函数：成员视图、最新账号、是否已经重试、
  请求送达阶段。HTTP/SSE 编码仍由各响应路径负责，不建立通用工作流框架。
- 用同一 fake Provider 跑三条路径，比较刷新次数、推理调用次数、RPM 拦截与账号健康。
- Lens：三个 reviewer 均建议收敛；原则：减少重复编排、保留既有保护。

### 4. 配置应用应只经过一个有效入口（中）

- worker 启动和 30 秒轮询都直接修改两家 cache_sim、Cursor 模型目录/工具策略，
  同时又调用 `Provider::apply_hot_settings`。已有注释记录过漏同步 Cursor TTL 的问题。
- 先把 provider 自己的缓存和策略收回已有配置入口；worker 只做 overlay、scheduler
  更新与应用状态展示。让失败可见，防止“面板保存成功但驱动没有生效”。
- 回归检查：启动与热更新得到相同有效配置；非法配置保留旧值；不同 provider 缓存独立。
- Lens：Architect、Minimalist；原则：缩小修改面、provider 边界纪律。

### 5. 共享输出能力不应继续挂在旧协议名下（低）

- `inference::Folder` 同时承担 Anthropic 输出、TextEmu 和 InferenceService protobuf
  解码，sandchat/box 需要前两者，形成协议模块间的隐式耦合。
- 已有第二个真实使用方，适合原样提取 `text_tools` / `stream_output`，无需新增 crate。
  账号代理、固定出口和 cursorvm 重定向策略也应有集中、可验证的接线。
- Temporal 与 box 的 nonce、会话、取消、清理仍分别保留；不强行统一成一个万能驱动。
- Lens：Architect、Minimalist；原则：有真实复用需求才提取、先减少耦合再加抽象。

## 后续启用相关能力前再核对

- **Temporal 会话一致性（中）**：续接判定发生在 run gate 之前，且未比较 system/tools
  的渲染头；并发同会话或修改工具 schema 时可能使用旧基准。box 已在锁内判断并比较 head。
  应用同一组纯状态测试分别验证两个协议；当前已验证的生产路径是 box。
- **孪生借链（中，需补身份契约证据）**：`twin_resync` 复制源号 token，但没有核对
  provider、owner、IdC 注册身份。不能仅凭静态代码断言发生了跨账号授权；也不能把同邮箱
  当作凭据可交换的充分条件。先明确“别名共享链”与“不同注册身份”的边界，再做修复。
- **YAML-only 降级（设计取舍）**：DB 打不开时继续运行但不持久化滚动 token，有运维风险。
  生产可以考虑要求 SQLite 就绪；是否保留显式无 DB 开发模式需要单独决定，不能顺手删除。

## 主审取舍

采纳前三项正确性问题和后两项局部提取建议。孪生借链从 reviewer 的高风险判断降为
需要身份契约证据的中风险项；YAML-only 保留为明确的产品/运维选择。拒绝仅以“超过
800 行”或“超过四个 crate”为依据的大规模拆分，也不撤销现有多 provider 能力。

建议逐项独立提交：先给 P0/P1 场景补能复现的测试并修复，再做保行为的模块提取。
每一步保留固定出口、两级亲和、bot 池隔离和账号刷新防护，运行相关测试后再全量验证。
