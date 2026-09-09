# GrokBot 0.39.0 协议逆向与网关复刻设计(2026-09-09)

## 背景

旧 BOT 池推理面 `aiserver.v1.InferenceService/Stream` 于 2026-09-09 被服务端锁死
(任何 cursor JWT 都回 `ERROR_NOT_LOGGED_IN`,与 IP/账号无关;同一 JWT 调
DashboardService 正常)。官方 grokbot 桌面端已升级到 0.39.0,推理架构改为
「unary 发消息 + 流式监听 transcript」。本文档是新版协议的逆向结论与网关复刻设计。

逆向素材:`/opt/Grok Bot` 0.39.0 asar 解包(proto.cjs 描述符)+ 本机实弹抓包
(SAND_BACKEND_URL 指向 mitmproxy 反向代理)+ curl 手工复现。

## 端点与鉴权

- 所有 GrokBotService RPC 直连 `https://api2.cursor.sh`,HTTP/1.1 + ConnectRPC。
- unary 方法:`content-type: application/proto`,body = 裸 protobuf。
- server-streaming:`content-type: application/connect+proto`,body = 5 字节信封
  (`[flag:1][len:4 大端]`,flag=0 数据帧 / 0x02 结束帧)包裹 protobuf;响应同格式。
- 鉴权头(与旧面一致):
  - `authorization: Bearer <access_token>`(oauth/token 刷新来的 JWT,client_id
    `KbZUR41cY7W6zRSdpSUJ7I7mLYBKOCmB`,access==refresh 同一枚)
  - `x-cursor-checksum`:base64url(zyg(6 字节大端 `floor(now_ms/1e6)`)) + machineId
    (+ "/" + macMachineId);zyg:`t=165; b=(b^t + i%256) & 255; t=b`
  - `x-cursor-client-type: sand`、`x-cursor-client-version: 0.39.0`、
    `x-sand-box-namespace: prod`、`x-request-id: <uuid>`、`te: trailers`
- 复刻不需要:EnsureSandBox / box 网关(`*.cursorvm.com`)—— 那是 computer-use 通道,
  纯聊天(temporal harness)全程只打 api2。

## 核心 RPC(proto 字段表,逐字段实锤)

### ListGrokBotAgents(Unary)
- req: `{1 role string opt, 2 include_team_agents bool opt}`(空消息即可)
- resp: `{1 agents repeated GrokBotAgent, 2 slack_connections repeated}`
- GrokBotAgent:`{1 id, 2 legacy_agent_id, 3 name, 4 description, 5 title, ..., 
  12 agent_id string, 13 harness string("temporal"/"box"), 19 kind enum(1 AGENT/2 ROOM), ...}`
- **无 model 字段**。客户端过滤 `harness=="temporal"` 的当聊天 agent。

### CreateGrokBotTemporalAgent(Unary,与 CreateGrokBotAgent 同请求类型)
- req: `{1 legacy_agent_id, 2 name, 3 description, 4 title, 5 avatar_shape, 6 avatar_color,
  8 agent_id(=客户端生成 uuid), 9 harness enum(1 BOX / 2 TEMPORAL),
  11 introduction_suppressed bool opt}`
- resp: `{1 agent GrokBotAgent, 2 harness enum}`;客户端校验 `agent.agent_id==请求的 uuid`。

### SendGrokBotUserMessage(Unary)— 发消息
- req: `{1 agent_id, 2 message_id(幂等键,uuid hex), 3 text, 4 sent_at_ms int64,
  5 rich_text opt, 6 reply_to_id opt, 7 is_fork bool,
  8 attachment_paths repeated string, 9 attachment_names repeated string,
  13 source enum(1=DESKTOP), 14 mcp_config_json opt, 15 session_id}`
- resp: `{1 dispatched bool, 2 mode enum, 3 workflow_id opt, 4 delivery enum
  (1 ACCEPTED_BOX / 2 ACCEPTED_TEMPORAL / 3 DUPLICATE / 4 REFUSED), 5 refusal msg opt
  {1 failure_code, 2 message}}`
- **session_id 是多轮关键**:同一会话续传同一个 session_id;新会话不带,服务端生成
  (echo 行和 watch rows 帧里都带)。

