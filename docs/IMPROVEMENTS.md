# kiro-gw 实现参考表(IMPROVEMENTS)

> 全新写 kiro-gw 时,**每个能力点参考哪份代码**的可追溯清单。不是"重构旧项目",是"从零写新项目,有据可依"。
> 来源:三参考项目(static_flow / xkiro.rs / KiroManager)+ 旧 kiro.rs 搬运资产。
> 生成:2026-06-05,7 个 sonnet 子代理逐文件核对行号(详见 §附录原始片段)。

## 怎么用这份文档

- 写某个 crate/模块前,翻到对应章节,照着"参考来源(文件:行号)"打开源码看实现。
- 每条标了 **类型**(🔵搬运 / 🟢借鉴 / 🟣自创)、**Phase**、**confidence**。
- 防封是命根,单独抽 §2 防封专章横切。
- 原始逐条核对表保留在 `docs/_refmatrix_*.md`(7 份),本文档是消化后的导航版;需要精确行号去查那 7 份。

## 来源图例

| 标记 | 含义 | 取用方式 |
|---|---|---|
| 🔵搬运 | 旧 kiro.rs 已生产验证 | **搬运不重写**,只调整封装适配新 crate 边界 |
| 🟢借鉴 | static_flow / xkiro 的成熟实现 | 参考其设计与边界,用自己的代码实现(注意 license) |
| 🟣自创 | 三参考都没有 | 全新设计(多进程/egress 绑定/router) |

## 总览:能力点分布

| 来源 | 能力点数 | 最高价值块 |
|---|---|---|
| static_flow converter 簇 | ~84 | 归一化/校验/tool配对/conversationId锚定/身份防穿帮 |
| static_flow stream+parser 簇 | ~53 | thinking签名/inline thinking/SSE状态机/空响应兜底 |
| static_flow cache+sched 簇 | ~71 | 双锚定/prefix_tree/调度亲和/封号检测 |
| static_flow runtime+headers 簇 | ~84 | 发包指纹/UA含machineId/refresh分流/代理一致性/错误分类 |
| xkiro.rs | ~57 | tool_compression/compressor/prompt_filter/cache_tracker |
| KiroManager(激活端) | ~39 | machineId来源/TLS指纹/激活时序/发包UA对齐(防封基准) |
| 旧 kiro.rs(搬运) | ~43 | eventstream解析/签名v42/计费v53/fallback v58/affinity v52 |

## ⚠️ 三个关键判断(影响架构决策)

1. **三参考项目都不做 local_address 源IP绑定**(static_flow `provider/client.rs:14` 实锤、xkiro `http_client.rs:46` 同样、KiroManager 靠 TLS+代理)。kiro-gw 的「多进程 egress 绑本机IP」是 🟣**全新自创**,没有现成参考,Phase 3 要自己实测 reqwest `local_address` 对 Kiro 上游(纯IPv4)是否生效。

2. **xkiro 的 `fingerprint.rs` 是半成品/未接线**(确认:`kiro/endpoint/ide.rs:37` 真实路径直接手拼 UA,从不调用 `Fingerprint::generate_from_seed`)。**别照搬整文件**,只借鉴"确定性多字段画像"思路,且必须真接进 header 装配路径。

3. **machineId 必须跨"激活/刷新/发包"一致**(KiroManager `machineId.ts:91` 读OS真值 + `proxy/kiroApi.ts:170` 嵌UA;static_flow `kiro_headers.rs:85` 嵌UA)。这是 ¥900 封号根因。kiro-gw 的 machineId 是头等防封资产,§2 详述。

---

# §1 按 kiro-gw 模块组织的实现参考

每个模块列出:该实现什么能力、参考哪份源码、类型、Phase。精确行号见 §附录的 7 份原始片段。

