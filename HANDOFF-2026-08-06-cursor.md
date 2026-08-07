# caio 交接 — 2026-08-06（gw-cursor / Cursor IDE 反代）

> 给后续 Claude Code / kimi / 任意 agent 用。读完即可接着干，不必重做已排除的实验。
> 工作区：`/home/iiap/桌面/self-work/claude-all-in-one`，分支多为 `refactor/account-group-membership`。
> 本机 Cursor：**3.14.27**（commit `047548b00c1a079373d74d00183f32510a4a41e0`），账号 **pro / active**。

---

## 0. 一句话现状

已新增 **`gw-cursor`** Provider（Anthropic IR → Cursor ConnectRPC/protobuf），协议栈与鉴权在真实服务端**全部验证通过**；**唯独推理调用**被一道「只拦推理、不拦 unary」的客户端完整性门软封。静态搬身份值已穷尽；破门需要**抓一条真实 IDE 的 `StreamUnifiedChatWithTools` 请求做逐字节 diff**，或改走官方 **`crsr_` API-key（cli）路径**。

用户意图：多进程下每个 worker「成为」一个真实 Cursor IDE 实例（固定出口 + 冻结指纹），不是包 CLI 子进程。

---

## 1. 交付了什么（代码）

### 1.1 新 crate

| 路径 | 职责 |
|---|---|
| `crates/gw-cursor/src/protobuf.rs` | 手写 protobuf wire 编解码（零 prost），varint / length-delimited |
| `crates/gw-cursor/src/wire.rs` | `x-cursor-checksum`（zyg cipher, t=165）、Connect 分帧 `[flag:1][len:4BE][payload]`、客户端版本常量 |
| `crates/gw-cursor/src/config.rs` | unary `ServerConfigService/GetServerConfig`，解析响应 **field 6 = `config_version`** |
| `crates/gw-cursor/src/chat.rs` | Anthropic → Cursor protobuf；Cursor 流 → Anthropic SSE；MVP 仅文本 |
| `crates/gw-cursor/src/models.rs` | 对外模型名 → Cursor 点分名（如 `claude-sonnet-4-5` → `claude-4.5-sonnet`） |
| `crates/gw-cursor/src/lib.rs` | `Provider` 实现，`family = "cursor"`；按账号缓存 config_version（TTL 120s） |
| `crates/gw-cursor/examples/e2e.rs` | 只读 `state.vscdb` 取 token/身份，打真实上游 |

### 1.2 已接线

- workspace `Cargo.toml`：members + `gw-cursor` path dep
- `crates/gw-app/Cargo.toml`：依赖 `gw-cursor`
- `crates/gw-app/src/registry.rs`：`reg.register("cursor", …)` + 单测 `builtins_include_cursor`

### 1.3 验证命令

```bash
cd /home/iiap/桌面/self-work/claude-all-in-one
# 限内存（本机曾被 cargo -j14 打穿）
nice -n 15 cargo test -p gw-cursor -j 4          # 约 25 测全绿（以本地为准）
nice -n 15 cargo run -p gw-cursor --example e2e -j 4 -- "Reply with exactly: hi"
# 可选：CURSOR_METHOD=StreamUnifiedChatWithTools|StreamUnifiedChat
#       CURSOR_E2E_MODEL=claude-4.5-sonnet
```

**未提交 / 未上生产**（截至写文档时工作区改动在本地）。admin-ui 账号页尚未专门适配 cursor schema（后端 schema 会驱动表单，但 UI 家族名列表可能还需补）。

---

## 2. 协议事实（已在真实服务端验证）

### 2.1 传输与端点

| 项 | 值 |
|---|---|
| Host | `api2.cursor.sh`（bundle 还有 api3/4/5） |
| 模型列表 | `POST /aiserver.v1.AiService/AvailableModels` — **HTTP/1.1 即可，空 body，HTTP 200** |
| 会话配置 | `POST /aiserver.v1.ServerConfigService/GetServerConfig` — unary，空 body，**field 6 = config_version** |
| 推理（当前 IDE） | `POST /aiserver.v1.ChatService/StreamUnifiedChatWithTools` — **强制 HTTP/2**；HTTP/1.1 → ALB **464** |
| Content-Type | `application/connect+proto`（流）/ `application/proto`（unary） |
| Connect 信封 | `[flags:1][len:4 大端][payload]`；flag bit1(0x02)= end-stream trailer（JSON） |

