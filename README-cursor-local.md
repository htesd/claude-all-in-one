# Cursor 本地网关（测试用）

把本机 Cursor 订阅接成 Anthropic 端点，供 opencode 等客户端调用。

## 起停

```bash
cd ~/桌面/self-work/claude-all-in-one
./target/release/claude-all-in-one --mode worker --instance 0 &
./target/release/claude-all-in-one --mode router &
```

- router `127.0.0.1:8990`（对外 Anthropic 线缆 + `/admin`）
- worker `127.0.0.1:9000`
- admin token：`local-dev-admin-token`（`config/system.yaml`）
- 客户 key：`~/.config/opencode/.secrets/cursor_local_key`

停：`pkill -f 'claude-all-in-one --mode'`

## opencode 里怎么用

已在 `~/.config/opencode/opencode.jsonc` 加了 `cursor-local` provider。

```
/models  → 选 "Cursor 本地网关"
```

或直接指定：`opencode --model cursor-local/grok-4.5`

## 能用的模型

| 模型 | 状态 |
|---|---|
| `grok-4.5` | ✅ |
| `composer-2.5` | ✅ |
| `default`（UI 的 Auto，路由到 Composer） | ✅ |
| `claude-*` / `gpt-*` | ❌ 本号前沿模型额度已耗尽（`ERROR_RATE_LIMITED_CHANGEABLE`）——**是计费不是协议** |

## ⚠️ 这一版的已知限制

1. **多轮历史是「折进单条消息」的。** 上游不接受 repeated `1.2.1`（实测：多于一条就
   200 接受然后永远只发心跳）。所以 Anthropic 传来的历史被渲染成
   `<conversation_history>…</conversation_history>` 附在当前消息前。
   模型读得到（实测多轮问答正确），但上游看到的是一条长消息而非结构化对话。
   真·有状态会话见 `crates/gw-cursor/PROTOCOL-agent-run.md` §12，仍缺 FileSync。
2. **`CURSOR_STATEFUL=1`** 可打开后续轮形态实验（默认关，因为目前会挂起）。
3. **没有** tool_use、thinking 透传、图像、文件附件。opencode 里别开需要工具的模式。
4. 账号凭据来自本机 Cursor 登录态（`config/accounts.yaml`，含活 token，**别提交**）。
   Cursor 重新登录后 token 会变，需要重新生成该文件。
5. 只有一个号、`max_concurrency` 默认值 —— 压测会撞并发闸。

## 排查

```bash
tail -f /tmp/claude-1000/*/scratchpad/worker.log     # 逐帧诊断(RUST_LOG=gw_cursor=debug)
curl -s http://127.0.0.1:8990/admin/api/accounts -H "x-api-key: local-dev-admin-token"
```

常见错误对照：

| 现象 | 含义 |
|---|---|
| `EmptyResponse: 90s 内只有心跳` | 上游接受了请求但不生成 —— 请求形态不对（见 PROTOCOL §11/§12） |
| `resource_exhausted` + `autoSwitchToModel` | 该模型额度耗尽，换 grok-4.5 / composer-2.5 |
| `Failed to resolve request context blobs` | 发了 `1.2.17` 的 blob 哈希但没走 FileSync 上传 |