## 1.1 `gw-core`(契约层)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| UpstreamError 统一错误枚举(RetryNext/Fatal/QuotaExhausted/RateLimited) | static_flow `provider/kiro_error.rs:8`、`kiro_dispatch.rs:1493` | 🟢借鉴 | P1 |
| 状态码→错误分类映射(402→Quota、429→RateLimited、400 transient→proxy冷却) | static_flow `kiro_dispatch.rs:1258-1270` | 🟢借鉴 | P1 |
| ProxyConfig 值对象(url+auth) | xkiro `http_client.rs:12`、static_flow `provider/client.rs` | 🟢借鉴 | P1 |
| runtime/management 上游基址分离 + env 覆盖 | static_flow `kiro_refresh.rs:504-536` | 🟢借鉴 | P1 |
| Anthropic usage JSON 形状(input/output/cache_creation/cache_read) | static_flow `anthropic/stream/usage.rs:22` | 🟢借鉴 | P1 |
| 内置工具必填字段契约表(Write/Edit/Read/Bash) | xkiro `anthropic/truncation.rs:177` | 🟢借鉴 | P2 |
| Grapheme 安全截断工具 | xkiro `anthropic/compressor.rs:733` | 🟢借鉴 | P1 |
| SDK version 常量按链路分离(不同版本本身是指纹) | static_flow `kiro_protocol.rs:5`、`kiro_refresh.rs:28-30` | 🟢借鉴 | P1 |

## 1.2 `gw-kiro/converter`(报文转换 — kiro-gw 心脏)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| Anthropic Messages→Kiro ConversationState 主装配 | 🔵旧 `anthropic/converter.rs:475-687`;🟢static_flow `converter/convert.rs:147-221` | 🔵搬运+🟢借鉴 | P1 |
| 当前轮范围锚定(回溯尾部连续 user turns) | static_flow `converter/convert.rs:42-64` | 🟢借鉴 | P1 |
| 报文合法化硬校验(user/assistant 允许的 block 类型) | static_flow `converter/validate.rs` | 🟢借鉴 | P1 |
| system 三级分流(稳定promote/动态drop/未知preserve as user) | static_flow `converter/normalize.rs:211-390` | 🟢借鉴 | P1 |
| tool 配对清理链(orphan prune + dup tool_use_id rewrite) | 🔵旧 `converter.rs:907-1073`;🟢static_flow `converter/tool_pairing.rs`、`tool_name.rs` | 🔵搬运+🟢借鉴 | P1 |
| tool 名 sanitize+hash alias(兼容 plugin:skill 长名) | 🔵旧 `converter.rs:1075-1161`;🟢static_flow `converter/tool_name.rs` | 🔵搬运 | P1/P2 |
| 历史工具补 placeholder tool spec(防 400) | 🔵旧 `converter.rs:447-471`;🟢static_flow `converter/convert.rs:188-198` | 🔵搬运 | P1 |
| JSON schema 归一化(null/脏字段修正 + key 排序去抖) | 🔵旧 `converter.rs:98-141`;🟢static_flow `converter/tools.rs` | 🔵搬运 | P2 |
| 空content 400 兜底(媒体补引导语/全空补空格/纯tool_result保持空) — current+历史user+历史assistant 三处 | 🔵旧 `converter.rs:598-619,1082-1097,1326-1515` | 🔵搬运 | P1 |
| message content 归一化抽取(text/image/document/tool_result+截图) | 🔵旧 `converter.rs:696-893` | 🔵搬运 | P1 |
| 历史构建与 system 折叠(Kiro 无独立 system 字段) | 🔵旧 `converter.rs:1207-1323`;🟢static_flow `converter/convert.rs:299-349` | 🔵搬运 | P1 |
| tool 定义超阈值压缩(>20KB 瘦schema+截断desc) | 🟢xkiro `anthropic/tool_compression.rs:17,96`(已接线 `converter.rs:668`) | 🟢借鉴 | P2 |
| document/image 处理(base64+去重+空文档placeholder) | static_flow `converter/document.rs`、`image.rs`、`convert.rs:423-460` | 🟢借鉴 | P2 |
| developer role→top-level system(兼容 OpenAI 风格客户端) | static_flow `converter/normalize.rs:282-385` | 🟢借鉴 | P2 |

