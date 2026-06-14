# Changelog

## [request-log-response + modal-layout] - 2026-06-14

### Feature —— 请求日志新增「模型回复」保存 + 详情弹窗布局修复

**背景**:请求日志已存「用户原始报文 / 发 Kiro 前报文」,但缺模型的实际回复,不便复盘"问了什么→
答了什么"。同时详情弹窗内容过长会撑出浏览器可视范围。

- **后端·模型回复入库**:`request_logs` 新增 `response_payload` 列(gzip 入库,旧库 `ensure_column`
  热升级回填空串)。回复 = 把本次响应折叠成的单条 Anthropic Messages JSON(与下发客户端同形):
  - 非流式:**复用**已折叠、即将下发客户端的同一份响应体(`ResponseLog::Folded`),**不二次折叠**——
    入库与客户端实收严格一致;
  - 流式:转发期间按**累计字节**预算(`RESPONSE_LOG_MAX_BYTES`,与入库截断同量级)采集 SSE 事件
    (`ResponseLog::Events`),收尾在 blocking 任务里折叠。复用转发时的 `data.to_string()` 既当字节
    度量又喂下游,避免二次序列化;
  - 折叠/序列化全在 detach 的 blocking 线程池,**绝不碰收入热路径**;失败请求/无 message_start → 空串
    (详情页不展示该区块)。
- **前端·展示**:详情弹窗第三块「模型回复」,沿用 PayloadView 的 formatted/raw 双视图(新增
  `parseAnthropicResponse` 识别单条 Messages 响应)+ 下载按钮;`response_payload` 为空则隐藏。
- **前端·弹窗布局**:共享 `Modal` 卡片限高 `max-h-[88vh]`、标题栏固定、内容区超出自身滚动,
  彻底解决长报文撑出视口。
- **设计取舍 / 对抗审查(Codex×3,1 high + 数 medium 已修)**:
  - 流式封顶从"按条数"改为**按字节**——单个超大 `input_json_delta` 也挡得住(原 high 共识);
  - 回复序列化超限时存**合法**占位 JSON(`_truncated:true`)而非半截非法 JSON,保住 formatted 渲染;
  - 非流式去掉二次折叠,消除入库与客户端响应分歧风险;`ResponseLog` 具名枚举替代裸 `Vec<SseEvent>`
    参数;弹窗滚动条改 `pr-1` 不用负 margin,避免其它弹窗水平溢出/焦点环裁切。
- **测试**:新增 `serialize_response_capped` 2 个(常规直通 / 超限合法占位)+ store 往返断言;
  workspace 全绿(557 测试)、admin-ui tsc+build 通过。

## [thinking-effort-validation] - 2026-06-14

### Feature —— thinking effort 白名单校验 + 非法值回退告警

**背景**:Kiro desktop/CLI 通过 `output_config.effort`(`low/medium/high/xhigh`)控制思考强度,
caio 全程支持客户端传入(`thinking_policy` 客户端值优先,`generate_thinking_prefix` 注入
`<thinking_effort>` 到上游 wire,缺省 xhigh)。但此前 effort 是**裸字符串透传**,无任何校验——
客户端传个 Kiro 不认的串会原样打到上游,可能触发 400,且存在往 wire 前缀注标签的隐患。

- **新增** `anthropic_types::VALID_EFFORTS`(`["low","medium","high","xhigh"]`)+ `normalize_effort()`:
  归一到白名单(大小写不敏感、去前后空白),命中→标准小写形态;非空但非法→回退 `xhigh` 并返回
  `fell_back=true`;`None`/空白→默认 `xhigh`(不算非法、不告警)。`effective_effort()` 改走它。
- **单一 wire 出口做归一 + 告警**:`thinking_policy` 改为保留客户端**原始** effort 串(不预归一),
  合法化与 `tracing::warn!` 统一在 `generate_thinking_prefix`(`<thinking_effort>` 唯一注入点)做,
  避免双重归一/双重告警。顺带堵死往 effort 标签注入尖括号的口子。
- **设计取舍**:白名单为代码常量(非 YAML 热编辑);非法值默认回退最高档 xhigh(对 Opus 无副作用),
  而非 400 拒绝——宁可降级也不打断请求。客户端要降档须传合法档位或用 `thinking.budget_tokens`。
- **测试**:新增 11 个(`normalize_effort` 6 + `generate_thinking_prefix` 端到端 3 + 既有 effort 2);
  gw-kiro 全套 354 passed。已部署 139(router health 200,启动无 panic)。

## [strip-ephemeral-reminders-history] - 2026-06-14

### Fix —— 剥离历史里的 Claude Code 临时提醒块(降 Kiro 积分,缓存命中)

**根因(实测坐实)**:Kiro 按真实 prefix-cache miss 扣积分。用请求日志的完整 kiro_payload
对同一 conversationId 的连续两轮做 diff,发现 Claude Code 把 `<system-reminder>` /
`<internal_reminder>` 注入 user 轮,再随对话推进**从旧轮里删掉**,导致同一条历史消息跨轮
字节抖动 → Kiro 前缀缓存从该轮起全 miss。实测 caio opus-4-8 烧 **1.28 积分/次** vs kiro.rs
**0.90**(+42%),就是这么来的(总上下文 caio 142k < kiro.rs 162k,纯粹命中差)。

- **修复**:在 `converter/history.rs::merge_user_messages`(**只走历史**)调
  `strip_ephemeral_blocks`,剥掉 `<system-reminder>`/`<internal_reminder>` 块,让历史前缀
  跨轮字节恒定。当前轮经另一路径 `merge_current_message_content`,**保留提醒**(模型本轮仍看得到)。
  这是既有「历史 thinking 丢弃」(`convert_assistant_message`)的直接同构。
- **两处漂移源**(首版只修了第一处,线上 log 2099 验证时抓到第二处):① 主 user 文本;
  ② **tool_result 内容**(`toolResults[].content[].text`——CC 把 workflow 提醒追加在工具输出后)。
  第二处经 `strip_ephemeral_from_tool_results` 就地剥(同样只走历史)。长会话(工具密集)主要靠这处。
- 纯手写扫描(无 regex 依赖):边界校验防 `<system-reminderish>` 误匹配、无闭合标签不吞正文、
  确定性 + 幂等;仅当确有剥离才 trim 首尾(无提醒的历史**零改动**,不引入 cold-start)。
- `KIRO_STRIP_HISTORY_TAGS`(逗号分隔)可追加更多漂移标签。conversationId/亲和键不受影响
  (从原始 messages 派生)。
- 数据驱动确认:census 里 `<available_skills>`/`<env>`/`<skill>` 等高频标签**跨轮稳定、不漂移**,
  故**不剥**(删了丢上下文、零收益)。唯一漂移真凶 = 那两个 reminder。

### Notes & Caveats

- 中段提醒只能尽力对齐(实测提醒都在轮尾,主场景已覆盖)。
- 预期:opus-4-8 积分/次从 1.28 往 0.90 甚至更低降;上线后用 prefix-diff(应变 verbatim)+
  metering_credit/次 下降验证。这套修复也可回灌 kiro.rs(它同样没剥)。

## [account-negative-remaining] - 2026-06-14

### Fix —— 账号剩余积分允许负值(显示超额了多少)

Kiro 账号允许超额跑,但原先 `remaining = (limit - used).max(0)` 把超额夹成 0,
看不出超了多少。去掉两处 clamp(后端 `AccountQuota::from_used_limit` 的 `.max(0.0)`
+ 前端 `formatCredits` 的 `Math.max(n,0)`)→ `remaining = limit - used`,可为负。

- 超额账号在账号表显示如 `-93 / 1,000`(实测 used 1092.87 / limit 1000 → remaining -92.87)。
- `isQuotaLow` 加 `remaining < 0 → true`:超额一定标红,即使上限未知也不漏标。
- 不影响调度:账号停用由 `QuotaExhausted` 错误驱动,不读此快照(对抗审查确认)。
- 已部署 139。

## [usage-cost-stats-panel] - 2026-06-14

### Feature —— 积分与成本统计面板(按 Anthropic 标准价折算 · 计费/真实双口径)

`/usage` 页原本只有 token 用量 + 时间筛选,缺积分与 USD 成本。补齐成"累计用量与成本"卡:
总花费 / 总请求 / 总积分消耗 / 每积分成本 + 输入·输出·缓存读 token + 每积分 in/out,
按模型表加 积分 / 每积分in / 花费 列。沿用既有时间筛选(7d/30d/all + 自定义区间 + key)。

- **存储**:`usage_records` 加 `metering_credit`(Kiro 真实积分)+ `real_cache_read_tokens`
  (金标准缓存命中)两列;`ensure_column` 热迁移存量库(历史行回填 0,**不可补**)。
  `record()` 落库带上两值;三个聚合查询 SUM 出来。负 credit 在插入处 `max(0)`,聚合再用
  `SUM(CASE WHEN metering_credit>0 …)` 兜底(单条坏行不污染分母)。
- **计价**:新 `gw-core/pricing.rs`——Anthropic 标准价(opus 5/25/0.5/6.25、sonnet 3/15/0.3/3.75、
  haiku 1/5/0.1/1.25,USD/1M),子串分档。`admin/usage.rs::model_cost` **按模型分别计价后求和**
  (不同模型不同价,不混算)。
- **关键口径**:caio 存的 `input_tokens` 是**总上下文**(含 cache_read 子集),故折算 USD 前
  必须 `input - cache_read` 得未命中输入再计价(否则缓存 token 既按 input 全价又按命中价**重复计费**);
  前端展示同样扣除,与 kiro.rs 成本面板口径一致。
- **双口径**:计费口径用上报 `cache_read`,真实口径用 `real_cache_read`(历史多为 0 → 全价估算,
  卡片加脚注说明真实成本为上限)。

### Design Rationale

- 单一权威成本算在 admin 层而非 store 层:store 只忠实记录,计价/折算是展示策略,放近端点便于改价。
- 子串分档(非精确日期名匹配):同族同价,`claude-opus-4-5-20251101` 与 4.5 同价,够用即可
  (对齐 kiro.rs,符合"计费分类器够用就行")。

### Notes & Caveats

- **历史无积分**:`metering_credit`/`real_cache_read` 是新列,部署前的行恒 0,回填不了 →
  积分/每积分成本/真实口径缓存只对**部署后**新流量有数;USD 花费用历史就有的 token 数算,历史也准。
- `by_key` 暂不出 USD 成本(需按 key×model 分组才准确;面板当前不需要)。已知保留。
- summary 的 token 总数与成本来自两条独立查询(usage_summary + usage_by_model),活跃写入时
  可能差一个亚秒快照——只读统计面板,刷新自愈,非计费权威路径。已知限制。