ChatService 方法族（bundle `makeMessageType`）：

- `StreamUnifiedChat` — ServerStreaming，**已 DEPRECATED**（会吐文案再报 `ERROR_DEPRECATED`）
- `StreamUnifiedChatWithTools` — **BiDiStreaming**（IDE 主路径）
- `…WithToolsIdempotent` / `…SSE` / `…Poll` — 变体；SSE 输入是 `BidiRequestId`，不是直接塞 chat body

### 2.2 Checksum（zyg / 「Jyh」）

逐字复刻 workbench `zyg`：

```
timestamp = Math.floor(Date.now() / 1e6)  → 6 字节大端
t = 165
for i in 0..5: e[i] = (e[i] ^ t) + (i % 256); t = e[i]
x-cursor-checksum = urlsafe_b64_nopad(e) + machineId [ + "/" + macMachineId ]
```

`x-client-key` = sha256hex(token)；`x-session-id` = UUIDv5(DNS, token)。

本机身份来源：

| 键 | 位置 | 说明 |
|---|---|---|
| `cursorAuth/accessToken` | `~/.config/Cursor/User/globalStorage/state.vscdb` | JWT session，**不是** `crsr_` API key |
| `storage.serviceMachineId` | 同上 | 36 位 UUID（服务级） |
| `telemetry.machineId` / `macMachineId` | 常在 `storage.json`；本机 vscdb 里可能缺失 | 真 IDE checksum 用 telemetry 口径 |
| `cursorai/serverConfig` | state.vscdb | 含磁盘缓存的 `configVersion`（**会过期**，须现调 GetServerConfig） |

### 2.3 最小 protobuf（MVP 已够出字——若门放行）

`StreamUnifiedChatRequest`（inner）：

- `1` conversation[]：`ConversationMessage{1 text, 2 type(1=HUMAN,2=AI), 13 bubble_id}`
- `3` explicit_context（system）
- `5` model_details{1 model_name, 可选 5 enable_slow_pool}
- `22` is_chat=true
- `23` conversation_id

WithTools 外包：`StreamUnifiedChatRequestWithTools{1: stream_unified_chat_request}`。  
响应文本：`StreamUnifiedChatResponseWithTools` oneof field **2** → `StreamUnifiedChatResponse` field **1** = text。

解包脚本（只读 bundle）：`/tmp/cursor_proto_extract.py`  
`python3 /tmp/cursor_proto_extract.py flat aiserver.v1.StreamUnifiedChatRequest`

Bundle：`/usr/share/cursor/resources/app/out/vs/workbench/workbench.desktop.main.js`（约 40MB）。

---

## 3. 卡住的确切错误

`StreamUnifiedChatWithTools` 返回 Connect end-stream trailer（HTTP 本身可能仍是 200 流）：

```json
{
  "code": "resource_exhausted",
  "details": [{
    "debug": {
      "error": "ERROR_GPT_4_VISION_PREVIEW_RATE_LIMIT",
      "isExpected": true,
      "details": {
        "title": "Update Required",
        "detail": "Your version of Cursor is no longer supported. Please update to the latest version at cursor.com/downloads to continue.",
        "analyticsMetadata": { "actionRequired": "payment" }
      }
    }
  }]
}
```

**障眼点**：文案像「版本过旧 / 要付费」，实测**都不是**。

---

## 4. 已排除清单（不要重做）

对本机 **pro / active / 本月 usage≈0 / token 未过期** 账号实测：