## 1.3 `gw-kiro/cache_sim`(缓存命中 — 你自评最强,对标 static_flow)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| conversationId 稳定派生(前两条user+system anchor 哈希,剥 rolling 指纹) | 🔵旧 `converter.rs:353-399,286-329` | 🔵搬运 | P1 |
| agentContinuationId 稳定派生(Kiro 后端也看 continuation id) | 🔵旧 `converter.rs:408-426` | 🔵搬运 | P1 |
| **lookup_anchor(只覆盖已存在历史,不混当前轮)** | 🟢static_flow `cache_sim/projection.rs:67-103` | 🟢借鉴(我可能没做) | P1 |
| **resume_anchor(历史+当前轮+assistant响应,成功后写,下轮恢复)** | 🟢static_flow `cache_sim/projection.rs:105-116` | 🟢借鉴(我可能没做) | P1 |
| prefix_tree 共享前缀缓存(64token/页只算整页,TTL+budget淘汰) | 🟢static_flow `cache_sim/prefix_tree.rs` | 🟢借鉴 | P2 |
| anchor_index(anchor→conversationId 的 TTL+LRU 恢复表) | 🟢static_flow `cache_sim/anchor_index.rs:50-127` | 🟢借鉴 | P2 |
| stable prefix 纳入 tools 定义、但 resume anchor 不纳入 | 🟢static_flow `cache_sim/projection.rs:72-88,491-523` | 🟢借鉴 | P2 |
| cache_sim 前缀命中纯函数(最长公共前缀→命中token) | 🔵旧 `kiro/cache_sim.rs:95-104` | 🔵搬运 | P1 |
| cache_sim 状态表 observe(TTL/LRU,跨轮指纹) | 🔵旧 `kiro/cache_sim.rs:127-250` | 🔵搬运 | P1 |
| 从 ConversationState 提取稳定指纹(忽略tools/容器差异,解决v53崩盘) | 🔵旧 `kiro/cache_sim.rs:271-428` | 🔵搬运 | P1 |
| billing header 归一化为占位符(避免版本漂移破坏 fingerprint) | 🟢static_flow `converter/system.rs:60-63`;xkiro `cache_tracker.rs:366` | 🟢借鉴 | P1 |
| JSON canonicalize + 最小可缓存阈值(1024/2048/4096 按模型) | 🟢xkiro `anthropic/cache_tracker.rs:490-509` | 🟢借鉴 | P2 |
| request input token 本地估算(compaction/usage 前置) | static_flow `kiro_dispatch.rs:227` | 🟢借鉴 | P2 |