- 对抗审查(codex×3):核心已验正确(双口径减法、迁移竞态、除零守卫);采纳 3 项廉价加固
  (pricing 文档纠正、负 credit 聚合兜底、真实口径脚注);Minimalist"砍掉积分/真实/每积分列"
  与用户明确指定的目标面板冲突,**驳回**。

## [request-logs-gzip-fulltext] - 2026-06-13

### Fix —— 报文不再截断:gzip 全文存储 + 下载完整报文

**问题**:报文文本按 512KiB 截断,大报文(实测 534KB roleplay)被截尾 → JSON 非法 →
详情页既不能格式化(回退原始)也不完整。任何固定上限迟早截到长对话。

- **gzip 全文压缩存储**(`gw-store`):`client_payload`/`kiro_payload` 入库前 `gzip_text`
  压成 BLOB(文本压 5-10 倍),读时 `value_to_payload` 按动态 `Value` 还原——`Blob`→解压、
  `Text`→旧明文行原样(**无缝兼容本特性前的明文行**,无需迁移)。gzip 失败退原文字节、
  解压失败 lossy,绝不 panic。文本截断上限 512KiB→**16MiB**(纯防御;Kiro 报文硬上限 ~6.3MB,
  真实报文绝不触顶,全文无损)。
- **前端「下载完整报文」按钮**:详情弹窗两份报文各加下载(Blob→`a.download`),超大报文不便
  在弹窗滚动时直接存盘。

### Notes & Caveats

- 旧明文行与新 gzip 行共存:读侧按 SQLite 存储类(Blob/Text)分流,`r.get::<_,Value>` 兼容两者
  (rusqlite `Vec<u8>` FromSql 只认 BLOB,故不能直接读旧 TEXT 行——必须走 Value)。
- 解压无外部输入面:BLOB 只由我方 `gzip_text` 产出,旧行是明文,无解压炸弹风险。
- 格式化视图单 text block >20000 字符仍转纯文本 `<pre>`(防大正文卡死),与本次叠加。

## [request-logs-pagination] - 2026-06-13

### Feature —— 请求日志列表分页

原本一次最多返回 200/2000 条(`total=754` 实测),前端一次渲染太多。加基本分页:

- 后端 `RequestLogFilter` 加 `offset`;`list_request_logs` 加 `OFFSET`;新增 `count_request_logs`(同筛选总数,不受 limit/offset 影响)。`GET /logs` 改返回信封 `{items,total,page,page_size}`;`LogsQuery` 加 `page`/`page_size`(默认 50,钳 [1,2000],兼容旧 `limit`),`offset=(page-1)*page_size`(`saturating_mul` 防溢出)。
- 前端列表消费信封,加上一页/下一页 + 「共 N 条 · 第 X/Y 页」;筛选变化回第一页(`keepPreviousData` 翻页不闪)。`i18n t()` 支持可选 `{name}` 占位参数(向后兼容,原无参调用不变)。

## [request-logs-media-dedup] - 2026-06-13

### Feature —— 请求日志保存图片/文档(去重)+ 前端格式化/Markdown 视图

调试需要看到客户**实际上传的图片/文档**,但同一张图在一个会话里每轮都发,直接存会撑爆库。
方案:**内容寻址去重**存储 + 前端内联渲染。

- **媒体 blob 去重存储**(`gw-core` + `gw-store`):新增 `extract_log_blobs`——按字段把
  Anthropic `image/document.source.data`(需同级 `media_type`/`type:base64`)、Kiro `source.bytes`、
  `images[].source.bytes` 的 base64 抽到 `log_blobs` 表(主键 `hash=sha256(base64)`,`INSERT OR IGNORE`
  自动去重),报文里只留 `blob:<hash>` 短引用。对话正文走 `text` 键、**绝不误抽**(修正旧版按
  字符集猜测会误伤无空格长正文的问题)。`log_blob_refs(log_id,hash)` 记多对多引用。
- **环形 GC**(`insert_request_log` 改事务):日志裁到最新 2000 条时,连带删被裁日志的 blob 引用,
  再清掉已无任何引用的 `log_blobs`(防无限膨胀)。详情按 `log_id` JOIN 取回该条引用的 blob。
- **前端格式化视图**(`PayloadView.tsx`):详情弹窗加「格式化/原始」切换。格式化把 Anthropic/Kiro
  两种报文解析成对话(system/user/assistant + text/thinking/tool_use/tool_result/media 块),正文用
  `react-markdown`+`remark-gfm` 渲染;**图片内联**(data URI)、**文档给下载链接**;解析失败回退原始 JSON。
- 报文**文本**上限从 64KiB 提到 512KiB(媒体已抽走,只剩文本 + blob 引用,≈13 万 token 覆盖绝大多数对话)。

### Notes & Caveats

- 安全:`react-markdown` 默认不渲染原始 HTML、拦 `javascript:` 协议;Markdown 远程图片不加载(占位)。
  blob 媒体的 `media_type` 是**客户可控**值,故 data URI 走**白名单**:仅 png/jpeg/gif/webp/avif/bmp 内联
  渲染、application/pdf 可下载;其余(text/html、svg 等)只占位不生成 data URI——杜绝
  `data:text/html` 下载链接被中键/Ctrl 点开新标签执行任意 JS(对抗审查 high 修复)。
- 健壮性:`PayloadView` 解析器整体 `try/catch` + 数组字段 `Array.isArray` 守卫;单个文本块 >20000 字符
  不跑 Markdown 改纯文本(防不可信超大正文卡死详情页主线程)。
- 详情响应会内联该条日志引用的 blob 完整 base64;一条引用多张大图时响应偏大(debug 单条查看,可接受)。
- 库膨胀:512KiB×2×2000 文本 + 去重后 blob 表;`DELETE` 不自动缩文件(SQLite 不 VACUUM),空间复用但
  文件不缩。去重 + GC 保证不无限涨。

## [wire-parity-isError-multimodal] - 2026-06-13

### Fix —— 两处发往 Kiro 报文与 static_flow 金标准对齐

离线把 caio 完整 wire 与 static_flow/kiro.rs 逐字段对比(`[cache-fix-no-agent-continuation]` 后已无大缓存差异),修两处剩余 parity/正确性差异:

- **`ToolResult.is_error` 永不上 wire**(`kiro_types/tool.rs`):原在出错时发 `"isError":true`,改为 `#[serde(skip_serializing)]` 永不发——真 Kiro 客户端/static_flow 都不发,错误信号只走 `status:"error"`(caio `ToolResult::error()` 与 content.rs 已设 status,删序列化安全)。去掉真客户端没有的字段,收窄检测面、对齐金标准报文。
- **多模态 schema 兼容**(`converter/tools.rs` + `mod.rs`):新增 `apply_multimodal_tool_schema_compatibility`——请求(当前轮或历史)含图片时,把带 `anyOf/oneOf/allOf/$defs` 等 10 个关键字的工具 schema 整体降级为宽松 object schema,避免 Kiro 对"多模态 + 复杂 schema"返回 400 Improperly formed request。逐字搬 static_flow `converter/schema.rs`,在工具放置前 hook(current/history 放置都覆盖)。

### Notes & Caveats

- 两项均**非缓存**改动:`[cache-fix-no-agent-continuation]` 已把真号真实成本降到 kiro.rs 水平(0.0106→0.0100/1k);本次是报文合法性/抗检测对齐。`真=miss` 显示是 Kiro 对 caio 这批号不填 cacheReadInputTokens 的报告口径,与本次无关。
- 多模态降级**仅在含图片时**触发,且只替换真正含不支持关键字的 schema;纯文本请求工具定义零改动(有测试锁双向)。降级会丢失该工具参数约束,是 static_flow 接受的取舍(宁可宽松也别 400)。

## [cache-fix-no-agent-continuation] - 2026-06-13

### Fix —— 真实缓存全 miss 根因:停发自造的 agentContinuationId

**症状**:线上 caio 每条请求真实命中 `真=miss`(Kiro `tokenUsageEvent.cacheReadInputTokens=0`),
同一对话连续轮也全冷,真号真实额度暴烧;同期 kiro.rs 同类负载真实命中 20~37%。

**根因**:caio converter 把自造的稳定 `agentContinuationId`(+ body 里的 `agentTaskType="vibe"`)
写进发往 Kiro 的 ConversationState。Kiro 收到 `agentContinuationId` 会把请求当成「续接一个它从未
签发的 agent 上下文」,**绕过 conversationId 前缀缓存** → 每轮冷 miss。conversationId/亲和/前缀
其实都正常(模拟器都能检测到前缀连续),唯独这个多余字段毒掉了真实命中。

**修复**(`crates/gw-kiro/src/converter/mod.rs`):ConversationState 只发 `chat_trigger_type="MANUAL"`,
**不再发 `agentContinuationId` / `agentTaskType`**——逐字对齐 static_flow 金标准(其测试
`convert_request_does_not_send_random_agent_continuation_metadata_by_default` 断言这俩为 None)
与 kiro.rs 生产(二者均不发)。删除无用的 `derive_agent_continuation_id`,补回归测试
`convert_request_does_not_send_agent_continuation_metadata` 锁死 wire 形状。

**纠正旧误判**:此前注释/记忆称"agentContinuationId 必须稳定否则 3 倍计费"——那个实验对比的是
「稳定发」vs「随机发」两个都错的配置;正解是**根本不发**。本修复正是用刚上线的"真/credit"
请求日志(`[request-logs-real-credit]`)抓到 `真=miss` 才定位的。

## [request-logs-real-credit] - 2026-06-13

### Features —— 请求日志增加「真(真实命中)/ 报(上报缓存)/ credit(Kiro 原生计费)」

让每条请求都能看到**真号真实消耗**信号,供按真实缓存命中持续优化(对齐 kiro.rs 的 真/报/credit 展示):

- **真** = 上游 `tokenUsageEvent.cacheReadInputTokens`——真号在 Kiro 服务端的**真实** prefix cache
  命中(决定真号真实额度消耗速度);**报** = 既有 `cache_read_tokens`(经 floor/cap 模拟后上报
  NewAPI 的计费口径);**credit** = 上游 `meteringEvent.usage`(Kiro 原生计费,真号本次真实积分消耗)。
- **捕获(`gw-kiro/chat.rs`)**:此前 `tokenUsageEvent.cacheReadInputTokens` 算进 report_total 即
  丢弃、`meteringEvent` 整个吞掉;现各留一份。**仅记录,绝不参与上报计费**(`reported_cache_read`/
  `build_usage_json` 逻辑零改动)。热路径仅新增 meteringEvent 一次小 JSON parse(流末尾、不阻塞 TTFB)。
- **透传**:`ChatUsage` +`real_cache_read_tokens`/`metering_credit`(去 `Eq`,因含 f64);
  `RequestLog`/`RequestLogRow` 同步加列;`worker/mod.rs::write_request_log` 透传。
