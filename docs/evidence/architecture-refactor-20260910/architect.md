结论：计划需修订后再进入实施，主要阻塞点是 provider 送达契约、凭据写入边界及升级顺序。以下均为只读代码核实与条件性失败序列，未声称观察到线上故障；未运行测试、访问网络或凭据文件。

1. **high｜当前代码问题＋计划依赖缺口：worker 无法独自判断上游是否已执行，必须先补 provider 契约。**

   [`UpstreamError`](crates/gw-core/src/error.rs:174) 只有错误类别等信息，没有送达状态；[`Provider::chat`](crates/gw-core/src/provider.rs:299) 仍把首包前错误描述为可透明重试。

   box 的 [`command`](crates/gw-cursor/src/sandchat/box_gateway.rs:136) 将发送超时、响应读取失败统一变成 `ServerError`；[`sendPrompt`](crates/gw-cursor/src/sandchat/box_gateway.rs:614) 失败经 readiness 返回 `chat()` 的同步错误，随后 [`worker`](crates/gw-app/src/worker/mod.rs:3214) 按类别换号。

   **失败序列：**账号 A 的云端已接受 prompt，但响应丢失 → interrupt/delete 即使成功，也不能证明此前没有执行 → worker 换 B，以新 nonce 再提交。

   R3 的“保留准备失败换号”必须拆成**确定未提交**与**提交结果未知**。先给 provider 错误补充最小送达事实，再由 worker 决定重试；不能只增加 `RetryContext`。这部分正确性修复应与 R1 同批提前，不能排在完整 R2 之后，也不能作为 box 的既有行为原样保留。

2. **high｜当前代码问题＋R2 范围缺口：管理员 `extra_merge` 本身仍会整块覆盖凭据，Store 已有事务不能解决它。**

   [`admin/accounts.rs:1620`](crates/gw-app/src/admin/accounts.rs:1620) 先读取账号，脱敏哨兵也在这里还原；[`1641`](crates/gw-app/src/admin/accounts.rs:1641) 的 `extra_merge` 在内存合并整份旧 extra，再经 [`1699`](crates/gw-app/src/admin/accounts.rs:1699) 调用整块更新。Store 最终执行 [`extra = ?`](crates/gw-store/src/lib.rs:800)。

   **失败序列：**管理员读取 T0，只改 driver → worker 将 T1 成功落库 → 管理员提交含 T0 的合并结果 → 数据库回退 T0。这个序列不需要刷新写失败。

   因此，R2 不能把管理员仅列为外部并发参与者、把条件更新写成“核对是否需要”。必须修改实际写入入口：增量更新、哨兵保留和显式凭据替换分别定义事务内语义。现有 [`merge_account_extra`](crates/gw-store/src/lib.rs:980) 已有事务；缺的是过期写入判断及调用方边界，不是笼统“增加事务”。计划中的 `gw-store/src/control.rs` 也应改为实际位置 `src/lib.rs`。

3. **high｜已核实身份契约冲突：孪生恢复已经满足计划所述的提升条件，不能继续列为待证实。**

   [`admin/accounts.rs:1329`](crates/gw-app/src/admin/accounts.rs:1329) 根据不同 `client_id` 判定孪生，并在 [`1351`](crates/gw-app/src/admin/accounts.rs:1351) 写入源号关系。可是 [`twin_resync`](crates/gw-app/src/worker/mod.rs:365) 不检查 provider、注册身份或认证区域，只复制三个 token 字段。

   **失败序列：**不同 IdC 注册的 B 刷新失败 → 借 A 的 refresh token → 保留 B 的 client ID、secret 和区域 → [`refresh_idc`](crates/gw-kiro/src/token.rs:214) 将这组混合凭据发出。代码已经证明身份混搭；是否被上游拒绝不需要靠本轮生产探测确认。

   应把关系区分为“导入身份碰撞派生”与“经验证可共享凭据链”，在 R2 内阻止前者借链。即使确属同链，不同 account ID 的进程内锁也不会互斥，仍需明确刷新所有者。

4. **high｜升级计划缺口：“兼容共存、每项独立回滚”缺少可执行约束。**

   按当前代码构建的旧 router 仍对全部发送错误及 OpenAI 404 重放（[`router/mod.rs:545`](crates/gw-app/src/router/mod.rs:545)、[`568`](crates/gw-app/src/router/mod.rs:568)）；旧 worker 会把完整脏 extra 回写（[`worker/mod.rs:1579`](crates/gw-app/src/worker/mod.rs:1579)）；旧管理进程仍存在第 2 项覆盖路径。

   **失败序列：**新进程条件写入 T1 → 尚存旧 worker flush 或旧管理员保存 T0 → 旧 SQL 不检查新版本依据，照样覆盖。数据库向后兼容、旧镜像可读均不能阻止它。

   计划应具体规定：

   - R1 先升级 router，使无明确证明的旧 worker 响应按未知处理；再增加 worker 能力标记。
   - R2 明确所有共享数据库写入服务的升级范围；同一凭据所有权不能由新旧 worker 重叠持有。
   - 若允许旧写入方继续运行，必须说明数据库如何阻止其破坏新不变量；否则采用暂停相关写入口及停止旧 owner 后交接。
   - 回滚必须区分“旧镜像能启动”与“旧写入逻辑仍安全”。后者目前不成立。

5. **medium｜当前代码问题＋R2 生命周期缺口：只管理 worker 待写增量，仍未覆盖生产方丢更新与停机尾部。**

   Cursor 的 [`report_token_update`](crates/gw-cursor/src/clidrv.rs:262) 对同账号直接覆盖 HashMap；[`prepare_home`](crates/gw-cursor/src/clidrv.rs:387) 与 [`pump`](crates/gw-cursor/src/clidrv.rs:2112) 都是生产方。较旧观测若迟到，可以在 worker 消费前覆盖较新观测，消费者随后再做新旧判断也无法找回被丢的数据。

   停机另有独立窗口：[`worker/mod.rs:1485`](crates/gw-app/src/worker/mod.rs:1485) 只 flush scheduler 脏状态，没有最后一次采纳 provider 队列；后台配额任务也没有停止协调（[`1375`](crates/gw-app/src/worker/mod.rs:1375)）。

   **失败序列：**CLI 已捕获新 token，但尚未到轮询时间 → HTTP 排空 → scheduler 没有脏项 → worker 返回退出，新 token 未进入数据库。若 CLI 文件仍保留，可能后续恢复，但不能据此宣称数据库排空成功。

   R2 应覆盖生产方合并规则，并规定停机顺序：停止并等待凭据生产方 → 最后采纳队列 → 持久化待写增量 → 退出。无需先建设通用后台任务框架。

最短修正执行顺序：**R0 固定工作区基准与写入方清单 → R1 router 保守重放＋提前完成 provider 送达契约修复 → R2 管理端原子写入、孪生身份约束、生产方/停机收敛及升级交接 → R3 统一请求预算与成员视图 → 分开提交模块提取。** 每步先验证具体失败序列，再推进。

可延期：R4 配置入口归并、R5 输出折叠器和 client 构造提取、YAML-only 与退役政策。Temporal 修复只有在明确阻止进入该路径时才能延期：当前 [`sandchat.rs:1371`](crates/gw-cursor/src/sandchat.rs:1371) 在 `ensure_agent` 成功后会自动进入 Temporal，并非独立显式启用的路径。