## 1.4 `gw-kiro/stream` + `gw-kiro/parser`(流处理 + eventstream 解析)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| AWS eventstream 帧/header/CRC/decoder 状态机(抗分片/半包/坏包) | 🔵旧 `kiro/parser/{frame,header,crc,decoder}.rs` | 🔵搬运 | P1 |
| Kiro 事件→高层语义桥(AssistantResponse/Reasoning/ToolUse/Metering/Exception) | 🔵旧 `handlers.rs:1024-1111`;🟢static_flow `stream/context.rs:390` | 🔵搬运+🟢借鉴 | P1 |
| SSE 协议状态机(message/block 的 start/delta/stop 时序约束) | 🔵旧 `stream.rs:279-538`;🟢static_flow `stream/state.rs:50-166` | 🔵搬运+🟢借鉴 | P1 |
| thinking 签名 protobuf 重写(只换模型代号,保真) | 🔵旧 `anthropic/signature.rs:65-152` | 🔵搬运 | P1 |
| thinking 签名合成兜底(上游不发时合成合法签名) | 🔵旧 `signature.rs:171-241`;🟢static_flow `stream/signature.rs:284-293` | 🔵搬运+🟢借鉴 | P1/P2 |
| 原生 reasoningContentEvent→Anthropic thinking 块(签名先到/正文后到) | 🔵旧 `stream.rs:916-1015`;🟢static_flow `stream/context.rs:467` | 🔵搬运+🟢借鉴 | P1 |
| inline `<thinking>` 流式解析(跨chunk半标签/跳过引用伪标签) | 🔵旧 `stream.rs:68-171,1018-1154`;🟢static_flow `stream/inline_thinking.rs` | 🔵搬运+🟢借鉴 | P1 |
| thinking block 收尾时序(thinking_delta→signature_delta→stop) | 🟢static_flow `stream/context.rs:693-697` | 🟢借鉴 | P1 |
| tool_use 增量 JSON 聚合(input_json_delta,stop时反序列化) | 🟢static_flow `stream/context.rs:729` | 🟢借鉴 | P1 |
| empty-response buffered fallback(首包empty→非流式重发→转SSE) | 🔵旧 `handlers.rs:598-966` | 🔵搬运 | P1 |
| 空流首帧探测+延迟首事件(缓冲到首完整frame再出字节) | 🟢static_flow `kiro_dispatch.rs:1080-1127` | 🟢借鉴 | P1 |
| 空流透明重试(退避200ms*attempt,同号) | 🟢static_flow `kiro_dispatch.rs:1127` | 🟢借鉴 | P1 |
| buffered 非流式统一解析器(text/reasoning/tool_use/metering→IR) | 🔵旧 `handlers.rs:994-1171` | 🔵搬运 | P1 |
| buffered结果→Anthropic SSE 重放 | 🔵旧 `handlers.rs:1180-1288` | 🔵搬运 | P2 |
| 流终态失败分类(UpstreamError/Exception/EmptyResponse/StreamIo) | 🔵旧 `stream.rs:612-647` | 🔵搬运 | P2 |
| input token 阈值判定(小请求信本地估算,大请求信contextUsage) | 🟢static_flow `stream/usage.rs:18-52` | 🟢借鉴 | P1 |
| 空响应兜底(只thinking无内容→补空格text+max_tokens) | 🟢static_flow `stream/context.rs:870`;🔵旧 `stream.rs` | 🔵搬运+🟢借鉴 | P1 |
| tool call 截断检测→软失败(空输入/未闭合/括号不平衡/缺必填) | 🟢xkiro `anthropic/truncation.rs:61-138` | 🟢借鉴 | P1 |
| bracket 风格工具调用回退解析([Called X with args:{}]) | 🟢xkiro `anthropic/bracket_tool_parser.rs:75` | 🟢借鉴 | P2 |
| structured output tool→文本(不暴露tool_use,聚合JSON转text) | 🟢static_flow `stream/context.rs:730-749` | 🟢借鉴 | P2 |

## 1.5 `gw-kiro/headers` + `gw-kiro/machine_id`(发包指纹 — 见 §2 防封)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| 统一 header 注入入口(避免各路径手搓漂移) | 🟢static_flow `kiro_headers.rs:41` | 🟢借鉴 | P1 |
| UA/x-amz-user-agent 双轨构造(machineId 嵌入末尾) | 🟢static_flow `kiro_headers.rs:85`;KiroManager `kiroApi.ts:170` | 🟢借鉴 | P1 |
| 发包 header 完整清单(invocation-id随机/sdk-request重试语义/optout) | 🟢static_flow `kiro_headers.rs:50` | 🟢借鉴 | P1 |
| generate vs MCP vs usage header 模板不同(不能复用) | 🟢static_flow `kiro_protocol.rs:15-57` | 🟢借鉴 | P1 |
| machineId 派生(凭据级>全局>API Key/refreshToken派生>兜底) | 🔵旧 `kiro/machine_id.rs:24-109` | 🔵搬运 | P1 |
| social vs IdC 的 UA/agent-mode 分流 | 🟢static_flow `kiro_headers.rs`;KiroManager `kiroApi.ts:1162` | 🟢借鉴 | P1 |
| external_idp 特殊 TokenType 头、internal redirect 头 | 🟢static_flow `kiro_headers.rs:65-68` | 🟢借鉴 | P1 |
| host 头从 URL 解析(不手写常量) | 🟢static_flow `kiro_refresh.rs:518` | 🟢借鉴 | P1 |