### GetGrokBotSendStatus(Unary)— 送达轮询/重发判定
- req `{1 agent_id, 2 message_id, 3 session_id}`;resp `{1 status enum
  (1 NOT_FOUND / 2 ACCEPTED / 3 REJECTED / 4 PENDING / 5 UNKNOWN_DURABILITY),
  3 rejection_code opt}`。NOT_FOUND 时官方客户端原样重发一次。

### WatchGrokBotTranscripts(ServerStreaming)— 收回包
- req: `{1 cursors repeated GrokBotTranscriptCursor, 3 inline_body_max_bytes uint32}`
  - cursor:`{1 agent_id, 2 generation uint32, 3 after_updated_seq uint64, 4 session_id}`
- 事件帧 oneof:
  1. `connected {stream_id, server_time_ms, absolute_lifetime_ms}`(寿命到期要重连)
  2. `rows {agent_id, generation, entries repeated, deletes repeated, replay bool, session_id}`
  3. `cleared`、`cursor_too_old`(→ 用 ListGrokBotTranscriptEntries 权威重拉)、
     `heartbeat`、`agent_state {live repeated GrokBotAgentLiveState, snapshot bool}` 等
- entry(GrokBotTranscriptEntry):`{1 seq u64, 2 entry_kind string, 3 body bytes opt,
  4 blob_hash opt, 5 updated_seq u64, 6 entry_id opt, 7 body_omitted bool}`
- **body 是 JSON 文本**,kind ∈ `send-message | message | user-attachment | tool-call |
  notice | event | feedback`:
  - `message`: assistant 文本;**流式=同一 entry 反复覆写**(`isStreaming:true`,seq 不变
    updated_seq 递增),`isStreaming:false` 即定稿。`role:"user"` 是用户消息回显。
  - `send-message`: 用户消息的服务端回执回声(含 `clientNonce`=我们的 message_id,
    `requestId`=本轮运行 id,`batchId`)。
  - `tool-call`: agent 自己的工具调用(name/status/summary),与我们客户端工具无关。
  - `user-attachment`: `{file_path, file_name}`。
  - `spend-initiation` 行:计费事件标记(无 token 数)。
- **没有 token/usage 统计字段**。token 计数在旧 agent.v1 通道,0.39 不走那条。

### 附件上传(宿主 IPC,非 ConnectRPC)
桌面端:renderer 分块 → 宿主进程写 agent 文件系统 → 返回 committedPath 填进
attachment_paths。**网关侧附件方案待定**(见「设计」);PromptUpload
(BackgroundComposerService Presign/PUT/Complete)实测可传 S3 但 agent 把
attachment_path 当用户电脑路径读,此路不通。

### InterruptGrokBotAgentRun(Unary)
req `{1 agent_id, 2 reason, 3 session_id opt}` — 客户端断开时喊停,避免空烧额度。

## 2026-09-09 下午补实锤(二次实验,全部本机直连)

1. **InferenceService/Stream 是对所有号退役**:健康 Ultra 号的 token 打它也回
   `ERROR_NOT_LOGGED_IN`;同 token 打 GrokBotService 200。→ 旧面永久死,与账号无关。
2. **session_id 只有 temporal agent 支持**:box agent 带 session_id 发送 → REFUSED
   `sessions_unsupported_on_box / Non-default sessions require a Temporal-hosted agent`。
   box agent 的多轮连续性是**隐式默认会话**(实测跨消息记得住数字)。
3. **temporal 不是每个号都能建**:Ultra 号 CreateGrokBotTemporalAgent → 400
   `The Temporal Grok Bot harness is not available`;该号 GetGrokBotRuntimeCapabilities
   里 temporal_creation_enabled 缺席。→ 网关要两级降级:能建 temporal 就用
   `session_id=caio-<会话指纹>` 多会话并行;建不了就 box agent 隐式会话 + 账号内串行。
4. **刷新风暴疑杀 RT**:bot7-3 一小时内被我们(生产重试 + 手动探针)反复用同一 RT
   刷新,最终 `shouldLogout:true` 终态死亡。疑似 RT 轮换重用检测(或高频刷新风控)。
   grokbot1 同样 shouldLogout。**纪律:每账号内存里缓存新 token,10 分钟内绝不二次
   刷新;shouldLogout 即终态。**
