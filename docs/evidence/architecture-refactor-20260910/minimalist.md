计划需要调整后再实施：先补送达边界和待写增量的最小修复，再决定提取范围。以下均为静态代码核实的可构造序列，未声称已观察到运行故障。

1. **high｜当前代码缺陷＋计划缺口：box 的“准备失败”包含送达未知，不能整体保留换号语义。**

   [box_gateway.rs:614](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-cursor/src/sandchat/box_gateway.rs:614) 提交 `sendPrompt`，收到 `accepted=true` 后才完成 readiness；请求超时、断连和响应解析失败均可能提前返回错误（[同文件:136](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-cursor/src/sandchat/box_gateway.rs:136)）。错误经 readiness 返回 `Provider::chat`，worker 按错误类别换号（[worker/mod.rs:3229](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-app/src/worker/mod.rs:3229)）。

   **失败序列：**云端收到并执行 `sendPrompt` → 确认响应丢失 → 清理尝试结束 → worker 换号，用新 nonce 再提交。取消、删除 agent 不证明此前没有执行。

   R3 的“准备失败仍可换账号”必须限定为**能证明推理未提交的失败**。当前错误契约只讨论客户端是否输出（[error.rs:7](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-core/src/error.rs:7)），worker 无法自行推断云端送达事实。最小方案是补必要的送达分类与禁止重放判断，将此项提前到 R1 后；无需先建设完整 `RetryContext`。

2. **medium｜当前代码缺陷＋验收遗漏：串行写入也会丢掉之前失败的增量。**

   刷新写失败后保留脏标记（[worker/mod.rs:334](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-app/src/worker/mod.rs:334)）；随后 provider 更新只持久化本次 `delta`，成功却清除整个账号的脏标记（[worker/mod.rs:1642](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-app/src/worker/mod.rs:1642)）。

   **失败序列：**新 token 落库失败 → 定时 flush 仍失败 → `sand_agent_id` 单独落库成功 → 整体清脏 → 同轮账号同步用数据库旧 token 覆盖内存。同步确实依次执行 flush、provider 更新、账号重载（[worker/mod.rs:742](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-app/src/worker/mod.rs:742)）。

   R2 当前强调“旧写成功清掉后来更新”，还应补这个相反方向的用例。先用待写字段集合保证“成功只确认实际写入字段”；不能把版本框架或跨进程 CAS 当作修复此问题的前提。现有 Store 已有事务内增量合并，实际位置是 [gw-store/src/lib.rs:980](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-store/src/lib.rs:980)，不存在要求中提到的 `control.rs`。

3. **medium｜计划缺口：Temporal 不是等待人工启用的休眠路径，不能无条件延期。**

   [sandchat.rs:1371](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-cursor/src/sandchat.rs:1371) 优先调用 `ensure_agent`；仅在确认 Temporal 门控后才走 box。门控缓存还会到期失效（[sandchat.rs:236](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-cursor/src/sandchat.rs:236)）。

   **失败序列：**已有可用 Temporal agent，或后续探测成功 → 自动进入 Temporal → 客户端保留消息前缀但修改 system/tools → `lookup` 只比较消息指纹并决定续接（[sandchat.rs:274](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-cursor/src/sandchat.rs:274)）→ 增量正文不携带新 head（[sandchat.rs:1017](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-cursor/src/sandchat.rs:1017)）。此外，续接判断在账号 run gate 之前。

   最小修复是锁内重判并比较 head。若延期，必须有可核实的本地不可达条件；历史 box 验收不足以支持延期。

4. **medium｜当前代码缺陷＋范围建议：R3 应先修两个状态传递点和非流式重放边界，再统一恢复编排。**

   同步刷新后的流进入 `finish_response`，两条响应路径重新传入 `false`（[worker/mod.rs:4101](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-app/src/worker/mod.rs:4101)、4120）；响应路径的 RPM 准入又丢失成员视图（[worker/mod.rs:4207](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-app/src/worker/mod.rs:4207)、4655）。这些可以先通过传递已有状态修复。

   还有更直接的遗漏：非流式收集已收到正文或 Usage 后，若随后出现 `TokenInvalid`，仍可刷新并重新推理（[worker/mod.rs:4166](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-app/src/worker/mod.rs:4166)、4195）。

   **失败序列：**上游输出部分正文 → 流内认证错误 → 客户端因非流式尚未收到内容 → 整请求重发。客户端未输出不能证明上游未执行。

   先补“已恢复状态、成员视图、上游已有产出”的定点修复。账号切换、profileArn 和过载预算仍保留原作用域；三条路径的测试应比较各自允许的行为，不能为了次数一致而扩大恢复范围。

5. **low｜过度设计风险：R5 把输出提取与 HTTP client 策略合并成一个阶段，验证成本没有必要绑定。**

   输出确有第二个调用方：[box_gateway.rs:292](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-cursor/src/sandchat/box_gateway.rs:292) 使用 `inference::Folder`。但传输构造存在刻意差异：通用账号 client 按代理 URL 缓存（[lib.rs:1542](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-cursor/src/lib.rs:1542)），Inference/Sandchat client 按账号与代理缓存并强制 HTTP/1（[inference.rs:281](/home/iiap/桌面/self-work/claude-all-in-one/crates/gw-cursor/src/inference.rs:281)）。

   若“集中构造”直接复用通用 client，会遗漏已有 HTTP/1 约束；若反向统一，则改变其他协议的传输行为。计划尚未证明需要这种统一。最小范围是按需共享重复的重定向策略，保留各协议 client 构造；输出提取另作独立提交。

最短修正顺序：**R0 固定含未跟踪文件的基准 → R1 router 重放修复 → box 送达未知修复 → R2 待写增量修复 → R3 状态传递与非流式边界修复**。Temporal 最小修复在继续开放该路径前完成。每项先做针对性回归，再考虑移动模块。

可延期：`credentials.rs`／`retry.rs` 提取、R4 配置入口整理、R5 输出提取和传输去重，以及已有的生命周期设计议题。

本轮只读检查了指定核心调用链、架构文档、代码差异和两个未跟踪 box 模块；`git diff --check` 通过。未修改文件、运行测试、访问网络或生产。