| 假设 | 结论 |
|---|---|
| 客户端版本过旧 | **否**。真 IDE 也发 `3.14.27`；GetServerConfig 自证 `updateLevel=CLIENT_UPDATE_LEVEL_NONE`，minSupported≈3.4.0 |
| 额度耗尽 / 未付费 | **否**。`full_stripe_profile`：pro、active、`lastPaymentFailed=false`；`/auth/usage` 本月近 0 |
| machineId 格式 | **否**。36 位 serviceMachineId、64 位 sha256(token)、真 telemetry 64hex+mac 都试过 |
| 随机 vs 磁盘 vs **新鲜 GetServerConfig** 的 config_version | **否**。机制正确（field 6、会轮换），但**换真值仍被拦**——不是破门关键 |
| enable_slow_pool | **否** |
| 模型档位 | **否**。haiku / flash / mini / 旧 sonnet / 新模型**全拦**；非法名才变 `ERROR_BAD_MODEL_NAME` |
| 纯 `StreamUnifiedChat` | 另一错：`ERROR_DEPRECATED`（会先吐一段升级提示文本） |

**决定性证据**：非法模型名 → `ERROR_BAD_MODEL_NAME` ⇒ 请求**已过鉴权 + 模型路由**；同一套 Bearer+checksum 的 **AvailableModels / GetServerConfig 全 200**；只有「真正跑推理」被统一软封。

**结论**：存在**只作用于推理路径的运行时「客户端证明」**，静态复刻版本/commit/machineId/macMachineId/config_version/头清单**不够**。

---

## 5. kimi 咨询结论（2026-08-06）

本机 kimicode：`/home/iiap/.kimi-code/bin/kimi`（约 v0.29.0），非交互：`kimi -p "…"`。

kimi 独立 grep 了 bundle，关键贡献：

1. **`x-cursor-config-version` = GetServerConfig 响应 field 6 的回显**，不是随机 UUID —— 已实现并验证；**仍不破门**。
2. 门更可能在 **WithTools / agent 端点能力字段或某次握手**，而非单纯缺某条静态头。
3. 务实建议：**API-key（cli）路径作交付主线**，IDE session 路径作研究线。
4. 抓包可不 MITM：DevTools Network / Node `--require` 钩 `http2`。

---

## 6. 两条接入路径（架构选型）

| | IDE 路径（当前 gw-cursor） | CLI / API-key 路径 |
|---|---|---|
| Auth | `state.vscdb` 的 session JWT | 官方 **`crsr_…`**（Dashboard 生成） |
| `x-cursor-client-type` | `ide` | `cli` |
| `x-cursor-client-version` | `3.14.27` | `cli-2026.08.04-aaa8809`（本机 agent） |
| checksum / 全套 client-* | **要** | agent 实测**几乎不要**（极少头 + `x-cursor-streaming: true`） |
| 指纹门 | **卡死推理** | **agent 已证明可过**（需真 API key；session JWT 当 API key 会被拒） |
| 适合反代？ | 与「每 worker 一个 IDE」愿景一致，但破门未完成 | 更稳、更正规；计费/模型差异未测全 |

本机 agent 位置：

```
~/.config/Cursor/User/globalStorage/anysphere.cursor-agent-worker/agent-cli/.local/bin/cursor-agent
→ …/versions/2026.08.04-aaa8809/   # 含 index.js、自带 node
```

`CURSOR_API_KEY=crsr_… agent -p --output-format text "hi"` 可测；**不要**用 session JWT 填 API_KEY。

---

## 7. 抓包现状（未完成）

目标：抓真 IDE 的 `StreamUnifiedChatWithTools` **完整 headers + framed body**，与 `gw-cursor` 逐字节 diff。

- 钩子草稿：`/tmp/hook.js`（`NODE_OPTIONS=--require /tmp/hook.js` 钩 `node:http2`）
- **曾失败**：后台 agent「另起一个 Cursor」而不是杀干净旧进程；旧 PID 仍无钩子；用户拒绝/跳过了一次杀进程重启
- **风险**：重启 Cursor = 断掉当前 agent 会话；须用户明确同意后再 `pkill` + 带钩子 relaunch
- 备选：Help → Toggle Developer Tools → Network 过滤 `StreamUnifiedChat`；或 `--remote-debugging-port` + CDP
- 注意：若 chat 走**渲染进程 fetch** 而非 exthost `node:http2`，Node 钩子**抓不到**，必须 CDP/DevTools

辅助脚本（/tmp，不入库）：