- **store**:`request_logs` 加 `real_cache_read_tokens INTEGER` + `metering_credit REAL` 两列;
  存量库经 `ensure_column` 热迁移(并发 open 竞态下 duplicate-column 视为成功,审查 Skeptic#1)。
- **前端**:请求日志表把单列"缓存读"拆为 **真 / 报 / credit** 三列(真带命中%、credit 4 位小数、
  input 下显示上下文%),详情弹窗补 6 格;contextWindow 归一点号/连字符模型名(审查 Architect#1)。

### Design Rationale

- 两个新值是**已被上游下发、原本丢弃**的数据,捕获近乎零成本;刻意只新增透传字段、不动
  `reported_cache_read`/`build_usage_json`,与"报"(上报计费)严格隔离——优化看"真/credit",计费看"报"。
- credit 直接取 Kiro `meteringEvent.usage`(金标准 metering),不自建价格表;判断 Kiro 服务端有没有
  应用缓存折扣的权威信号。

### Notes & Caveats

- `real_cache_read_tokens=0` → 前端显示 "miss"(无 tokenUsageEvent 真值或真实零命中);
  `metering_credit=0` → 显示 "—"(无 meteringEvent 信号)。非 Kiro provider(subprocess)二者恒 0。
- 上下文% 前端按 model→窗口估算(opus 4.6/4.7/4.8、sonnet 4.6 = 1M,其余 200k);写库的 `input_tokens`
  为 report_total(含缓存总量),故真实命中% = 真 / input_tokens。

## [request-logs] - 2026-06-13

### Features —— 请求日志(调试用:每次 chat 落一条,含完整报文)

补全此前只建了 store 层地基的"全报文请求日志":worker 在每次 chat 收尾环形落库最新 2000 条
(账号/模型/流式/成功失败/状态码/错误类型/耗时/TTFB/input·output·cache_read token + 用户原始
报文 client_payload + 发 Kiro 前报文 kiro_payload),admin 暴露列表(按成功/失败/账号/模型/时间
窗筛选)+ 详情(含两份报文),前端新增"请求日志"页(筛选 Segment + 表格 + 详情弹窗看报文)。

- **捕获(`worker/mod.rs`)**:挂在 #130 已验证的 `StreamCtx::Drop`(流式)与 `collect_response`
  (非流式)收尾;**绝不碰收入热路径**——两份报文序列化 + `kiro_payload` 重渲染 + 同步 SQLite 写
  全部 detach 到 **blocking 线程池**(`spawn_blocking`),handler 入口不再同步序列化 client_payload。
  失败请求也记:首包前终态失败(上游 400 / 重试耗尽)在 chat-error 分支补落一条,故"失败"筛选
  能看到生产 400 类失败。
- **渲染助手(`gw-kiro/chat.rs::render_kiro_payload`)**:纯函数复刻转换 + profileArn 注入,
  不跑 cache_sim、不发送、无副作用,在 blocking 任务里重渲染"发 Kiro 前报文"。
- **store**:复用既有 `request_logs` 表 + `insert_request_log(cap)`(环形)/ `list_request_logs`
  / `get_request_log`;每份报文入库前按 UTF-8 边界截断到 64KiB(最坏 2000×2×64KiB≈256MiB)。
- **admin**:`GET /admin/api/logs`(筛选,limit 钳 ≤2000)+ `GET /admin/api/logs/{id}`(详情)。
- **前端**:`pages/RequestLogsPage.tsx` + `features/logs/*` + 侧栏导航 + i18n(zh/en)。

### Design Rationale

- 复用 #130 的 `StreamCtx::Drop` + `pending_writes` 排空机制,捕获与 usage 落库同生命周期;
  请求日志走 `spawn_blocking`(序列化/写库是阻塞工作),usage 走 `spawn`(sink.record 是 async)。
- 不扩 `ChatUsage`/`StreamItem`/chat 发送链路,故 credit、真实缓存命中(meteringEvent)本期不抓
  (schema 无此列);避免改每请求必过的热路径核心类型。

### Notes & Caveats

- **对抗审查(Codex)修复**:client_payload 移出热路径(blocking 任务内序列化)、render+写库改
  `spawn_blocking`(不占 async worker 线程)、非流式日志改 detach(不持 lease 不延迟响应)、
  补失败请求落库、payload cap 256→64KiB、limit 钳制、上游错误回显 400/502 状态码。
- **未抓**:token 刷新类终态失败(B/C/D 站点)不落库——经账号状态可见,本期略;
  `kiro_payload` 展示的是转换产物,不含体积护栏剔除媒体后的瘦身版(护栏属另一关注点)。
- 部署即生效;capture 仅在 chat 流量上触发,上线时 caio 空闲故 `request_logs` 暂 0 行,
  下一笔真实流量即落库(受"真号禁聊天验证"约束未自造流量验证写路径,端点/鉴权/前端已验)。

## [admin-ui-restyle] - 2026-06-13

### Features —— admin 前端整体换肤（玻璃拟态 → playmate 设计语言）

把 admin 控制面从原"玻璃拟态（glassmorphism）"全面改为 playmate-platform 的设计语言，
统一品牌观感。**纯视觉重构：零业务逻辑/API 契约/i18n key 改动**，所有 hooks、props、
表单校验、两阶段验活、脱敏哨兵、轮询、proxy 三态语义原样保留。

- **设计 token 重写**（`styles/globals.css`）：调色板换成 暖米白纸面（浅色 `#f3f4ef`）/ ink 近黑画布（深色 `#050505`），
  唯一强调色 acid 荧光黄绿 `#d7ff2f`；圆角整体放大（卡片 28px / 容器 32px / 按钮·标签 pill）；
  字体引入压缩超粗大标题（`font-display` + `font-black` + 负字距）+ `.eyebrow` 章节小帽。
  旧玻璃工具类名（`glass-card*` / `gradient-bg-primary` / `page-hero` 等）**保留为主题 API**，
  底层重定义成纸面风格，避免逐处改类名。
- **原子组件换肤**（`components/ui/*`）：Button（黑 pill，hover 翻 acid）/ Card（实色白卡）/
  Badge / Segment（acid 激活药丸）/ Table / Modal（白圆角大卡）/ Select / StatCard / ErrorNote。
- **布局**：侧边栏恒为 ink 黑 + acid 激活块 + CAIO wordmark（`Sidebar`/`AppShell`）；
  登录页为暗色品牌时刻（双径向渐变 + 噪点 + acid CTA）。
- **页面**：6 个页面页头统一为无框纯排版（eyebrow + 压缩大标题）；usage/accounts/keys/groups/settings
  各 feature 组件随原子组件换肤。

### Design Rationale

- 对标 playmate-platform（Next.js + Tailwind）的视觉，但落地仍用本项目既有栈（React 19 + Tailwind v4 + Vite），
  不引入新框架/新依赖，构建与 embed-ui 部署链路零改动。
- 沿用旧玻璃类名作为"主题 API 层"重定义，使 feature 组件改动面最小、回归风险最低。

### Notes & Caveats

- 本次仅改 `admin-ui/` 前端样式文件 + 本 CHANGELOG；未触碰任何 Rust 代码。
- 全量 `bun run build`（tsc + vite）通过；Playwright 实测登录/看板（明+暗）/账号/设置/对话框五处布局无崩溃。
- 工作区另有 5 个文件（`accounts.rs`/`accounts/api.ts`/`hooks.ts`/`settings/types.ts`/`i18n.tsx`）为**更早会话遗留的未提交功能改动**，与本次无关、未触碰。

## [body-guard-poison-memo] - 2026-06-13

### Features —— 出站体积护栏 + 毒报文备忘录（从 kiro.rs v63 移植）

背景:kiro.rs 生产事故——客户会话史带大 PDF/图,序列化后超 Kiro 报文体积硬上限
（实测 ≤6,341,854 字节成功、7,336,893 字节确定性 400 "Improperly formed request"），
整个会话每轮必死。caio 此前**无任何体积防护**,同样场景会复现。

移植 kiro.rs 四联修中 caio 缺失的两项（Fix A 上游 400 透传、Fix B 全 kind 冷却 caio 早已具备，未搬）：

- **出站体积护栏**（`converter/shed.rs` + `chat.rs`）：序列化后若超
  `max_upstream_body_bytes`（默认 6,300,000，留 4096 headroom 吸收 profileArn 注入），
  先从 history **最老媒体按附件粒度**剔除（文档优先，达标即停，追加中性占位
  "[attachment omitted due to size limits]"），**绝不动 currentMessage**；剔后重序列化仍超限、
  或无历史媒体可剔 → 本地 `BadRequest`（→客户端 400，不发上游、不惩罚账号）。
  会话从"超限即毒化"变"自动瘦身续命"。
- **毒报文备忘录**（`poison_memo.rs`）：上游确定性 400（`BadRequest` kind）的请求体
  SHA-256 指纹记入 TTL 600s / 容量 512 的备忘录；同字节级 payload 再来本地 400 拦截，
  兜住无视 400 仍重试的客户端（本次事故 NewAPI 上游即此类）。指纹由 chat 路径持有
  （发包前算一次），避免对 MB 级 body clone；锁中毒 into_inner 恢复，不 panic 主路径。
- **配置**：`KiroProvider.max_body_bytes`（`from_config` 读 `max_upstream_body_bytes` /
  `apply_hot_settings` 30s 热调，0 视为非法忽略）。

### Design Rationale

- caio 的 `classify_chat_error` 已正确把上游 400 分到 `BadRequest`→客户端 400，
  `report_failure` 已全 kind 覆盖冷却，故 kiro.rs 的 Fix A/B 无需移植；只补缺失的 C/D。
- 媒体结构（`KiroImage`/`KiroDocument`/`history`）与 kiro.rs 逐字段一致，`shed_history_media`
  近乎原样移植，仅改 import 路径。
- 护栏插在 `chat.rs` 序列化点之后（唯一接缝），超限返回 `UpstreamError::bad_request`
  复用既有 `BadRequest`→客户端 400 路径，语义正确且 `BadRequest` 天然不惩罚账号。

### 对抗审查修复（Codex Skeptic/Architect/Minimalist 三视角）

- **砍掉死配置管线**（三人共识 medium）：`from_config`/`apply_hot_settings` 读
  `max_upstream_body_bytes` 是死代码——caio 的 `SystemSettings` 没有该字段且
  `deny_unknown_fields`，worker 构造 provider cfg 时也不塞，永远读不到。改为固定常量
  `DEFAULT_MAX_BODY_BYTES`（provider 普通 `usize` 字段，测试用 `with_max_body_bytes`
  注入小值）。热调留作将来配 admin 面板时一并设计正确的 SystemConfig 归属节。
  *附带收益*：砍掉 config 读取后用户传不进过小值，Skeptic 的下限越界风险一并消失。
- **去掉 `BODY_LIMIT_HEADROOM`**（Architect#4）：caio 的 profileArn 在序列化前就注入，
  发包路径只加 header 不改 body，序列化的 body 即最终出站字节，kiro.rs 的 headroom 前提
  在 caio 不成立，留着只是白降 4KB。
- **cache_sim 移到护栏之后**（Architect#2）：否则剔除历史媒体后仍按未剔除口径观测，会
  高估 input/cache_read tokens **落库计费**（不只显示），且把从未发出的前缀写进模拟缓存
  污染后续轮。现观测的是真正发出的 conversation_state。
- **收紧毒备忘录 remember 条件**（Minimalist#1）：只记 body 含 "Improperly formed" 的
  格式/体积类 400，不记所有 `BadRequest`——后者可能含账号/profile 相关 400，换号或修
  profile 后同 body 本可成功，记毒会跨账号误伤 600s。对齐 kiro.rs v63 短语门。
- **护栏抽成 `enforce_body_limit` 独立函数**：与 kiro.rs `enforce_upstream_body_limit`
  对称，补上 3 个单测（透传/剔除/无媒体可剔→BadRequest），不再只靠 HTTP 路径覆盖。

### Notes & Caveats

- 体积上限 6.3MB 固定为常量（真实上限在 (6.34, 7.34]MB 区间）；暂不经 admin 热调，
  需调整改 `DEFAULT_MAX_BODY_BYTES` 重新构建，或后续接入 SystemSettings。
- 毒备忘录每请求算一次 SHA-256（MB 级 body 几 ms），与 kiro.rs 同款取舍：账号保护优先。
- **毒备忘录是单 worker 进程级**（Architect#3，已知限制不修）：caio router+多 worker 进程
  模型下，全局 static 不跨进程共享；但会话亲和已把同会话钉到同 worker，且备忘录是
  纵深第二道（第一道 400 透传已让正常客户端不重试），跨进程共享属过度工程。各 worker
  各自记毒后仍收敛。

## [model-catalog-unified-table] - 2026-06-13

### Features
- **`/v1/models` 公告全量模型名(thinking 变体 + 日期快照)**:原只列 6 个裸名,NewAPI 渠道按日期名(`claude-sonnet-4-5-20250929`)/thinking 名(`claude-opus-4-8-thinking`)选型时拉不到。现从权威表生成 **20 个对外 id**——每个模型展开 plain / `-thinking` / 日期快照 / 日期`-thinking`(4.5系与 haiku 各 4 个,其余各 2 个)。这些名字此前**发 chat 其实已能路由**(map_model 子串匹配 + thinking_policy 处理后缀),本次只是把它们如实公告出来。
- **单一权威表 `KIRO_MODELS`**(`gw-kiro/src/converter/model_map.rs`):对外裸名 ↔ 上游 Kiro 模型 ↔ 身份短名 ↔ 上下文窗口 ↔ thinking 能力 ↔ 日期别名,一处定义。`/v1/models`、chat 路由(`map_model`)、身份规范化(`requested_model_identity`)、窗口推导(`get_context_window_size`)**全部从表派生**,消除历史"四处列表不一致"漂移。新增 `resolve_base`(统一归一 plain/-thinking/日期/日期-thinking 四形态)。

### Bug Fixes
- **修日期快照名导致 Kiro 真实代号(claude-quince)泄漏**:`requested_model_identity` 原对去 `-thinking` 后的名字做**精确匹配**,`claude-sonnet-4-5-20250929` 匹配不上 → 返回 None → system identity 行不规范化 → 上游真实代号泄漏给客户端。改走 `resolve_base`(剥 thinking + 剥日期后匹配),身份行回显**去 thinking 的原请求名**(保留日期名,与甲方请求一致),既不泄漏又名实相符。一旦公告日期名,此泄漏必被触发,故本次同改。

### Design Rationale
- 参考甲方 API_All_in_One 的 ALLinOne `models.yaml` 声明式单一来源思路,但用 **Rust 常量表**落地(代码表先上;暂不做 YAML 热编辑 + admin 模型管理页)。新增/下线模型只改 `KIRO_MODELS` 一处,派生点自动跟随。
- `map_model`/`get_context_window_size` 保留**子串兜底**:权威表未命中时(老客户端异名)仍按版本号子串路由,零回归。
- 日期↔版本静态对应(sonnet-4.5=20250929 / opus-4.5=20251101 / haiku-4.5=20251001)由表维护——Kiro 全链路拿不到带日期快照号,自维护是唯一路径。

### Notes & Caveats
- **不出价格**:NewAPI 价格在其渠道侧手配,`/v1/models` 不返回价格、NewAPI"从上游获取"也只拉 id 列表。
- opus-4.5 之前只被 map 接受、未公告,本次纳入公告 + 身份规范化(甲方列表含 `claude-opus-4-5-20251101`)。若 Kiro 上游对某裸名回 INVALID_MODEL_ID,从 `KIRO_MODELS` 删该行即可。
- cargo test 501 全绿;本地 worker `/v1/models` 实测 20 个 id 正确。

## [import-verify-on-arrival] - 2026-06-13

### Features
- **批量导入「导入即验活」**(对齐 kiro.rs batch-import-dialog 能力):导入对话框升级两阶段——导入(原智能合并不变)后**自动逐账号验活**:刷新 token + getUsageLimits 查配额,**全程只读、绝不发 chat**(no-chat-test-on-real-accounts)。逐行状态(等待/验活中/✓成功含 积分剩余/上限/⚠无配额数据/✗失败含上游原文)+ 进度条 + 汇总计数;验活失败且**本次新建**的账号提供「一键删除」回滚(merged 是已有账号,绝不连带);验活失败的号由 worker `report_failure` 自动标禁用,死号导入即现形。死号再也不用烧一条 chat 才暴露。
- **后端验活通路**:worker 新增 `POST /accounts/{id}/quota`(确保 token 有效→查配额;成功回写 quota_cache 让积分列即时反映,失败也写入节流缓存)+ admin 顺序扇出 `/accounts/{id}/quota`(镜像 refresh:首个非 404 即返、错误原文透传)。`try_fetch_quota` 返回类型改 `Result<_, UpstreamError>`(原 anyhow 包装),让验活端点能透出可分类错误;后台轮询调用方行为不变。
- **导入后立即同步(消 30s 窗口)**:worker 新增 `POST /sync`(与 30s 周期循环**共用** `sync_accounts_from_db` 实现);admin `import_accounts` 落库后 best-effort 并发捅所有 worker `/sync`——导入完立刻可验活/刷新,不再报「没有 worker 持有该账号」。前端仍保留 404=等待同步的重试兜底(4s×8)。

### Design Rationale
- 不照搬 kiro.rs 的"客户端逐条 add + 验活失败自动回滚删除":caio 的服务端一次性导入 + 智能合并更安全(token 不回退/碰撞防护),验活失败改**显式按钮**删除且只删 created——merge 语义下自动删有误删真号风险。
- 新端点仅挂 loopback(与 reset/refresh 同块),信任同机 router;验活失败 report_failure 与 chat/refresh 路径同口径(TokenInvalid→invalid_refresh_token 禁用,transient→计数,救号可清)。

### Notes & Caveats
- 验活通过=「能刷新+能查配额」,**不等于保证 chat 可用**(suspended 号已知 refresh 200 但 chat 403;此类号配额查询通常也失败/无数据,能被识别)。
- 本地隔离烟测 + Playwright 全流程验证(导入假号 2s 内验活命中、invalid_grant 原文透传、自动禁用、一键删除→DB 清除);cargo test 494 全绿。

## [machineid-no-freeze-match-kirors] - 2026-06-12

> **⚠️ 2026-06-13 更正**:本条的依据「kiro.rs 从不冻结、每次按当前 rt 重派生」是**误读**——只看了 `machine_id.rs::generate_from_credentials`,漏看加载层:`kiro.rs token_manager.rs:649-652` 在启动时对无 machineId 的凭据**派生并写回 credentials.json 持久化**(= 首导入即冻结),此后 rt 滚动不重算。即:用户长期不封号的 kiro.rs 实为**冻结**行为;static_flow 则是每次重派生,也不封。→ 冻结/重派生**都不是封号因子**,mrdev3258 等翻车实为 IdC durable 整批预死。本次改动(删冻结)与 static_flow 一致,行为无害,**暂保留**;若要逐字对齐 kiro.rs 可恢复导入时冻结(`freeze_machine_id_if_absent` 仍保留未删)。

### Bug Fixes（防封,重大）
- **撤销 machineId 冻结,改为每次按当前 rt 重新派生(对齐 kiro.rs + 真实客户端)**:`gw-kiro/src/lib.rs::refresh_auth` 原在首次刷新时把派生的 machineId(`sha256("KotlinNativeAPI/"+rt)`)**冻结**为显式 `machine_id` 持久化(理论:防"指纹随 rolling token 漂移")。**该理论错了**:对照 kiro.rs `generate_from_credentials` —— 它**从不冻结**,每次按**当前** refresh_token 重新派生;真实 Kiro 客户端也这么算,rt 滚动时 machineId 随之滚动、始终与上游一致。冻结反而在 rt 滚动后发出**陈旧** machineId(真实客户端不会发的值)→ 风控视作换设备 → 封号(mrdev3258 被反复刷新滚 rt + 陈旧冻结值,秒封)。已移除冻结调用;`generate_from_account` 保持"显式真机值优先,否则每次按当前 rt 派生"。`freeze_machine_id_if_absent` 标 `#[allow(dead_code)]` + 弃用说明防误接线;gw-core MachineIdentity 文档同步纠正。**有真机 machine_id(import 带入)的号不受影响,仍优先用显式值。**

### Notes & Caveats
- 已有库里若残留**之前冻结**的 `machine_id`(派生值),会继续被优先使用(陈旧)→ 建议清掉这些号的 `machine_id` 字段让其重新按当前 rt 派生(本次部署时账号库为空,无残留)。

## [import-camelcase-compat] - 2026-06-12

### Features
- **「添加账号」智能填充 + 类型自动识别**:CreateAccountDialog 顶部新增「智能填充」粘贴框——粘一个账号的 JSON(camelCase / snake_case / KiroManager 嵌套 `credentials` 都认,非 JSON 当纯 refresh_token),**客户端自动抽取** refresh_token/client_id/client_secret/region/machine_id + 从 email/userId 派生 account_id,并**识别类型**:有 client 凭据按 provider 分 `BuilderId✅安全` / `IdC⚠️易封`,无凭据为 `Social`。识别后字段可手动核对/改。同时补齐手动凭据字段(Client ID/Secret 成对校验 / Region / Machine ID)。i18n zh/en。线上合成账号烟测确认 client_id/client_secret/region/machine_id 正确入库(secret 脱敏)。**批量仍走列表页「导入」;此为单号便捷补录。**

### Bug Fixes
- **扁平导入兼容 camelCase(修 BuilderId 号导入丢 client 凭据)**:`gw-kiro/src/import.rs::map_flat` 原用 `top_str` 精确取键、只读 snake_case 的 `client_id`/`client_secret`/`access_token`/`profile_arn`/`auth_method`,而用户在 kiro.rs 用的 **camelCase BuilderId 导出**(`clientId`/`clientSecret`/`refreshToken`/`provider:BuilderId`)会**静默丢失 client 凭据** → 刷新分流(`is_idc` 看 client_id+secret 在不在)判成 social → BuilderId 号刷新失败/行为错。新增 `flat_str`(snake_case 优先、回退 camelCase,对齐 kiro.rs 的 `#[serde(rename_all="camelCase")]`)+ `snake_to_camel`,`map_flat` 全字段改用之;`expires_at` 也认 `expiresAt`。snake_case durable 格式不回归。新增 3 测试(camelCase BuilderId 保留 client 凭据 / camelCase accessToken·profileArn·machineId / snake_to_camel);线上合成账号烟测确认 clientId+clientSecret+provider 正确入库。

### Notes & Caveats
- **账号类型与防封**:BuilderId(个人 AWS Builder ID)的 machineId 本就按 `sha256("KotlinNativeAPI/"+rt)` 派生,我方派生值=真客户端值 → 不封(`has_machine_id:false` 对 BuilderId 是**正常且安全**的)。IAM Identity Center(`auth_method:idc`,企业 SSO)用真机 machineId,派生对不上 + 新出口 IP → 风控锁号(`TEMPORARILY_SUSPENDED`)。**生产用 BuilderId,避开 IdC durable 导出。**

## [x-api-key-auth-and-concurrency-display] - 2026-06-12

### Bug Fixes
- **接受 Anthropic 标准 `x-api-key` 鉴权头(修 NewAPI「从上游获取」500)**:`router::extract_bearer` 原只读 `Authorization` 头,而真 Anthropic API / NewAPI 的 Claude 渠道 / Anthropic SDK / Claude Code 指到本网关都发 **`x-api-key`** → 一律 401,NewAPI 再包装成 500(表现为点「从上游获取模型」报 500)。改为**两种头都认**:优先 `x-api-key`(本网关对外是 Anthropic 线缆),回退 `Authorization: Bearer <key>`(或直接放 key);均 trim、空值跳过。修复后 `/v1/models`(及 `/v1/messages` 等所有代理端点)对 Anthropic 原生客户端可用。错误文案改「缺少 API key(x-api-key 或 Authorization: Bearer)」。新增单测覆盖两种头/优先级/空值回退/缺失。
- **并发列语义翻正(显示在用而非空闲)**:账号页「并发」列原显示 `available_permits/max`(空闲槽/上限),空闲账号反直觉地显示 `2/2`。改为通用约定 **`在用/上限`**(`在用 = 上限 - 空闲`),空闲 → `0/2`;加 tooltip。纯前端 + i18n(zh/en)。

### Notes & Caveats
- 鉴权仍逐请求查库验 key,`x-api-key` 只是多认一个头、不放松校验(无绕过)。
- 排障副产:导入账号配额列显示 `—` 是因导入的 access_token 已过期 + 60s 失败缓存 TTL;系统会自愈(后台轮询/按需刷新会先刷 token 再查),新加的手动刷新按钮可即时解决。

## [manual-token-refresh-and-full-models-catalog] - 2026-06-12

### Features
- **人工「刷新 token」按钮(账号管理页)**:每行新增刷新图标(始终可用,与编辑/删除一致),强制该账号走一次 rt→at 上游交换。后端新增 worker `POST /accounts/{id}/refresh`(仅 loopback 挂载,镜像 reset)+ admin 顺序扇出 `POST /accounts/{id}/refresh`;前端 `useRefreshAccount` + 成功轻量反馈条(账号 + 新 token 有效期,下次操作自动消失)。**只刷新、不发 chat → 不触发风控**(见 no-chat-test-on-real-accounts),就是后台轮询/按需刷新本就在做的 OIDC 交换。
- **补全 `/v1/models` 目录**:`list_models` 原手列 3 个 stub(opus-4-8/sonnet-4-5/haiku-4-5),漏报 chat 实际可服务的 opus-4-6/4-7 与 sonnet-4-6。改为映射 converter 新权威常量 `MODEL_CATALOG`(6 个公告模型,带展示名/thinking 能力),`context_length` 由 `get_context_window_size` 推导(opus-4.6+/sonnet-4.6=1M,余 200k)。

### Design Rationale
- **token 刷新拆两条语义**(对抗审查 Skeptic#2/Architect#3 共识 medium):`force_refresh`【无条件】刷新供人工按钮(必须真打上游验证 rt);chat 403 retry 改走 `refresh_after_rejection`【CAS:仅当 scheduler 现存 access_token 仍是这枚被拒 token 才刷】,避免同账号 N 个并发 403 各无条件重刷一次、放大 token 交换/rolling refresh_token。判据用「token 是否被换掉」而非 expires_at 新鲜度(被拒 token 常仍看似新)。共享尾段抽成 `do_refresh_and_persist`(3 调用点复用,保锁内不变量)。
- **admin refresh 顺序扇出**:与 reset 并发扇出不同——refresh 有副作用(滚动 rt),故顺序问各 worker;**成功立即返回**(成功才滚 token,绝不刷到第二个 worker),失败(未滚 token,安全)记下首错继续问,扫完无成功才透出首错/404(审查 Skeptic#3/Architect#4:重复持有窗口里别让首个瞬时 502 掩盖另一 worker 的成功)。
- **`MODEL_CATALOG` 作公告子集**:`map_model` 子串匹配更宽松(也接受 legacy opus-4-5),目录只列完整支持(含身份规范化)的 6 个稳定模型。

### Bug Fixes
- **opus-4-5 身份泄漏(对抗审查 Architect#1)**:`map_model` 接受 `claude-opus-4-5` 但 `normalize::requested_model_identity` 无对应分支 → 路由到 Kiro 后真实代号(claude-quince)可能从 system identity 行泄漏。补 opus-4-5 分支(映射集合须 ⊇ 身份规范化集合)+ 回归测试 `every_advertised_model_normalizes_identity` / `legacy_opus_4_5_normalizes_identity`。
- **人工刷新失败如实反映(审查 Skeptic#1)**:worker refresh handler 失败时 `report_failure`——rt 永久失效→即标 invalid_refresh_token 禁用(仪表盘即时见死号),transient→计失败数(救号一键清),与 chat 路径一致。
- **前端刷新成功条 stale(审查 3 名 reviewer 共识)**:改本地态(任一操作开始即清),不再派生自 `refreshMutation.isSuccess`(后者会在其它 mutation 成功后残留旧账号);刷新按钮不再 gate `runtime.online`(runtime 降级时仍可手动刷新)。

### Notes & Caveats
- 重复持有窗口(组重分配中两 worker 暂持同账号)下,admin refresh 仍可能刷一个、另一个 30s sync 前用旧 rt;这是 30s 最终一致模型的固有性质(reset 同),非本次回归。
- 刷新成功仅展示 expires_at(不回传 token 明文);worker handler 仅 loopback 挂载(非 loopback 误配下禁用,避免绕过 router 鉴权)。

## [request-log-store-and-experimental-settings] - 2026-06-12

### Features
- **请求日志 store 层(③ 调试功能地基,零风险)**:gw-store 新 `request_logs` 表 + `RequestLog`/`RequestLogRow`/`RequestLogDetail`/`RequestLogFilter` 类型。`insert_request_log(log, cap)` 追加并**环形保留最新 `cap` 条**(`id <= max_id - cap` 裁旧,AUTOINCREMENT 不复用);`list_request_logs(filter, default_limit)` 按 account/model/success/时间窗筛选(不含大 payload);`get_request_log(id)` 取含完整 client+kiro payload 的详情。3 单测(往返/环形裁剪/筛选)。**捕获(worker)+ admin 端点 + 前端日志页仍待建**(留待后续:难点是流式 Drop 收尾穿透原始 body + 渲染后 Kiro body + 计时)。
- **实验性开关进设置面板(热控)**:`tools_in_prefix` / `cache_point` 从 env-only 改为可经设置面板热控。gw-kiro converter 改进程级 `RwLock<ExperimentalFlags>`(env 仍作启动默认,后向兼容),`KiroProvider::apply_hot_settings` 经 settings overlay 热改;gw-core `SystemConfig.experimental` + `SystemSettings` overlay。前端 SettingsPage 新增「实验性」卡(tools_in_prefix/cache_point 带说明)+ 调度卡补 `quota_poll_enabled` 复选框 + i18n(zh/en)。

### Notes & Caveats
- 请求日志 store 方法均为 `pub` 库 API,暂无调用方(捕获未接);非死代码(lib pub item),由单测覆盖。
- 实验开关在 worker **重启后有 ≤30s 窗口**先用 env 默认值,再被 DB overlay(30s settings 轮询)覆盖——与其它热设置一致;默认全 off 故窗口无害。

## [dup-tool-id-fix-and-ambient-quota-poll] - 2026-06-12

### Features
- **跨轮重复 `tool_use_id` 修复(正确性)**:新 `gw-kiro/src/converter/tool_id.rs::rewrite_duplicate_tool_use_ids`(🟢 借鉴 static_flow)。客户端(Claude Code)反复 auto-compact 后,同一 `tool_use_id` 可能在两个【各自已完成】的 assistant 轮里各出现一次;原 `validate_tool_pairing` 用 `HashSet` 静默去重,但 history 残留两个同 id 的 `ToolUseEntry` → Kiro 400 Improperly formed。修复=把跨轮重复 id(及按 FIFO 配对的 tool_result)改写成带 `__caiodup{N}` 后缀的唯一 id。**只作用于发往 Kiro 的 wire 报文**;在 conversationId 派生【之后】调用,身份链(与 worker `affinity_key_from_body` 同源)不漂移。
- **static_flow 式后台配额轮询(防封 ambient 流量)**:`worker` 自身每 240–300s(`jittered_secs` 时间熵抖动,免引 `rand`)兜底刷新一遍 `getUsageLimits`(只读,安全),**不依赖 /health 被打**。复刻真实 Kiro IDE 每 ~5min 一次配额轮询的 ambient 流量(纯反代账号两次聊天间对上游静默是最易被审计的指纹)+ 让配额面板无人看 dashboard 时也新鲜。被 /health(TTL=60s)在 floor(240s)内刷过的账号本轮跳过——不双倍打点;上游并发仍受 `quota_sem(3)` 节流。启停经 `SchedulerConfig.quota_poll_enabled` 热开关(设置面板可即时启停,无需重启)。

### Design Rationale
- **dup-id 改写绝不报错**:畸形/compact 残留(同轮活跃重用、孤儿 result)交既有 `pairing` 阶段兜底,FIFO 队列按出现序配对,绝不把可清理的输入升级成用户可见 400。
- **种子纳入所有现存 id**(tool_use + tool_result):后缀候选避开全部现存 id,防改写后撞上历史孤儿 tool_result 把它"复活"成错误配对。
- **配额轮询单驱动顺序 sweep**:不再 per-account spawn(避免上万账号周期性建上万个挂起 task);跳过 disabled/dead 账号(禁用即不再使用,不周期性打上游/试刷 token)。
- 对抗审查(Codex×3:Skeptic/Architect/Minimalist)逐条处理:A1 孤儿复活(加 used_ids 全 id 种子+专测)、A2 去掉所有报错改 FIFO、A4 身份链不变量加回归测试、B1 单 task 顺序、B2 跳过 disabled、B4 env→SchedulerConfig 热开关、B5 floor inline。

### Notes & Caveats
- **dup-id 跨 compact 漂移(已知,接受)**:改写后缀按"该 id 第几次出现"分配,确定性 given 可见历史。若客户端后续 compact 删掉较早那次重复,同一逻辑调用的序号变 → wire id 随之变(`dup__caiodup2`→`dup`),打断该点之后 Kiro 前缀缓存。只影响【已含重复 id 的罕见降级会话】,代价是一次缓存未命中(非 400),仍优于不改写直接 400。
- **轮询 shutdown 边界**:后台任务是 daemon(同既有 30s sync loop,无独立 shutdown 协调)。安全依据:`refresh_locked` 刷新 token 后**同步落库**(`merge_account_extra`),停机中途被 drop 也不丢 rolling token;sweep 唯一外部副作用是只读 getUsageLimits,被 drop 无损。
- `QUOTA_POLL_MIN_SECS`(240)与 `QUOTA_TTL`(60)耦合:前者作 stale floor 必须 > 后者,否则与 /health 重复打点。调任一常量时检查二者关系。
- **tools_in_prefix 保持默认关闭**:把工具定义 blob 挪进 history[0] 前缀想蹭 Kiro 缓存省积分,是"部分客户端工具调用失效"的根因(Kiro 只可靠地从 currentMessage 提供工具),金标准 static_flow 亦不用。env `KIRO_TOOLS_IN_PREFIX=1` 才开,默认 off,本轮未动。

## [settings-panel-and-egress-proxy] - 2026-06-12

### Features
- **系统设置面板(前端首个设置页)+ DB 持久 + 30s 热生效**:`settings` 单行表(JSON overlay)叠在不可变的 `system.yaml` 基线上;`GET/PUT /admin/api/settings`(GET=有效全量,PUT=部分 patch,`null`/空=删该 overlay 字段回 YAML 默认)。worker 30s 轮询用 `from_effective` 广播全量回 provider(`apply_hot_settings`)+ `scheduler.update_tuning` + cache_sim,**无需重启**。前端 `SettingsPage` 四组卡片(代理/缓存/调度/图像),只提交改动字段。
- **全局默认代理 + 每账号出口代理(全程同出口)**:新 `gw-kiro/src/resolver.rs::EgressResolver` 按账号解析出口 client,优先级 **账号 `extra.proxy` → 全局 `default_proxy` → worker 绑定源 IP**。KiroProvider 的 chat/refresh/quota/profileArn **四处**统一走 `resolver.client_for(account)`——同一账号刷新与发包同 IP(防封铁律)。代理 client 用 `reqwest::Proxy`(不绑 local_address);无代理才用 base(绑源 IP)。
- **每账号代理写入通道**:创建账号 `extra.proxy`;导入 `batch_proxy`(整批);编辑 `PATCH proxy_url`(定点 `merge_account_extra`,空串=清除,**绝不碰凭据**,规避 PATCH extra 整块替换坑)。前端三对话框对应加字段。
- **调度 `Tuning` 热更**:`RwLock<Tuning>` + `update_tuning`;`KiroProvider` 的 cache_billing/image_cfg 改 `RwLock` 承接热调。

### Design Rationale
- 不改 `ProviderFactory` 签名:`default_proxy` 经 `provider_cfg` JSON 注入 + `Provider::apply_hot_settings`(默认 no-op)——改动局限在 Kiro。
- 对抗审查(Codex×3)修复:①**代理写入边界 fail-closed**(`validate_proxy_url`:非法/含掩码占位的代理在 create/import/update/settings-PUT 一律 400,杜绝"配了代理却静默回退裸 IP");②**代理密码脱敏**(`redact_proxy_url`:GET 响应把 `user:pass@` 的密码段掩成 `***`,真值仍存库供 resolver 用);③**`deny_unknown_fields`**(设置 PUT 拼错 key 直接 400,不落库死 overlay);④Provider trait 出口契约文档更新(不再"出口由进程固定")。

### Notes & Caveats
- **默认代理热更的请求内一致性**:账号**无专属代理**、恰好在一次请求的 refresh 与 chat 之间被 admin 改了全局 `default_proxy` 时,这一次可能两步走不同出口(极窄窗口、自愈)。**用每账号专属代理(完全稳定)做严格按号隔离**,不要在有流量时热改全局默认代理。彻底冻结"请求级出口身份"需重构 provider/worker 出口流程,代价大、暂不做。
- `default_proxy` 仅 DB 管理(不读 `system.yaml` 基线);设置 PUT 的读-改-写非单事务(单运营者并发可忽略);前端数字字段暂无"逐字段重置回 YAML 默认"(可手动填默认值)——均为已知后续项。
- 进程拓扑(端口/每 worker 源 IP)仍在 instances.yaml,改动需重启(面板已注明)。

## [scheduler-hardening-and-image-compression] - 2026-06-11

### Features
- **模型能力过滤(opus 防误杀)**:`Provider::account_supports_model`(Kiro 实现 = `subscription_title` 含 FREE 拒 opus,未知放行)+ `AccountScheduler::acquire_where(谓词)` + 新错误 `NoModelSupport`(HTTP 400)。实测:纯 FREE 池打 opus 在选号阶段即拒,**零上游调用**——此前会 403 → 误判 TokenInvalid → 永久禁用健康号。
- **订阅档位数据闭环**:getUsageLimits 的 `subscriptionTitle` 回填 `extra.subscription_title`(内存锁内单字段合并 + DB 持久化);worker 启动/账号 sync 后对缺该字段的账号**预热**配额查询(只读,quota_sem=3 节流)。只导 rt 的老号也能收敛。
- **调度/冷却参数 config 化**(`system.yaml scheduler` 段),默认对齐 kiro.rs 生产:429 冷却 60s→**300s**、empty 冷却 20s→**60s**、empty 窗口 120s→**60s**、亲和 TTL 1800s;`max_failures=5` 保留(本项目 5xx 计数、kiro.rs 不计,语义不同)。
- **救号 reset 贯通**:`scheduler.reset_account`(清运行时禁用/冷却/计数,**配置禁用不动**)→ worker `POST /accounts/{id}/reset`(仅 loopback 挂载)→ admin 扇出端点 → 前端 HeartPulse 按钮(运行时禁用/有失败计数才显示)。
- **图像压缩移植**(🔵 kiro.rs/xkiro):四档阈值缩放 + **解码前 OOM 护栏**(1 亿像素/64MB 上限拦解压炸弹)+ 信号量背压 + spawn_blocking;失败一律回退原图。`system.yaml image` 段可配,接在 KiroProvider::chat 转换前。
- **`POST /v1/messages/count_tokens`**:router 本地估算(对齐 kiro.rs 默认路径),补 NewAPI/客户端探测兼容。
- `max_concurrency` 默认 1→**2**(serde/admin create/导入/SQLite schema/前端表单五处对齐 kiro.rs);DB 存量行不受影响,可 PATCH。

### Design Rationale
- 对抗审查(Codex×3)发现并已修复:①刷新回写「替换→置脏」非原子,30s sync 可在窗口内用 DB 旧值洗掉新 rolling token(新增 `update_account_dirty` 单锁原子化);②`flush_dirty_extras` 用旧快照整块落库+无条件清脏,会回滚并发刷新刚写库的新 token(改为逐账号持 refresh_lock、锁内重读+重查脏位);③刷新基底改用 scheduler 真值而非调用方旧快照(防用已作废 rolling token 刷新、防抹掉 merge 进来的字段);④全灭自愈带模型过滤(opus 请求不复活无关 FREE 失败号);⑤acquire 尝试预算与 max_failures 解耦(max_failures=1 时自愈后仍有机会重选);⑥reset 端点仅 loopback 挂载(非 loopback 误配不暴露无鉴权写操作)。
- 冷却状态仍纯内存(QuotaExhausted 重启复活,撞一次 402 重禁),与 kiro.rs 的差异已知、影响小,暂不持久化。

### Notes & Caveats
- 调度参数改动需重启 worker;热调控制面与 cache 参数热调一起留作后续(跨进程 plumbing)。

## [profile-arn-discovery] - 2026-06-11

### Features
- **动态 profileArn 发现(`ListAvailableProfiles`)** —— 🔵 对齐 static_flow,**kiro.rs 无此能力**(它依赖导入时凭据自带 profileArn)。企业/IdC 号的 chat 与 getUsageLimits 都强制要求 profileArn,凭据常不带;`gw-kiro/src/profiles.rs` 在缺失且无固定兜底(social/builderid)时,运行时 POST `q.{region}.amazonaws.com/ListAvailableProfiles`(跨候选区、翻页、runtime UA)发现并经 `ensure_profile_arn` 持久化进 extra。一次发现、后续短路。
- `Provider::discover_profile_arn`(默认 None)+ KiroProvider 实现 + worker 在 chat/配额前 `ensure_profile_arn`(发现失败不阻断,让上游自然 400 BadRequest,不惩罚账号)。

### Design Rationale
- 发现失败(403「not authorized」= 个人/Builder ID 层无此功能)时静默回退,由固定兜底或显式 profileArn 接管。复用 `persist_extra_field`(refresh_lock 互斥,与 subscription_title 同协议)。

### Notes & Caveats
- **实测验证(真号端到端,IdC Builder ID PRO 账号)**:
  - getUsageLimits → `KIRO PRO` 已用 9456/1000(945% 超额),subscription_title 回填成功;
  - opus 非流式 chat → `OK` + end_turn + usage(cache_read=683/input=6150);
  - opus 流式 chat → 完整 SSE 事件序列(message_start…message_stop);
  - count_tokens 真实内容 → 估算正常;模型过滤 opus 路由正确;BadRequest **不封号**(账号始终 enabled)。
- **发现**:Builder ID 个人号(clientName「Amazon Q Developer for command line」)的 `ListAvailableProfiles` 返回 403「not authorized」,须靠 `kiro_provider=builderid` 走固定 `BUILDER_ID_PROFILE_ARN` 兜底;原始 JSON 凭据(无 kiro_provider)目前需手填该字段。**待办**:对此类 403 自动回退 builder ARN,让裸凭据即插即用(本次保守未做,避免误判真企业号)。
- **运营提醒**:`PATCH /accounts/{id}` 的 `extra` 是**整块替换**(凭据轮换语义),漏字段会清空凭据——admin UI 编辑须回填全字段或改后端为字段级 merge(待办)。

## [thinking-xhigh-and-converter-hardening] - 2026-06-11

### Features

睡前 backlog 的安全增量(thinking 深度 + 缓存配置 + converter 400 兜底):

- **thinking 默认 effort `high`→`xhigh`**:Opus 全系默认 adaptive 思维链时,缺省 effort 之前是 `high`(实测仅产 ~43 字符桩推理),改为 `xhigh`(~3560 字符深推理),对齐 static_flow。`OutputConfig.effort` 改为 `Option<String>` + `effective_effort()` 缺省 xhigh(客户端带 output_config 但不带 effort 时也能正确回退,不再被 serde 默认强写 high)。
- **cache_sim 会话表可配**:`CacheConfig` 加 `sim_ttl_secs`(默认 300)/`max_sessions`(默认 4096),worker 启动时同步到全局 sim store(此前恒用编译期默认)。带 serde 默认,旧 system.yaml 仍可解析。
- **converter 400 兜底两项**(纯正确性,对齐 static_flow):空工具描述兜底为 `Client-provided tool '{name}'`(某些 Kiro 版本拒空描述 400);文档 `source.type="text"`(markdown/html/csv/txt)现 base64 编码后透传(此前静默丢弃)。
- **identity_override + 隐私策略注入**(`converter/history.rs`,逐字对齐 static_flow):每个请求 history[0] 始终注入,强制模型自认 Claude、不自曝 Kiro。⚠️ **实测发现不足**:用真号"你是谁"探针,模型注入后**仍答"我是 Kiro"**——Kiro 上游服务端身份压过客户端注入。单测证实注入确实落线缆;故此为 static_flow 平价(无害该留),**非身份检测银弹**。真实检测向量是结构化输出/desc/thinking 泄漏(见 docs 计划),需 38990 探针重放定位。

### Notes & Caveats

- 实测:gw-kiro 264 测试全绿(含 thinking effort/空描述/text 文档新单测);workspace 全绿。**未对真号发 chat**。
- **未做(故意推迟,见 docs/CONTEXT_LEGITIMACY_AND_TUNING_PLAN.md)**:① identity_override + 隐私策略注入(防身份检测封号)——**检测敏感,需 38990 探针重放验证后再上**,不盲发;② 更大的 converter 防 400 项(tool_use ID 清洗/文档去重限额/多模态工具 schema 兼容/stringified tool_result 解析);③ cache 三参数运行时热调端点(跨进程,需 router↔worker 内网跳);④ 前端补齐(请求日志/调度面板/强制刷新/批量操作)。

## [import-and-quota] - 2026-06-11

### Features

完整导入 KiroManager 账号 + 账号配额(积分)展示:

- **完整导入(防封核心)**:新增 `POST /admin/api/accounts/import` + 前端 `ImportAccountsDialog`,粘贴/上传 KiroManager 导出 JSON 一键导入。`gw-kiro/src/import.rs` 把导出字段映射到账号 extra——**关键是搬运真机 `machineId`**:此前只导 refreshToken,服务器据 rt 重派生一个不同 machineId(`sha256("KotlinNativeAPI/"+rt)`)→ 上游看到"激活设备 A、发包设备 B" = 双指纹 = 封号;完整导入消除这一根因。同时搬 clientId/secret/profileArn/region/kiro_provider。
- **智能合并**:已存在账号只补缺失身份字段;**token 字段(refresh_token/access_token/expires_at)仅创建时写,合并永不碰**(服务器拥有并轮换,导出里是旧值)。`machineId` 与已有不同时不覆盖、标 `machine_id_conflict` 提示。
- **账号配额展示**:`gw-kiro/src/usage_limits.rs` 移植 kiro.rs 的 `getUsageLimits`(只读),Provider 新增 `account_quota`。worker 侧 stale-while-revalidate 缓存(TTL 60s)+ 并发上限信号量,`/health` 带 `quota` 字段,前端账号表加"积分(剩余/上限)"列(吃紧标红)。

### Design Rationale

- **machineId 是防封关键,非端点**:KiroManager 导出 JSON 顶层带激活时的真机 `machineId`,完整导入原样搬入即可让发包指纹与激活一致。
- **token 合并即创建时写**:既兑现"不回退服务器已 roll token",又消除"导入读到旧值→并发刷新写新值→导入覆盖回旧值"的 TOCTOU 竞态(无需把合并塞进 DB 事务)。
- **配额只读 + 不阻塞 /health**:getUsageLimits 是只读查询(用户确认不招封号),后台刷新 + 缓存,/health 立即返回缓存值;信号量挡住"上百账号同时被查看"的 stampede。

### Notes & Caveats

- **配额刷新会 roll token**:后台刷新调用 ensure_credentialed,token 临期时会刷新(roll rt)。对已托管给反代的账号是预期行为;若同时还在用 KiroManager 管同一账号,两边 token 会发散。
- **machineId 必须合法**:导入只接受 64hex/UUID 形态的 machineId(非法形态丢弃,留空靠冻结按 rt 派生并提示),避免"谎报已设置但运行时仍派生"。
- **account_id 由 email 清洗派生**:可读但可能碰撞;智能合并用 user_id/email 稳定身份核对,不同真号撞同一 ID 时跳过(绝不合并两个真号)。
- getUsageLimits 走 `q.amazonaws.com`(同 kiro.rs)、发包走 `runtime.kiro.dev`,不同 host 但 machineId 一致(真实客户端本就跨 host)。
- 实测:workspace 测试全绿(gw-app 80 / gw-kiro 258,含导入映射/智能合并/碰撞/token保留/配额解析);admin-ui tsc+vite 通过;两个真号(POWER/PRO)经实时端点导入验证 machineId 落库后清理。**未对真号发任何 chat**。
- 对抗审查(codex Skeptic+Architect+Minimalist):修 3 个 high(account_id 碰撞合并/非法 machineId 谎报/token 覆盖竞态)+ 配额失败节流 + stampede 信号量 + 去 `backfilled` 泄漏字段名 + json 单一字符串形态;保留 Provider trait 默认方法(与现有 affinity_key 一致)。

## [kiro-wire-align] - 2026-06-10

### Features

gw-kiro 报文标准化 + machineId 防封 —— 逐字节对齐当前生产客户端 **static_flow**(commit 9051d71,已 `git fetch` 更新到最新):

- **主推理端点迁移**:`q.{region}.amazonaws.com` → `runtime.{region}.kiro.dev`(env `KIRO_RUNTIME_UPSTREAM_BASE_URL` / `KIRO_UPSTREAM_BASE_URL` 可覆盖)。kiro.rs 旧实现仍停在 `q.amazonaws.com` 且写死不可配,本项目对齐 static_flow 当前客户端。
- **新模块 `gw-kiro/src/headers.rs`**(报文单一事实源):主推理请求头逐字对齐——`accept: application/vnd.amazon.eventstream`、UA `os/darwin#24.6.0`/`nodejs#22.22.0`、**主 UA 去掉 `m/E`**(此前为残缺指纹)、条件头 `TokenType: EXTERNAL_IDP`(external_idp)/`redirect-for-internal`(internal provider)。chat.rs/token.rs/lib.rs 的 UA 全部收敛至此,消除版本漂移(此前 machine_identity 写死 `aws-sdk-js/1.0.0` 的陷阱)。
- **IdC 刷新 UA 对齐**:x-amz-user-agent 带版本 `KiroIDE-0.12.155`;user-agent 去掉 `api/sso-oidc`、补齐 os/node 版本。
- **machineId 冻结防封**(核心):`machineId = sha256("KotlinNativeAPI/"+refresh_token)`,而 refresh_token 是 **rolling** 的——不冻结则每次刷新 machineId 漂移 = 上游视为"同账号换设备" = 封号。`freeze_machine_id_if_absent` 在 `refresh_auth` **覆盖新 token 之前**用旧 token 派生值钉成显式 `machine_id` 并经 worker delta 持久化,设备指纹此后恒定。
- **账号 schema 扩字段**:暴露 `machine_id`(防封关键,可填真机指纹)、`auth_method`、`client_id`/`client_secret`(IdC 一等公民)、`kiro_api_key`、`kiro_version`,admin 表单可配置三种凭据(Social/IdC/API Key)。`FieldSpec` 加 `with_help` 提示。
- **脱敏补全**(安全):admin GET 脱敏此前只认 `token`/`secret`/`password`,漏掉 `kiro_api_key`(含 `key`)→ 明文泄漏。现加入 `key` 规则,所有 `*_key` 凭据字段一并脱敏(PATCH `***` 哨兵保留逻辑不受影响)。

### Design Rationale

- **为什么是 machineId 而非端点**:经 kiro.rs / static_flow / gw-kiro 三方代码交叉验证,KiroManager 导入(Social 号)易封、JSON 导入(常为 IdC)不易封的根因是 machineId 指纹漂移——Social 号注册绑真机 `vscode.env.machineId`,导出常丢失该值,派生哈希对不上;且 rolling token 让派生值持续漂移。端点 `q.amazonaws.com` 仍可用(static_flow 自身也用于 ListAvailableProfiles),属"不够像当前客户端"的次要指纹,非已证实封号主因。
- **冻结在 provider 的 refresh_auth**:此处持有刷新前的旧 token(派生材料),且返回的 extra 经既有 worker delta 持久化机制落库,无需新增跨层管道。
- **headers.rs 单一事实源 + golden 单测**:把"对齐 static_flow"显式化为逐字断言的单测(`streaming_ua_matches_static_flow_exactly` 等),static_flow 再更新时测试即对照点,防止悄悄漂移。

### 对抗审查加固(codex Skeptic + Architect,CONTESTED → 处置)

- **撤下未实现的 API Key 路径**(high):此前 schema 暴露 `kiro_api_key` 但调用链 OAuth-only(refresh_auth 强制 refresh_token、chat 只读 access_token),会"加载通过、首请求才报错"。现从 schema 移除,`validate_account` 明确要求 refresh_token 并提示 API Key 暂不支持。
- **profileArn 固定兜底**(high):端点迁 runtime.kiro.dev 后,缺 profileArn 可能被拒/命中错误 profile。port static_flow 的 `fixed_profile_arn`:按 `kiro_provider`(github/google→social 共享 ARN;builderid→builder ARN)兜底,显式值优先,企业号仍省略(动态 ListAvailableProfiles 未实现)。
- **IdC 刷新报文逐字对齐**(medium):补 `accept: */*`、头序对齐 static_flow `refresh_idc`,并抽到 `headers::apply_idc_refresh_headers` + golden 测试(此前 golden 不覆盖 IdC 刷新)。
- **machineId 误判修复**(medium):`is_api_key_credential` 改为必须有非空 `kiro_api_key`(仅 `auth_method=api_key` 标签不够),避免误配账号落随机指纹再被冻结固化。
- **provider 撞名修复**(medium):`redirect-for-internal` / profileArn 兜底改读专用键 `kiro_provider`(`extra["provider"]` 会被 serde flatten 吃到 `Account.provider` 顶层字段)。
- 安全:admin 脱敏补 `key` 规则,`kiro_api_key` 等 `*_key` 字段不再明文经 GET 泄漏(Architect 复核已确认修复)。

### Notes & Caveats

- 冻结的局限(如实声明):若账号导入时 refresh_token 已 roll 过(KiroManager 导出的是当前而非原始 token),冻结值仍 ≠ 真机指纹,只是阻止"继续漂移";彻底规避需在账号里填真机 `machine_id`(schema 已支持)。Social 号首次刷新时 info 日志提示。
- `kiro_version` 可 per-account 覆盖,但 OS/Node/SDK 版本写死:改它而不同步会造成现实不存在的指纹组合(schema help 已警示),非必要勿动。
- 端点统一 `runtime.{region}.kiro.dev`:暂未保留 static_flow 对 gov region 的 `q-fips.*` 特殊 host(当前无 gov 账号;需要时经 env 覆盖)。
- "对齐 static_flow" 靠 vendored 常量 + golden 单测(注明源 commit 9051d71),非自动同步:static_flow 升级 client/SDK 版本时需手动比对更新(已 `git fetch` 到最新)。endpoint-family 抽象(MCP/usage/profile 路径)暂缓(当前无 MCP 上游路径)。
- 实测:gw-kiro 测试(含 headers golden 10 + machineId 冻结/收紧 + IdC golden);workspace 387 全绿,零警告。报文未对真实上游发包验证(凭据/风控约束),靠 golden 单测对照 static_flow 源码保证字节一致。

## [rename] - 2026-06-10

### Features

- 项目更名:**kiro-gw → Claude All in One**。二进制 `claude-all-in-one`、admin UI 标题/登录页、文档、示例配置、前端包名全部跟随;localStorage 键前缀 `kiroGw*` → `caio*`(已登录会话需重输一次 admin token)。

### Notes & Caveats

- 内部 crate 名(gw-core/gw-kiro/gw-store/gw-app)是实现细节,未随名;上游 provider "Kiro" 的指称(gw-kiro、账号示例 kiro-01)保留——那是上游名,不是项目名。
- 启动命令换为 `./target/debug/claude-all-in-one --mode ...`;旧名二进制已从 target 删除,防误用旧产物。

## [admin-v1.1] - 2026-06-10

### Features

后端完善四件套 + 停机数据安全:

- **router 负载计数修正**:删掉只增不减的累计计数器,活跃负载改为从亲和表派生(钉在该 worker 上的未过期 session 数)。session 过期负载即回落,空闲 worker 能重新承接新会话;亲和指向已下线 worker(拓扑变更)时丢弃重选。
- **admin `/accounts/runtime` 并行聚合**:逐 worker 拉 `/health` 由串行改 `join_all` 并发,最坏耗时 ≈ 单个 2s 超时,不再随离线 worker 数累加。
- **优雅停机**:router/worker 响应 SIGTERM/Ctrl-C(`with_graceful_shutdown`)——停止接收新连接,在途请求(含流式 SSE)自然跑完;排空不设上限,硬截止由 supervisor 兜底(docker 默认 10s、systemd `TimeoutStopSec`)。
- **worker 停机前脏 extra 落盘**:`flush_dirty_extras` 由 30s sync 循环与停机排空共用——「刷新成功但 DB 回写失败」的 rolling refresh_token 在进程退出前有最后一次落盘机会,不再依赖下轮 30s 重试(进程退出即 drop)。
- **`--features embed-ui` 单二进制部署**:rust-embed 把 `admin-ui/dist` 嵌进二进制;SPA 客户端路由兜底回 index.html;vite 哈希资产 `immutable` 永久缓存、index.html `no-cache` 保发布即生效。feature 默认关闭(fresh clone 无 dist 仍可编译),关闭时维持原 ServeDir 磁盘读取。

对抗审查(Skeptic+Architect)加固:

- **router 故障转移**(Architect high):worker 进程挂掉但仍在配置里时,原实现会让钉住它的 session 502 长达 30 分钟。现 `send()` 连接失败 → 丢弃指向故障实例的亲和、在其余 worker 里重选重发一次(请求未送达,无重复送达风险),亲和重钉到备选。活体实测:打挂掉的 instance 0 → 自动转移 instance 1 拿到正常响应,第二个请求直达不再转移。
- **停机等待在途 usage 落库**(Skeptic medium):SSE 收尾的 usage/quota 落库是 Drop 里 detach 的 spawn 任务,graceful shutdown 只等响应体不等它们。新增 `PendingWrites` RAII 登记,排空后 `wait_idle`(5s 上限)等这批任务收尾,最后一批计费记录不再随 runtime 关闭静默丢失。
- **亲和全表清理节流**(Skeptic medium):O(n) retain 从每请求改为 ≥5s 一次(`cleanup_if_due`);命中路径补 O(1) 精确过期判断(不依赖清理兜底)。几十万 session 时 router 延迟不再随表大小线性放大;代价是负载统计里陈旧条目最多滞留 5s。

### Design Rationale

- **负载从亲和表派生而非独立计数**:单一事实源,过期清理(retain)天然让负载回落,无需配对的 increment/decrement(后者漏一边就永久漂移——正是被替换实现的病根)。代价是「负载」语义为活跃 session 数而非在途请求数,对会话粘性网关是合理代理指标。
- **embed-ui 用 feature 门控而非无条件嵌入**:rust-embed 编译期要求资产目录存在,`dist` 又被 gitignore;无条件嵌入会让 fresh clone 直接编译失败。release 部署构建用 `cargo build --release --features embed-ui`(需先 `bun run build`)。
- **停机落盘只兜「已脏」数据**:正常路径刷新成功即同步落库(admin-v1 已做),脏位只在落库失败时存在;停机 flush 是窄窗口兜底而非主路径。

### Notes & Caveats

- 实测链:负载回落/重选/故障转移/PendingWrites 有单测(注入 now / in-memory store);内嵌单二进制从无 dist 目录起服 curl 验证(index/哈希资产/SPA 兜底/缓存头/admin api);SIGTERM 实测日志+1s 内干净退出;故障转移双 worker 活体实测(挂 0 → 转移 1 → 亲和重钉)。
- 「活跃负载」语义是亲和 session 数,不是在途请求/流数(Architect medium,接受):一个 session 开多条长 SSE 只计 1。换真实在途计数留给下阶段(届时需拆亲和表双职责)。
- 负载均衡对「无 session_id」请求不计负载(无亲和记忆,一发即走)。
- worker 运行态(冷却/封禁)仍是内存态,重启即清——优雅停机不改变这一点(既有设计:持久化冷却反而会在重启后误恢复过期冷却;DB 只存配置)。
- embed-ui 与默认 ServeDir 两条路径的缓存头有漂移(ServeDir 无 Cache-Control;Architect low,接受):ServeDir 仅用于开发迭代,生产走 embed。

## [admin-v1] - 2026-06-10

### Features

admin 控制面完整落地(嵌入 router 进程,`/admin` SPA + `/admin/api/*`,单一 `admin.token` 鉴权,常量时间比较):

- **用量看板/用量页**:总览卡、按模型、按客户 apikey 三维度;时间窗(近 7/30 天/全部/自定义起止)+ 按 key 筛选;未归属流量单列桶。
- **API Keys 管理**:列表(掩码+复制)、新建(服务端 `sk-gw-<uuid4>` 生成或导入自定义 key,字符集 `[A-Za-z0-9._~-]{8,128}`)、备注、启停(逐请求查库即时生效)、删除(usage 历史保留)。
- **账号管理(yaml → SQLite)**:`accounts.yaml` 启动幂等导入(只播种,绝不覆盖已 roll 的 token),此后 DB 是配置事实源;admin CRUD;worker 30s 周期 sync——增删改免重启生效;**token 刷新成功先回写 DB**,rolling refresh_token 重启不丢;运行态(冷却剩余/禁用原因/并发占用)由调度器快照经 worker `/health` 暴露,admin `/accounts/runtime` 聚合;凭据响应一律脱敏保尾 4 位。
- **分组**:groups 表(色板/备注/账号数/key 数);账号与 key 都可归组;删组成员转未分组(事务),不级联删。
- **按客户限额(计费 v1)**:`api_keys.quota_tokens/used_tokens`,UsageSink 落库时锁内累加(口径 input+output,未归属不计);鉴权 SQL 内算 `over_quota`,超额回 429 `rate_limit_error`;admin 可设额/清除/重置已用,即时生效。

### Design Rationale

- **DB=配置事实源,worker 内存=运行态**:配置(账号/key/组/限额)进 SQLite 由 admin 管理;冷却/封禁/并发等运行态留在调度器内存经 HTTP 快照暴露——避免高频运行态写库,也避免重启后误恢复过期冷却。
- **sync 翻转语义**:配置 `disabled` 仅在**翻转**时触碰运行态(→false 视为 admin 显式复活)。同值周期 sync 绝不洗掉风控冷却/封禁,防止 30s 轮询反复"救活"被风控的账号。
- **统计读连接分离**:admin 全历史聚合走独立只读连接(WAL 并发),控制面再慢也压不到数据面的鉴权与计费落库。
- **限额单位用 token(input+output)**:v1 求简单可解释;加权成本(cache 折扣/模型价差)留到计费表达式阶段,届时只需替换累加口径。
- **busy_timeout 必须最先设**:router/worker 并发启动同一 WAL 库,否则抢锁直接 "database is locked" 即死。

### Notes & Caveats

- 明文 key 仍兼任身份/PK/usage 归属键:key 轮换、同名重建会合并历史归属。引入稳定 `key_id` 规划在加权计费阶段一并做。
- keys/accounts 列表无分页,客户量大(千级)后需要加。
- 限额检查在鉴权时读、settlement 时写:并发在途请求可少量超额(误差 ≤ 在途并发量),对人为限额场景可接受。
- admin 改账号生效有 ≤30s 的 sync 延迟(UI 已提示)。
- 部署:`admin-ui/dist` 由 router 运行时读取(改前端需 `bun run build`);单二进制内嵌(rust-embed)待做。
