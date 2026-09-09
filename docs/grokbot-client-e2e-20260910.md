# GrokBot 客户端端到端验收（2026-09-10）

## 范围

模型 `grok_bot_auto`。从本机实际启动 Claude Code 2.1.266 和 OpenCode 1.18.30，
使用现有 BOT 组 key 访问现有公网 router，经过生产 worker、账号调度和 box 网关。
临时测试目录包含随机文本、PNG 和 PDF；请求提示中不提供附件内的校验码。

这次验收同时检查客户端的工具事件、真实本地文件和工具结果后的最终回答。
Rust 驱动直连的五轮验证作为下层证据，不用它替代公网和客户端验收。

## 已验证链路

| 检查 | Claude Code | OpenCode |
|---|---|---|
| Bash 真实执行 | `Bash`，本机运行 Node 生成随机码并写文件；工具结果续轮准确回传 | `bash`，命令退出码 0；本机文件和最终回复匹配 |
| 图片 | `Read` 读取本机 PNG，工具结果携带图片，正确识别 `IMG-BE070A893C` | `--file` 传图片，正确识别同一随机码 |
| PDF | `Read` 的工具结果携带原始 base64 PDF，正确回答随机码及合计 | `read` 返回 `application/pdf` 附件，正确回答随机码及合计 |
| PDF 内容 | `PDF-59C61E31CC`，合计 `391` | `PDF-59C61E31CC`，合计 `391` |
| 续接已有会话 | `--resume` 后正确回答先前的随机口令，无工具调用 | `--session` 后正确复述先前 Bash 返回的随机值，无工具调用 |

公网 HTTP 独立检查还覆盖非流式正文、SSE 的 `message_stop`、直接 base64 图片和 PDF。
具体响应、工具参数/结果、本机文件摘要见 [验收数据](evidence/grokbot-20260910/results.json)。

## 修复前实际失败与处理

1. 公网入口复现了用户报告的 **503 overloaded_error**。线上仍是旧
   `stream401-demote-20260909` 镜像，Temporal 未开放使所有候选失败。切换到 box 网关镜像。
2. 初版 box 对普通文字有效，但真实 Bash 测试中，上游在云端寻找本机临时目录。
   增加 box 专用客户端工具转发契约后，两款客户端的 Bash 回路均通过。
3. 旧 sandchat 把图片替换为省略提示，PDF 只抽文本。现在 box 上传原始文件，
   并支持 `tool_result` 内嵌附件；校验先于 PDF 渲染/提取。
4. box 准备阶段失败曾因提前返回流对象而绕过 worker 换号路径，表现为 502。
   增加准备完成信号，准备错误仍作为 `Provider::chat` 的错误返回。
5. `bot1`～`bot6`、`bot7-2`、`bot8`、`bot11`、`bot12` 共 10 条旧凭据
   被上游明确判定为永久失效，但重启后仍参与选号。
   已通过管理 API 将上述记录设为 disabled，保留全部凭据和账号记录。重新登录后可再启用。
6. 本机这版 Claude Code 的文件工具为 `Read`、`Edit`，没有 `Write`。测试已改用实际工具；
   第一轮 Edit 多写一个空格使严格格式断言失败，另发修正指令验证编辑与结果续轮。

## 版本与检查

- 当前 BOT worker：`claude-all-in-one:box-gateway-20260910-ready`。
- 镜像摘要：`sha256:7e7e6d35a19942bda8fd36a8877799225c36c7b16fd9faeec9c05911466e04f9`。
- 完整客户端 Bash/图片/PDF 矩阵在 v3 完成；后续附件前置校验、准备握手修正单独审查，
  最终镜像再次执行 OpenCode Bash、Claude Code 编辑和公网 PDF 检查，结果纳入验收数据。
- 最终公网 PDF 请求返回 200，耗时约 25 秒，随机码及合计均正确。
- `cargo test --workspace`：**1,744 通过，0 失败**；Docker 内前端构建及后端 release 构建通过。
- 实际 `codex exec` 对抗审查及后续定点复查均执行；最终准备握手复查未发现具体回归。
- 仅更新 BOT worker；Compose 已写入新镜像标签。旧镜像和 Compose 备份保留。

## 实际限制

- 可以完成上述功能，不代表所有账号都有效或上游始终稳定。仍受上游容量、账号额度和
  固定代理可用性影响；不会为了测试成功移除鉴权、改出口或回退到其他池。
- `ultra-test` 的 cloud run 连续失败三次，被既有调度器自动运行时禁用；未改它的持久化
  enabled 状态。该状态重启后可能重置，重新选中仍可能出现上游错误，需要继续关注。
- 延迟有明显波动：Claude Code Bash 这轮约 29 秒，图片约 111 秒，PDF 约 189 秒；
  OpenCode 一次续接也等待数分钟，transcript 显示是上游该轮处理较慢。
- box 本地工具转发依赖提示契约，上游协议没有原生的客户端工具通道。
- 内联附件最多 16 个、解码总量 20 MiB；不抓取 URL 附件。Temporal 路径保留旧附件限制。
- token 计费为估算；进程异常退出或网络清理失败可能遗留云端 agent，重启时全量恢复历史。

## 运维记录

生产机保留 `docker-compose.pre-box-gateway-20260909.yml` 与
`/root/caio-bot-disabled-20260910.json`，分别记录部署前配置和这些账号的原 disabled 状态。
回滚镜像不应自动重新启用已确认失效的凭据；恢复账号应先取得有效的新凭据。

原始客户端日志保存在本机 `/tmp/caio-client-e2e/`；项目内只保存合成测试内容及脱敏摘要，
不保存 API key、上游 token 或真实用户文档。
