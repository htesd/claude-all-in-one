# 架构重构前代码审查（2026-09-10）

## Intent

用户要求开始架构重构，先制定计划并审查代码。目标是修复重复执行、凭据一致性和重复恢复编排，
同时保留原设计中的进程隔离、固定出口、会话亲和、Anthropic IR 和 provider 边界。

## Verdict: REJECT（原草案及现存高风险行为）

三个 reviewer 对 box 提交未知仍可重放达成共识；两个 reviewer 独立确认管理端也会覆盖新凭据。
这些是进入重构实施时必须先处理的正确性问题。结论不代表需要推翻主体架构。

已据此修订 [实施计划](architecture-refactor-plan-20260910.md)。修订版由主审收敛，
尚未实施、运行新回归用例或进行第二轮独立审查。

## 审查方法与证据

- 对照 `docs/ARCHITECTURE.md`，读取当前工作区、已跟踪差异及尚未提交的 box 模块。
- 实际运行三个 `codex exec --ephemeral --sandbox read-only`，分别采用 Skeptic、Architect、
  Minimalist 视角；均退出码 0，确认三个输出文件存在且非空后才合成结论。
- 原始报告：[Skeptic](evidence/architecture-refactor-20260910/skeptic.md)、
  [Architect](evidence/architecture-refactor-20260910/architect.md)、
  [Minimalist](evidence/architecture-refactor-20260910/minimalist.md)。
- 本轮是静态审查及文档校验，没有访问生产或凭据、没有故意制造线上故障，没有修改业务代码。
  以下失败序列是由调用链支持的可构造情形，不表示本轮已经运行复现。
- 上一轮 1,744 项测试及真实客户端验收属于历史基准；不能证明下面新增故障场景已覆盖。

## Findings（按风险排序）

### 1. [high] 两层都把送达未知当作可重发

- router：`crates/gw-app/src/router/mod.rs:566` 对所有 `send()` 错误进入 failover。
  A 已收到完整 POST，但响应头前断开，router 就可能向 B 重发。
- box：`crates/gw-cursor/src/sandchat/box_gateway.rs:614` 发出 `sendPrompt`，确认丢失后
  经 readiness 返回 `ServerError`，`crates/gw-app/src/worker/mod.rs:3229` 又允许换账号。
  interrupt/delete 成功与否均不能证明此前没有执行。
- Lens：Skeptic、Architect、Minimalist；原则：prove-it-works、boundary-discipline。
- 建议：R1a 修 router；R1b 提前补 provider 送达事实与 worker 禁止重放判断。客户端是否
  收到响应与上游是否执行分别记录，不能靠错误类别或 readiness 推断。

### 2. [high] 管理端的增量编辑实际回写完整旧快照

- `crates/gw-app/src/admin/accounts.rs:1620` 先读取 extra，`:1641` 在 handler 内合并，
  `:1699` 调用整体更新；`crates/gw-store/src/lib.rs:800` 最终使用 `extra = ?`。
- admin 读 T0 → worker 落库 T1 → admin 只改 driver 却提交含 T0 的整块 extra，新凭据回退。
  脱敏哨兵提前还原旧值也存在相同窗口，不需要发生 DB 写失败。
- Lens：Skeptic、Architect；原则：serialize-shared-state-mutations、boundary-discipline。
- 建议：将 patch、哨兵保留和替换守卫置于 Store 原子操作内；保留显式凭据替换语义。
  现有 merge 已有事务，问题不是笼统缺少事务，而是写入载荷和新旧判定错误。

### 3. [high] 账号级脏标记不能代表多个待写增量

- `crates/gw-app/src/worker/mod.rs:1579` 重试写完整 `acc.extra`，可以覆盖管理员的新字段。
  `:1645` 只写当前 delta，`:1648` 却清除整个账号脏标记。
- token T1 写失败 → flush 仍失败 → agent id 单独写成功 → 整体清脏 → sync 用 DB 的 T0
  覆盖内存。该序列可以完全串行发生。
- `crates/gw-cursor/src/lib.rs:2545` 又把普通更新合进 token delta；worker 对同值/旧 token
  的 `continue` 会连带丢掉普通字段。
- Lens：三个 reviewer；原则：fix-root-causes、serialize-shared-state-mutations。
- 建议：保存待写字段增量、按实际写入范围确认，分别处理认证与普通更新，补反向清脏测试。

### 4. [high] 成功落库也挡不住旧 sync 快照覆盖内存

- `crates/gw-app/src/worker/mod.rs:753` 读取账号快照，`:770` 发布；
  `crates/gw-app/src/worker/scheduler.rs:2727` 只看 dirty，`:2740` 替换账号。
- sync 读取 T0 → 刷新线程写入 T1 且成功清脏 → sync 恢复发布 T0。进程内 sync 锁只串行
  同步任务，不互斥刷新线程；在多线程运行时无需中间存在显式 await 即可交错。
- Lens：Skeptic；原则：serialize-shared-state-mutations。
- 建议：同步发布加入版本准入或保留较新本地更新的规则；用屏障测试上述全成功序列。

### 5. [high] 孪生记录的身份语义冲突

