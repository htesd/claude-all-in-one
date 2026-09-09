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
- R1：实施中。
- R2–R5：待实施。