- `cursor_proto_extract.py` — schema
- `cursor_handshake.py` — AvailableModels
- `cursor_chat.py` — 早期 Python 原型（HTTP/1.1 会 464，仅作参考）

---

## 8. 账号字段（设计意图）

`extra`：

| 字段 | 必填 | 说明 |
|---|---|---|
| `access_token` | 是 | session JWT（IDE 路径） |
| `machine_id` | 否 | 真 telemetry.machineId；空则 sha256hex(token) |
| `mac_machine_id` | 否 | 有则 checksum 用 `mid/mac` 形态 |
| `config_version` | 否 | 一般留空，由 provider 调 GetServerConfig |
| `proxy` | 否 | 与 dario/kiro 同：刷新与发包同出口 |

`refresh_auth`：**未实现**（MVP）；token 过期后会 TokenInvalid。生产前必须补 Cursor 刷新端点。

---

## 9. 建议下一步（按优先级）

### A. 破 IDE 门（用户曾选 capture）

1. 用户保存工作 → **杀干净**所有 `/usr/share/cursor/cursor` → 用 `NODE_OPTIONS=--require /tmp/hook.js` **只起一个**实例  
2. 在 IDE 发一句 chat → 确认 `/tmp/cursor_capture.log` 与 `/tmp/cursor_real_body.bin`  
3. Diff：多出来的 header / protobuf 字段 / BiDi 多帧交互  
4. 复刻进 `gw-cursor` → `e2e` 出字  
5. 若钩子未加载到 exthost → 立刻改 DevTools/CDP，别空等

### B. 交付主线改 API-key（kimi 推荐）

1. 用户提供 `crsr_` key  
2. 用同一 hook 抓 `cursor-agent` 真请求（或静态读 `index.js`）  
3. `gw-cursor` 增加 `type=cli` 模式：极简头 + `x-cursor-streaming`，砍掉强制 checksum  
4. e2e 出字 → 再补模型目录 / 限流语义文档

### C. 工程收尾（门未破也可做）

- 提交 `gw-cursor`（Conventional Commits + 中文 subject，如 `feat(cursor): …`）
- admin-ui 家族名 / OAuth 入口若需要再补
- CHANGELOG 记「协议已通、推理门未破」
- **勿**在测试里提交真实 token

---

## 10. 操作纪律

- 编译：`cargo test -j 4` + 内存上限（见旧 `HANDOFF-2026-07-28.md` §7）；禁默认 `-j14`
- 拆 bundle：流式 `awk` / 现成 extract 脚本；禁把整文件读进内存、禁 js-beautify 整包
- 凭据：只读 vscdb；日志打码；不把 Bearer 写入仓库或 PR
- 对真号：优先 unary 只读；推理试错用最短 prompt
- 本仓库注释/提交：**中文**

---

## 11. 给接棒 agent 的 prompt 模板

```
读 claude-all-in-one/HANDOFF-2026-08-06-cursor.md。
目标：让 gw-cursor 的 StreamUnifiedChatWithTools 在本机 pro 账号上 e2e 出字。
禁止重复文档 §4 已排除实验。
优先：要么（A）抓真 IDE 请求逐字节 diff 补「运行时证明」，要么（B）在用户提供 crsr_ key 后切 cli 路径。
验证：cargo test -p gw-cursor -j 4；cargo run -p gw-cursor --example e2e -- "hi"。
```

---

## 12. 开放问题（尚未证实）

1. 推理门校验的「证明」究竟是：动态签名头、HTTP/2/TLS 指纹、BiDi 多帧握手、protobuf 里某能力位、还是设备曾在某 RPC「注册」？
2. `x-cursor-rpc-*` / `x-cursor-agent-transport-mode` 是否仅 DevTools？目前倾向非 chat 必需。
3. IDE chat 流量是否 100% 走 exthost `node:http2`？（决定钩子是否够用）
4. cli 路径与 IDE 路径在模型列表、限流、计费上的差异。
5. session token 刷新协议与防关联（同出口）如何对齐 dario 的 fail-closed 模式。

---

*文档日期：2026-08-06。作者：Cursor agent 会话（Composer）+ kimi 对抗咨询。未部署、未保证可破门。*