- `crates/gw-app/src/admin/accounts.rs:1329` 因不同 `client_id` 判定另一个 IdC 注册，并写
  `twin_of`；`crates/gw-app/src/worker/mod.rs:365` 恢复时不核对注册身份，直接复制源 token。
- 失败时可能拿 A 的 token 配 B 的 client ID/secret/区域发起刷新。可以确认身份材料被混搭，
  但没有证据证明已造成生产跨账号访问或具体封号。
- Lens：Architect；原则：boundary-discipline、foundational-thinking。
- 建议：导入冲突派生关系不能当作共享认证链授权；在 R2 加身份守卫，明确合法共享链的
  单一刷新所有者。不能用同邮箱推断 token 可交换。

### 6. [high] 非流式已有产出后仍可能重试；请求状态又在分支入口重置

- `crates/gw-app/src/worker/mod.rs:4166` 已收集正文/Usage，`:4195` 仍按 TokenInvalid
  再次推理。此时客户端未收到响应，不等于上游未执行；递归还会丢弃此前收集的用量。
- `:4101`、`:4120` 将恢复标记重置 false，`:4207`、`:4655` RPM 准入传 `None`，丢失
  原成员视图，可能多一次推理或错误豁免暖机限制。
- Lens：Skeptic、Minimalist；Architect 的送达契约发现亦相关。
  原则：fix-root-causes、prove-it-works、subtract-before-you-add。
- 建议：先修状态传递与已有产出边界，再提取小范围共用函数；写清每类故障的允许次数。
  普通流首帧错误不因此新增换号，profileArn/过载仍使用各自预算。

### 7. [high，计划约束] 旧写入方共存会破坏新一致性保证

- 旧 router 的 admin 整块写和旧 worker 的完整 extra flush 不会遵守新版条件更新。
  两个 worker 的 OAuth 刷新也不受同一把进程内锁保护；事后 CAS 无法撤销两次轮换。
- Lens：Architect、Skeptic；原则：boundary-discipline、serialize-shared-state-mutations。
- 建议：R2 覆盖所有相关 DB 写者，暂停相关管理修改、排空或停止旧 owner 后交接。
  不重叠持有同一账号；回滚镜像不能回滚 token，也不能未经检查恢复旧的不安全写入。

### 8. [medium] provider 更新队列与停机收尾存在断点

- `crates/gw-cursor/src/clidrv.rs:262` 对同号观测直接覆盖，迟到旧观测可能替换未消费的新值。
  `crates/gw-app/src/worker/mod.rs:1485` 停机只 flush scheduler，没有最后收取 provider 队列。
- Lens：Architect；原则：serialize-shared-state-mutations、boundary-discipline。
- 建议：生产方保留新旧依据，停止并等待生产方后最后收队列、再落盘。文件副本可能帮助
  后续恢复，但不能视为 DB 已经安全排空；不必为此建设通用任务框架。

### 9. [medium] Temporal 当前可以自动进入，不能视为未启用

- `crates/gw-cursor/src/sandchat.rs:1371` 成功取得 agent 就选 Temporal；门控 TTL 会到期。
  `:1382` 在 run gate `:1413` 之前决定续接，lookup `:274` 未比较 system/tools head。
- Lens：Minimalist、Architect；原则：prove-it-works、foundational-thinking。
- 建议：首批增加独立小修复，锁内重判且比较 head；并发与工具 schema 变更分别验证。
  修正上一轮报告“启用前再检查”可能造成的误解，不用历史 box 成功假设该路径不可达。

## What Went Well

- provider 独立 crate、Anthropic IR 和 router/worker 分工仍适合现有业务，保留主体结构。
- 固定出口、单飞锁、增量 Store API、流首项窥探及 box 会话隔离均有可保留的实现基础。
- 已有真实 CC/OpenCode 工具和附件验收，可以作为重构后需重新验证的功能基准。

## Lead Judgment

1. 采纳 #1–#6：主审核对了具体调用链，属于正确性问题，排在纯模块拆分之前。
2. 采纳 #8：生产队列和退出时序属于同一凭据生命周期，纳入 R2，范围限定于现有生产方。
3. 采纳 #9：代码证明 Temporal 可自动进入，不能继续无条件延期。
4. 采纳 #7：把它写成可执行的 owner 交接约束，不声称只做 DB 兼容测试就能安全滚动升级。
5. 收窄 404 发现：当前业务错误被 worker 映射，未证实上游 404 穿透重放；仅补明确内部
   契约与新旧兼容测试，不为此建设能力注册系统。响应头后的 router 断流原本不重放，属保行为测试。
6. 采纳 Minimalist 的收缩建议：先传递现有恢复状态，再考虑 RetryContext；R5 输出提取与
   传输去重分开，保留 HTTP/1、client 缓存键及出口差异。
7. 拒绝按文件行数机械拆分、推倒 provider 分层、新增全局 retry 框架或通用插件配置系统。
   配置归并与共享输出提取延后到正确性修复稳定之后。

最短实施顺序：R0 固定基准 → R1 两层重放 → R2 完整凭据生命周期 → R3 重试状态及
Temporal 小修复 → 独立模块提取。具体提交边界、回归序列与部署约束以实施计划为准。