## 1.6 `gw-kiro/token`(刷新 + profileArn)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| ensure_call_context 统一入口(发包前最后一层拉auth/必要时刷新) | 🟢static_flow `kiro_refresh.rs:73-87`、`kiro_dispatch.rs:1214` | 🟢借鉴 | P1 |
| refresh 串行锁(每账号一个async mutex,防刷新风暴) | 🟢static_flow `kiro_refresh.rs:119,796` | 🟢借鉴 | P1 |
| refresh 前判断快过期(expires_at或JWT exp,提前10min) | 🟢static_flow `kiro_refresh.rs:589` | 🟢借鉴 | P1 |
| social vs IDC 刷新分流(不同endpoint+不同UA指纹) | 🟢static_flow `kiro_refresh.rs:607,635,688` | 🟢借鉴 | P1 |
| invalid_grant→永久禁用(区分可重试失败 vs 永久失效) | 🟢static_flow `kiro_refresh.rs:135,790` | 🟢借鉴 | P1 |
| 成功刷新后 merge 持久化(不覆盖其他元数据) | 🟢static_flow `kiro_refresh.rs:410,543` | 🟢借鉴 | P1 |
| profileArn 处理(固定快速路径 + 动态回源查询 + body/header/query 三通道) | 🟢static_flow `kiro_refresh.rs:188-358`、`kiro_dispatch.rs:1467`;KiroManager `kiroApi.ts:206` | 🟢借鉴 | P1 |
| 401/403→force_refresh一次再重试同号(减少无谓切号) | 🟢static_flow `kiro_dispatch.rs:1279` | 🟢借鉴 | P1 |
| 后台刷新器(独立task,配置校验/批处理/优雅停止) | 🟢xkiro `kiro/background_refresh.rs:48-209` | 🟢借鉴 | P2 |
| token 文件格式兼容(~/.aws/sso/cache,IDE自刷新单一真源) | KiroManager `kiroAuthSync.ts:103-232` | 🟢借鉴 | P1 |

## 1.7 `gw-kiro/scheduler`(组内账号调度 — 多进程下只管本worker那组)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| 会话亲和(session→account,落号即认不回弹) | 🔵旧 `token_manager.rs:816-1211`;🟢xkiro `kiro/affinity.rs:33-68` | 🔵搬运+🟢借鉴 | P1 |
| 独立 affinity 模块(从token_manager剥出,TTL+LRU) | 🟢xkiro `kiro/affinity.rs`(已接线 `token_manager.rs:668`) | 🟢借鉴 | P1 |
| 通用选号器(priority / balanced 模式) | 🔵旧 `token_manager.rs:920-979` | 🔵搬运 | P2 |
| 新会话按绑定数均衡分配 | 🟢static_flow `kiro_dispatch.rs:418` | 🟢借鉴 | P2 |
| 空响应阈值冷却(固定窗口累计,达阈值才冷却,不每次伤号) | 🔵旧 `token_manager.rs:1690-1759` | 🔵搬运 | P1 |
| 429 冷却 + 自动自愈恢复 | 🔵旧 `token_manager.rs:1633-1677`;🟢static_flow `kiro_dispatch.rs:1514` | 🔵搬运+🟢借鉴 | P2 |
| QuotaExhausted 按 routing_identity 整簇封禁(同身份多号共享配额) | 🟢static_flow `kiro_dispatch.rs:1501-1537` | 🟢借鉴 | P1 |
| 账号并发 permit + 起始间隔限制 | 🔵旧 `token_manager.rs:409-446`;🟢static_flow `kiro_dispatch.rs:400` | 🔵搬运+🟢借鉴 | P1 |
| 延迟感知路由(account+proxy 双维打分/band/平滑) | 🟢static_flow `kiro_latency.rs:14-209` | 🟢借鉴 | P3 |
| token/refresh 失败分类与全禁用自愈 | 🔵旧 `token_manager.rs:1765-1819` | 🔵搬运 | P3 |

## 1.8 `gw-kiro/billing`(计费)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| cache_read 统一上报公式(hit/sim_total比例→report_total,floor/cap夹限) | 🔵旧 `anthropic/usage.rs:55-122` | 🔵搬运 | P1 |
| 流式 usage/cache_sim 接线(倍率参数热调) | 🔵旧 `stream.rs:691-709` | 🔵搬运 | P2 |
| Kiro cached/uncached→Anthropic usage 字段回填 | 🟢static_flow `kiro_usage.rs:72` | 🟢借鉴 | P2 |
| credit metering 汇总(meteringEvent) | 🟢static_flow `stream/context.rs:233` | 🟢借鉴 | P2 |
| 计费倍率表 | 🟢static_flow `billable_multipliers.rs` | 🟢借鉴 | P3 |

