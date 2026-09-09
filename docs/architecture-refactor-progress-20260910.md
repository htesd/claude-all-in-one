# 架构重构实施记录（2026-09-10）

用户已授权按 [计划](architecture-refactor-plan-20260910.md) 顺序实施。

## R0：基准已固定

- 原 HEAD：`28b4e063e8b851b9dafec230f478f2882f21b0a0`。
- 上一轮已验证功能独立提交：`505ebb3`，作为本轮代码基准；未推送远端。
- 本轮重新运行 `cargo test --workspace`：1,744 通过，0 失败。
  日志：`/tmp/caio-refactor-20260910/baseline-tests.log`。
- `.notice-post-meta-tools.txt` 和 `PI-TOOLCALL-FIX-FOR-KIMI.md` 为既有用户文件，保留不提交。
- 本轮只读确认部署镜像：bot=`box-gateway-20260910-ready`，cursor=`stream401-demote-20260909`，
  router=`textemu-guard-20260908`，worker0=`cache-guard-20260824`，dario worker/router=`poison-fix`。
  此处省略共同前缀 `claude-all-in-one:`；尚未部署重构代码。

## 验收入口矩阵

| 入口 | 流式 | 非流式 | 协议约束 |
|---|---|---|---|
| Messages | SSE | Messages JSON | 所有生产 provider |
| Chat Completions | OpenAI SSE | completion JSON | 当前仅 Cursor 挂载 |
| Responses | Responses SSE | response JSON | 当前仅 Cursor 挂载 |

各阶段使用真实 TCP worker、fake Provider 和临时 SQLite 注入对应故障；最终再做实际客户端验收。
已有客户端证据属于基准，不能替代重构后的验证。

## 当前进度

- R0：完成。
- R1：完成。router 提交 `8815dcf`，provider 送达事实提交 `170552f`；另修复续收 401 误判永久失效。
  全工作区 1,754 项通过；真实 TCP 及 36 组合恢复/健康断言通过。
  三视角 CLI 对抗审查及两轮定点复查已完成，接受送达标注和健康分类问题并修复。
  最后定点复查通过，原始报告在 `/tmp/caio-refactor-20260910/r1-followup/health.md`。
- R2：实施中。R2a 提交 `387841d`，管理端写事务与独立连接测试通过。
  R2b 待写字段、条件合并、版本确认与同步快照准入已完成；三视角审查指出的 ARN 身份依赖问题
  已扩展到普通/强制发现链路修复，Notify 控制的四种交错通过，最后定点 CLI 复查 PASS。
  原始报告：`/tmp/caio-refactor-20260910/r2b-review/profile-followup.md`。
  R2c 生产队列回归已证实旧行为会丢新凭据，排序修复测试通过；停机收尾待完成。
- R3–R5：待实施。