5. **api2 建的 box agent 不能跑**:CreateGrokBotAgent(harness=BOX)能建成(200),
   但发消息持续 503 `unavailable/isRetryable`(EnsureSandBox、kickstart_requested
   都试过,无效)——能跑的 box agent 是官方 app「宿主中介创建」+ 接着用户电脑的
   local-exec 通道的那只。**网关唯一可行路径是 temporal agent**;temporal 能力是否
   对新号开放,必须拿活号实测(GetGrokBotRuntimeCapabilities 第 3 项)。
   若新号也没有 temporal → 退路是 box 网关路径(EnsureSandBox → gateway_url →
   `POST /api/sendPrompt` + `GET /events`,工程量翻倍,见子代理报告 §1b)。

## 模型

**协议层无模型选择**(send/create/agent 均无 model 字段),官方用户也不能选。
实测问 agent 自述:底层是 Grok(版本不暴露)。→ 复刻后 BOT 渠道 = 纯 grok 渠道,
claude 走 grokbot 额度的旧路(靠死端点指定模型名)永久消失。

## 网关复刻设计(gw-cursor 新驱动 sandchat)

### 账号侧
- 每账号惰性确保一个 temporal agent:extra 缓存 `sand_agent_id`;没有就
  ListGrokBotAgents 挑 `harness=="temporal"` 的,再没有就 CreateGrokBotTemporalAgent
  (name="caio-relay",introduction_suppressed=true)。
- 会话绑定:客户端会话指纹 → (agent_id, session_id, last_updated_seq)。
  一期可以单 agent 顺序服务;并发会话绑同一 agent 时串行排队(agent 是单对话),
  或者每会话独立 agent(临时 agent 创建便宜;后续观察 roster 膨胀再定清理)。

### 请求流
1. 会话首包:渲染完整历史(system + 消息)+ TextEmu 工具契约(复用现有
   tools_text_block / tail reminder 文本);续包只发增量。
2. SendGrokBotUserMessage;delivery REFUSED → 上抛 REFUSED 错误。
3. 开 WatchGrokBotTranscripts(cursor 带 agent_id/session_id/after_updated_seq):
   - 先等到我们消息的回声(clientNonce 匹配)→ 记录 requestId;
   - 收集该 requestId 的 assistant `message` 行:同 seq 覆写做 diff → Anthropic
     text_delta 逐块下发(保首字体验);
   - 全文本过 TextToolFilter:`<tool_call>` 块转 tool_use(复用 inference.rs 整套
     折叠/窄修复/未声明拦截);
   - 结束条件:assistant message `isStreaming:false` 定稿 + agent_state 不在跑,
     或 idle 超时兜底。
4. 客户端中断 → InterruptGrokBotAgentRun。
5. 用量:transcript 无 token 数 —— 按字符估算(输入渲染文本长 + 输出文本长),
   入账标记为估算值;真实额度由后台 GetSandUsageStatus 周期核对。

### 计费/额度口径
- 烧的是 Bot 周池(GetSandUsageStatus 的 usage_percent),与老 inference 面同池。
- request_logs 的 input/output tokens 为估算,UI 上加标记或容忍偏差(待讨论)。

### 待解(二期)
- 图片/PDF 附件:等官方粘贴态的 committedPath 格式实锤后复刻上传链。
- thinking:grok 的思考是否出现在 transcript(观察 `event`/`notice` 行)。
- 多客户端会话并发同号的排队策略与 agent 池化。

## 实测记录(2026-09-09 本机,Ultra 号)

- ListGrokBotAgents 空号回 200 空体;Ultra 号回 214B 含 agent
  `803915ac-7cf0-4ef2-abb4-03d65cba98aa`。
- 手工构造 send → 200(mode=1 delivery=1);watch 先收 connected(stream_id),
  随后 rows 携带 assistant 回复全文。
- watch 请求必须 `application/connect+proto` + 信封(裸 proto 得 415)。
- PresignPromptUpload/PUT/Complete 全通(BackgroundComposerService),但 grokbot
  agent 把 attachment_path 当**用户电脑路径**(经 local-exec 通道读),S3 upload_id
  不被识别。