## 1.9 `gw-store`(SQLite 持久化)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| 账号状态机持久化(ready/degraded/error/disabled + last_checked/success) | 🟢static_flow `kiro_status.rs:132-181` | 🟢借鉴 | P2/P4 |
| 余额视图(current_usage/limit/remaining/next_reset/subscription) | 🟢static_flow `kiro_status.rs:203`;xkiro `web_portal.rs:399` | 🟢借鉴 | P4 |
| auth JSON merge 持久化(刷新只更新部分字段) | 🟢static_flow `kiro_refresh.rs:410` | 🟢借鉴 | P1 |
| 零缓存/错误请求抓取 full request(debug审计) | 🟢static_flow `kiro_usage.rs:218` | 🟢借鉴 | P3 |
| token 文件格式(~/.aws/sso/cache 兼容 IDE) | KiroManager `kiroAuthSync.ts:116` | 🟢借鉴 | P1 |

## 1.10 `gw-app`(二进制:router/worker/admin/egress — 多进程)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| **多进程 router/worker + egress 绑本机IP** | 🟣无参考(三项目都不做源IP绑定) | 🟣自创 | P3 |
| HTTP client 构造(proxy cache/连接池/keepalive) | 🟢static_flow `provider/client.rs:14`;xkiro `http_client.rs:46` | 🟢借鉴 | P1 |
| **代理一致性(refresh/usage/generate/MCP 全走同一 route.proxy)** | 🟢static_flow `kiro_dispatch.rs:1435`、`kiro_refresh.rs:282-705`(同出口铁律) | 🟢借鉴 | P1 |
| proxy 级冷却(某些400是出口问题,冷却proxy而非账号) | 🟢static_flow `kiro_dispatch.rs:1270,1514` | 🟢借鉴 | P3 |
| messages 路径白名单归一(/v1/messages,/cc/v1/messages) | 🟢static_flow `kiro_protocol.rs:8` | 🟢借鉴 | P1 |
| 账号状态定时刷新 worker(warmup+周期+jitter,非请求时探测) | 🟢static_flow `kiro_status.rs:17-52`、`next_kiro_account_jitter:235` | 🟢借鉴 | P2/P4 |
| request_id/trace_id 中间件 + access log | 🟢static_flow `request_context.rs:14-52` | 🟢借鉴 | P4 |
| Kiro错误→Anthropic error 脱敏(对外保持Anthropic IR) | 🟢static_flow `kiro_error.rs:74-168` | 🟢借鉴 | P2 |
| 用户可配 prompt 过滤规则(admin) | 🟢xkiro `prompt_filter.rs:262` | 🟢借鉴 | P3 |
| 账户额度聚合(admin面板,web portal CBOR) | 🟢xkiro `kiro/web_portal.rs:141-399` | 🟢借鉴 | P4 |
| 远程媒体URL拉取+SSRF防护(可选key开关) | 🟢static_flow `kiro_media.rs:103-583` | 🟢借鉴 | later |
| 本地 web_search 短路(server_tool_use 仿真) | 🟢xkiro `anthropic/websearch.rs:143-645` | 🟢借鉴 | P4 |

## 1.11 `gw-kiro/prompt_filter`(可选,system 净化)

| 能力 | 参考来源 | 类型 | Phase |
|---|---|---|---|
| system 噪音清洗(env段/billing header/gitStatus/auto memory) | 🟢xkiro `prompt_filter.rs:95`(已接线 `converter.rs:1439`) | 🟢借鉴 | P1 |
| Claude Code prompt 识别+精简替换 | 🟢xkiro `prompt_filter.rs:70` | 🟢借鉴 | P2 |
| 限制段剥离(content_safety/git_safety,风险高需开关) | 🟢xkiro `prompt_filter.rs:237` | 🟢借鉴 | P4(慎) |

<!-- S1_END -->

---

# §2 防封专章(横切,命根)

封号 = 风控发现"激活时的指纹 ≠ 发包时的指纹",或"一堆号共用同一指纹"。kiro-gw 要在 4 个指纹维度上做到「激活=发包一致 + 账号间隔离」。

## 2.1 四个指纹维度 × 三个参考的现状

