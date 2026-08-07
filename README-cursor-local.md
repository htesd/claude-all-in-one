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
| `claude-opus-5` / `claude-sonnet-5` / `claude-fable-5` | ✅ 实测可用（2026-08-07） |
| `gpt-5.6-sol` / `gpt-5.6-terra` | ⚠️ 未实测 |

> ⚠️ 早先这里写「claude-* 前沿模型额度已耗尽」——**那是当时那个号的计费状态,不是通道能力**。
> `ERROR_RATE_LIMITED_CHANGEABLE` 是「该号在该模型上没额度」,换个有额度的号就好了
> (代码里已映射成 `ModelNotAvailable`:不惩罚账号、换号重试)。2026-08-07 实测
> `claude-sonnet-5` 正常出字、正常回 `tool_use`。

> 💡 输入成本按模型差一倍:Cursor 会注入自己的服务端系统提示,同一句问话
> `claude-sonnet-5` 报 ~26.7k input,`grok-4.5` 只报 ~12.8k(2026-08-07 实测)。

## ⚠️ 这一版的已知限制

1. **多轮历史是「折进单条消息」的。** 上游不接受 repeated `1.2.1`（实测：多于一条就
   200 接受然后永远只发心跳）。所以 Anthropic 传来的历史被渲染成
   `<conversation_history>…</conversation_history>` 附在当前消息前。
   模型读得到（实测多轮问答正确），但上游看到的是一条长消息而非结构化对话。
   真·有状态会话见 `crates/gw-cursor/PROTOCOL-agent-run.md` §12，仍缺 FileSync。
2. **`CURSOR_STATEFUL=1`** 可打开后续轮形态实验（默认关，因为目前会挂起）。
3. ~~没有 tool_use、thinking 透传、图像、文件附件。~~ **已全部支持**(2026-08-07):
   - `tool_use` 往返,参数含数字/布尔/嵌套对象(值是 `google.protobuf.Value`)。
     实测 Claude Code 与 opencode 的工具回路都能收敛。
   - thinking 透传成 Anthropic thinking 块(**只在调用方请求时发**)。
     注意上游是否产出 `1.4` 帧**由它自己决定**,与我方参数无关,见 PROTOCOL §16。
   - 图像内联、PDF(我方抽文本层)。
   仍**不支持**的是 Cursor 服务端**内建工具**(终端/读文件/网页搜索)的代执行 ——
   那等于跑模型选定的任意 shell 命令,**有意不做**;用系统提示护栏拦住。
4. 账号凭据来自本机 Cursor 登录态（`config/accounts.yaml`，含活 token，**别提交**）。
   Cursor 重新登录后 token 会变，需要重新生成该文件。
5. 只有一个号、`max_concurrency` 默认值 —— 压测会撞并发闸。
6. **在宿主上直接跑测试前先 `env -u ANTHROPIC_BASE_URL`。** 这台机器的 shell 里
   export 了 `ANTHROPIC_BASE_URL=https://api.anthropic.com`,忘了清就会静默打到
   真的 Anthropic API,回来一个带 Anthropic `request_id` 的 401,看起来跟网关鉴权
   失败一模一样(2026-08-07 踩过)。Docker 里不继承宿主 env,免疫。

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