| 维度 | KiroManager(激活端真值) | static_flow(发包端) | 旧 kiro.rs | kiro-gw 目标 |
|---|---|---|---|---|
| **machineId** | 读 OS 真值(`machineId.ts:91`),嵌 UA | refreshToken 派生,嵌 UA(`kiro_headers.rs:85`) | refreshToken 派生(`machine_id.rs`) | 派生确定性 + 跨激活/刷新/发包一致;**理想是导入激活时的真值** |
| **出口 IP** | 走代理/TLS出口 | route.proxy 统一出口 | 全裸奔同 IP | 🟣多进程 egress:一 worker 一 IP,一组号钉一 IP |
| **TLS 指纹** | chrome_146(`tlsClientPool.ts`) | 裸 reqwest(不做) | 裸 reqwest | 🟡暂不做(三参考里只有激活端做);记为 later 风险项 |
| **UA/认证流** | social/IdC 分流(`kiroApi.ts:1162`) | social/IdC 分流(`kiro_headers.rs`) | 部分 | social/IdC 严格分流 + UA 含正确 machineId |

## 2.2 machineId 一致性(¥900 封号根因,最高优先级)

- **事实**:machineId 嵌在 `x-amz-user-agent` = `aws-sdk-js/... KiroIDE-{ver}-{machineId}`。激活端(KiroManager)用 OS 真值,发包端若用别的值 → 双指纹漂移 → 秒封。
- **KiroManager 怎么来**:`machineId.ts:91 getCurrentMachineId` → Windows 读 `MachineGuid`、macOS 读 override/Kiro文件回退硬件UUID、Linux 读 `/etc/machine-id`。还会**写回**覆盖文件(`setMacOSMachineId:424`)。
- **kiro-gw 对策**:
  1. machineId 作为账号的**持久化字段**(gw-store),导入时若有真值就存真值,没有才 refreshToken 派生(`machine_id.rs:24-109` 搬运)。
  2. 单一事实来源:`Provider::machine_identity()`(gw-core 已定义)产出,header 装配只从这里取,杜绝散落手拼(static_flow `kiro_headers.rs:46` 的做法)。
  3. social refresh 带 machineId、IDC refresh 不带(static_flow `kiro_refresh.rs:84` 的细节差异,容易漏)。

## 2.3 出口 IP 一致性(多进程的核心价值)

- **事实**:三参考都靠"统一 route.proxy"保证 refresh/usage/generate/MCP 同出口(static_flow `kiro_dispatch.rs:1435`+`kiro_refresh.rs:282-705`),**没人做本机源IP绑定**。
- **kiro-gw 🟣自创**:多进程,一 worker 绑一个固定出口(egress local_ip 或 proxy),一组号钉死在一个 worker → 同号永远同 IP。**激活=发包同 IP** 靠"未来在服务器IP上激活"或"固定IP代理池"达成。
- **必须保证**(从 static_flow 学的铁律):worker 内 refresh / usage 查询 / 发包 / 状态刷新 **全部走本 worker 的出口**,绝不串。
- **Phase 3 待实测**:reqwest `local_address` 绑定对 Kiro 上游(纯IPv4,无AAAA)是否生效。

## 2.4 UA / 认证流分流(易漏点清单)

照 static_flow + KiroManager 核对,这些是"看起来对、实则穿帮"的坑:
- generate / MCP / usage / profile 查询 **header 模板各不同**,不能复用(static_flow `kiro_protocol.rs:15-57`)。
- social UA = 桌面端 `KiroIDE-{ver}-{machineId}`;IDC UA = `aws-sdk-js/3.980.0` 风格(`kiro_refresh.rs:806-814`)。
- agent-mode:social/IDE=`spec`,IdC/CLI=`vibe`(KiroManager `kiroApi.ts:1162`)。
- profileArn:BuilderId 占位ARN在流式端点会 403 被剥离;走 body(generate)/header(MCP)/query(usage)三个不同位置(`kiro_dispatch.rs:1467`、`kiro_protocol.rs:57`、`kiro_refresh.rs:454`)。
- SDK version 常量按链路分离——版本本身是指纹(`kiro_protocol.rs:5` vs `kiro_refresh.rs:28-30`)。

## 2.5 TLS 指纹(已知缺口,记 later)

- KiroManager 激活端用 `tlsclientwrapper` 的 chrome_146(`tlsClientPool.ts:29`)。static_flow 和旧 kiro.rs 都是裸 reqwest TLS。
- **判断**:激活端是 Chrome TLS、发包端是 Rust rustls TLS——理论上是个 JA3 差异点。但 static_flow 生产可用证明"发包端不模拟 TLS 也能跑",说明 Kiro 风控当前没把 TLS 指纹卡死。
- **kiro-gw 策略**:Phase 不做,记为 later 风险项。若未来封号压力大,再评估 rustls 定制 ClientHello 或上游 TLS 代理。

## 2.6 激活端时序(未来自建服务端激活的蓝本)

KiroManager `registrar.ts:1518 run` 是完整 BuilderID 激活时序(OIDC→Device→Email→Portal→Signup→OTP→CreateIdentity→SetPassword→SSO→Token→VerifyAlive,每步带加密浏览器指纹 blob)。**kiro-gw 现阶段不实现激活**(还是用 KiroManager/手工激活后导入),但若将来要"在服务器IP上激活以彻底对齐",这是蓝本。记 later。

---

# §3 Phase 路线(能力点 → 阶段)

> 实施时每个 Phase 照 §1 对应模块的「该 Phase 行」取参考。

| Phase | 目标 | 本阶段要落的关键能力(按依赖序) |
|---|---|---|
| **P1** 单 worker 直通 | 一个 worker 反代真实 Kiro(非流式优先) | eventstream解析🔵 / converter主装配+校验+tool配对+空content兜底🔵🟢 / machineId派生+header装配🔵🟢 / token刷新+social-IdC分流+profileArn🟢 / conversationId派生🔵 / cache_sim核心🔵 / 错误分类🟢 / HTTP client+代理一致性🟢 |
| **P2** 流式 + 资产搬运 | 流式打通,血泪资产到位 | SSE状态机🔵🟢 / thinking签名透传+合成🔵🟢 / inline thinking🔵🟢 / tool_use聚合🟢 / empty-fallback v58🔵 / 空流探测+透明重试🟢 / billing v53🔵 / tool_compression🟢 / 截断检测🟢 / prefix_tree+anchor双锚定🟢 |
| **P3** 多进程 | router + worker + egress 绑定 | 🟣多进程router/worker / 🟣egress local_ip绑定(实测) / 代理一致性🟢 / proxy级冷却🟢 / 延迟感知路由🟢 / QuotaExhausted整簇封禁🟢 |
| **P4** 存储 + admin | SQLite + 管理面板 | 账号状态机持久化🟢 / 余额聚合(web portal)🟢 / 状态定时刷新worker🟢 / admin prompt过滤规则🟢 / access log🟢 / 本地websearch🟢 |
| **P5** 灰度 | 第二IP小流量验证 | 防封验证(同号同IP激活/发包)🟣 / 与旧项目同口径对比 / 稳定后全切 |
| **later** | 风险/增强项 | TLS指纹模拟 / 服务端自建激活 / 远程媒体URL+SSRF防护 / 限制段剥离 |

---

# §附录:原始逐条核对片段

精确行号与逐条说明在这 7 份(sonnet 子代理逐文件核对产出,本文档是消化导航版):

- `docs/_refmatrix_sf_converter.md`(static_flow converter,84条)
- `docs/_refmatrix_sf_stream.md`(static_flow stream+parser,53条)
- `docs/_refmatrix_sf_cache_sched.md`(static_flow cache+调度,71条)
- `docs/_refmatrix_sf_runtime.md`(static_flow runtime+headers+refresh,84条)
- `docs/_refmatrix_xkiro.md`(xkiro,57条)
- `docs/_refmatrix_kiromanager.md`(KiroManager 激活端,39条)
- `docs/_refmatrix_oldkiro.md`(旧kiro.rs搬运资产,43条)

> ⚠️ license:static_flow / xkiro 是他人项目,🟢借鉴 = 参考设计与边界,用自己的代码实现,不直接拷贝源码。🔵搬运仅限旧 kiro.rs(自己的代码)。

