# Changelog

## [cursor-turn-trueup] 末轮补差:turn 合计对齐上游真总量 — 2026-08-18

用本机抓包先钉死了一条前提,再据它做补差。**做完这条,之前提的"手填地板常量"方案
可以彻底扔掉** —— 地板本来就含在真总量里,补差自动吸收。

### 前提:`turn_ended.input` 是本 turn **所有内部模型调用之和**

三次受控实测(钩 `cursor-agent` 的 http2 读原始帧;三个文件各 6 字节,所以工具轮
之间上下文只涨百来 token):

```text
工具调用 0 次 → 内部调用 1 次 → turn_ended.input = 16,664   (16,664/次)
工具调用 1 次 → 内部调用 2 次 → turn_ended.input = 28,550   (14,275/次)
工具调用 3 次 → 内部调用 4 次 → turn_ended.input = 67,786   (16,946/次)
```

**反证**:若它是"最后一次调用的上下文",三次应当都 ≈16.7k。实际是 1× / 1.7× / 4.1×,
正比于内部调用数。

同时确认(这条是坏消息但确定):三次都**只有一个** `turn_ended` 帧,协议里没有任何
per-call 用量帧 —— `stepCompleted` 只带 `step_id` + `duration_ms`。**所以"每个调用方
HTTP 轮的真值"在协议层不存在**,换 wire 驱动也拿不到。之前我说 `turn_ended` 是
"没人捡的金矿",只对了一半:它是真值,但粒度是整个 turn。

### 做法

`CliConv` 加 `billed_input`(会话级累加器,一个 CLI 进程 = 一个 turn,天然按 turn 归零):

- 每个自估轮(`finish_tool_use`)把报给调用方的 `input_tokens` 累进去
- 末轮拿到真值后:`上报 = max(真总量 − 已报之和, 本轮自己的估值)`

`max` 是护栏:前几轮若高估导致差额为负,末轮至少按自己那份走,不会出现 0 或负数。

**缓存按同一比例缩放,不是硬夹。** 补差后的 input 通常小于 turn 总量,而真值的 cache 是
整个 turn 的 —— 不缩放就会 `cache > input`,线缆侧 `input − cache` 归 0,客户又看到
「输入 0」(用户 08-17 报的那个)。按比例缩放同时保住**折扣率**。
`real_cache_read_tokens` **不缩放**:它是对账列,记上游实际做了多少命中,整个 turn 的
真值归在末轮这一条,前几轮是 0,合计正确。

### 为什么记末轮而不是回改历史行

前几轮的记录已经写库并上报给计费侧,回改会让下游对账错乱。记末轮只让**一条**记录
偏大,而 turn 合计正确 —— 钱是按合计收的。

### 影响

修的是**少收**:中间轮系统性偏低(漏 Cursor 注入的约 16k/次地板),差额此前无人回收。
一个 4 次内部调用的 turn,此前约少报 3 万 input token(其中大部分走缓存价 0.1×)。

隐藏的内部调用(`auto` 的模型选择器、自动摘要等)也计入真总量 —— 客户没直接触发,
但我方账号确实被扣,真值口径下自然算进去。这是定价决策,已确认按真值收。

### 验证

- `cargo test -p gw-cursor`:**261 passed / 0 failed**(+3)
- 新增三条:①turn 合计精确等于真总量;②前几轮高估时退回本轮估值(不得 0/倒挂);
  ③单轮 turn 补差是恒等变换
- `cargo check --workspace --all-targets` 干净

## [cursor-usage-wire-truth] 线缆取证钉死 usage 口径 + 补 cache_write — 2026-08-18

本机装了 `cursor-agent 2026.08.11-e8db854`,钩住它的 http2 把
`agent.v1.AgentService/Run` 的服务端帧原样落盘,**同一次调用两侧对照** —— 一次把
反复改口三次的 usage 口径问题定死了。

### 取证

```
线缆 InteractionUpdate.turn_ended(field 14),13 字节:
  field 1 input_tokens       = 16664      ← 总量
  field 2 output_tokens      = 34
  field 3 cache_read_tokens  = 12160
  field 4 cache_write_tokens = 0
  field 5 reasoning_tokens   = 33
CLI 的 result.usage:
  {"inputTokens":4504,"outputTokens":34,"cacheReadTokens":12160,"cacheWriteTokens":0}
  16664 − 12160 − 0 = 4504  ✓
```

CLI 源码(`1931.index.js`)就是这么算的,`result` 事件发的正是这个减过的对象:

```js
case "turnEnded": { ... ke = f({inputTokens: Number(e.inputTokens), ...}) }
f = e => ({...e, inputTokens: Math.max(e.inputTokens - e.cacheReadTokens - e.cacheWriteTokens, 0)})
... {type:"result", ..., usage: ke}
```

**结论:同一个字段名在两条路上口径相反。** 线缆 `input` 是总量;CLI `result` 的
`inputTokens` 是**已减掉 cacheRead 与 cacheWrite 的未命中量**。

### 修的两处

**① CLI 真值轮少报了一个缓存量的输入。** `usage_from_result` 把那个未命中量当总量填,
而 `ChatUsage::input_tokens` 的契约是总输入。现在
`total = inputTokens + cacheReadTokens + cacheWriteTokens`。

**② `cache > input` 从来不是异常,旧代码把合法缓存丢了。** 生产 39% 的记录
`cache_read > input_tokens` —— 命中大于未命中量是**常态**(取证那次 2.7 倍)。
旧逻辑判它"上游口径异常"并置 0,等于扔掉客户应得的折扣,同时输入也少报。丢弃逻辑删除。

**③ 线缆侧补读 field 4/5。** `run.rs` 原来只解 `1..=3`,三元组换成 `WireUsage`
结构体,`cache_write` 落到 `cache_creation_tokens`(2026-08-18 抽查生产 6,430 条
该列恒 0,即这块一直没收)。`reasoning_tokens` **不入账** —— 实测
`output=34 / reasoning=33`,它是 output 的子集,单独计会重复收费。

### ⚠️ 这条口径我改口过三次,三次的错法都记在 `usage_from_result` 的文档里

1. **08-17 第一版**:据「39% 的记录 cache > input」推断是未命中语义,改成
   `up_in + cached`。方向对,但**当时没有证据**。
2. **08-17 撤回**:我用「10 字符的提示词不可能有 6779 token 的新增输入」把自己说服了,
   判定 6779 是总量并撤回。**那个论证是错的** —— 新会话的未命中量本来就含
   AGENTS.md + tools + Cursor 注入的服务端 system(实测 16k 量级)。用"这个数看起来
   太大"反推字段语义,和用聚合统计反推一样不成立。
3. **08-18 定案**:钩 http2,两侧同时读同一次调用。**只有这种取证能定语义** ——
   一侧的数字无论多少条都推不出另一侧的口径。

### 影响:又是涨价(修的是少收)

以取证那一次为例(¥0.4/M 输入、缓存 0.1×):
旧 `4504 × 0.4 = 1,801 µ¥` → 新 `4504 × 0.4 + 12160 × 0.04 = 2,288 µ¥`,**+27%**。
只作用于**真值轮**(约占 CLI 请求 28%);自估轮不经这条路。

### 验证

- `cargo test -p gw-cursor`:**258 passed / 0 failed**
- 重写 4 条锁旧口径的测试;新增 `缓存写入计进总量且落对账列`、
  `缓存大于未命中量是正常的不得丢弃`
- 顺带删掉 `SsePhase::sim_total` 死字段(基准改 `ctx_base` 后没人读)
- `cargo check --workspace --all-targets` 干净

### 副产物:一套可复用的抓包装置

`scratchpad/hook.js` —— `NODE_OPTIONS=--require` 注入,patch `http2.connect`,
只对 `AgentService/Run` 的流 append 落盘,不改任何字节。配沙箱 HOME
(`auth.json` 只需 `{accessToken, refreshToken}`)即可用池号跑任意实验。
本次一并确认:出口主机 `agentn.global.api5.cursor.sh`(与我方 `lib.rs` 已有的一致)。

## [cursor-cli-ctx-basis-2] 首阶段基准:补 system + 去掉一处重复计费 — 2026-08-18

紧接上一条。部署后做了一次**受控两轮工具回路实测**(自己发,cursor 流量当时已停),
结果暴露同一个基准还差两块。

### 实测

```
第 1 轮(新会话,自估)  客户侧输入 3      缓存 45   → DB input 48
第 2 轮(工具轮,真值)  客户侧输入 16,366 缓存 455  → DB input 16,821
```

第 2 轮是 `end_turn`,CLI 进程退出、`result` 给了真值 —— **这个新会话的真实首轮输入
是 16,821**,而我方第 1 轮报 **48**。工具轮的四个数量级修好了,首轮还差 350 倍。

### 缺陷 1:新会话基准漏了 system(我方能算的那半地板)

差的 16.5k 是**每请求的固定地板**:我方写盘的 AGENTS.md(调用方 system)+ Cursor
注入的服务端 system(CHANGELOG 早前实测约 26k)。`first_in_tally` 只算 prompt + tools,
把 system 排除在外 —— 那条注释("算进本轮新增输入会重复计费")是为**旧基准**
(`fresh_in + cache_read`)写的,基准改成上下文总量后它就反了:system 每轮都被 CLI
重读、上游照样计费,漏算就是我方贴钱。现在计上。

Cursor 注入的那部分我方算不出来,仍缺;它是"每请求固定地板",属于定价要考虑的输入。

### 缺陷 2:续会话基准重复计了 tools 与 prompt(我自己上一条引入的)

上一条写成 `ctx_base = sim_total` **同时**叠 `first_in_tally`(prompt + tools),而
`sim_total` 的口径本来就是 `system + tools + 全部历史`
(见 `fingerprints_from_context`)。tools 对 Claude Code 一类客户能有上万 token,
等于每条会话的首轮凭空多收一份工具清单。

修法:抽出纯函数 `first_phase_basis(resumed_sim_total, system, prompt, tools)`,
**两条分支互斥、绝不相加** ——

| 会话 | ctx_base | in_tally |
|---|---|---|
| `--resume` 老会话 | `sim_total` | **空** |
| 新开/重铺 | 0 | `system + tools + prompt` |

### 验证

- `cargo test -p gw-cursor`:**257 passed / 0 failed**(+2)
- 新增两条:①续会话 `in_tally` 必须为空(锁重复计费);②新会话基准必须含 system
- `cargo check --workspace --all-targets` 干净

## [cursor-cli-ctx-basis] CLI 驱动:自估轮 input 基准改累加式 — 2026-08-18

用户从生产账单发现:「很多特别低的缓存命中」。查出来是**同一个基准错误的两个表现**,
其中主因是我上一批为躲另一个坑而引入的。

### 病征(生产实测,grok-4.6 同一条会话)

```
id       输入 i    输出 o   缓存 c    占比      耗时
1585721  121,483  19,487  115,409  0.9500  131.5s   ← 有上游真值的轮
1585725       10   1,145       10  1.0000   14.6s   ← 紧接着的工具轮
1585727       10     802       10  1.0000    0.06s
1585730       10     460       10  1.0000    0.07s
1585737       10     308       10  1.0000    0.08s
1585739       10   1,191       10  1.0000    0.07s
1585744       10     529       10  1.0000    9.9s
1585749       10     490       10  1.0000    0.08s
1585766  132,812     442  126,171  0.9500   42.1s
```

**病 1:`占比` 恒等于 `cap_ratio`,缓存列不含任何信息。** 它就是 `cap × 输入` ——
模拟器算出来的真实命中值恒大于夹限,永远被削掉。

**病 2(主因):基准本身错了。** `10` = 喂回的 `tool_result` JSON 约 40 个 ASCII 字符。
上游每轮读的是**整个会话文件**,不是我方这轮送的那几十字节 —— 真实上下文 12 万,
我方报 10,差四个数量级,客户按 10 个 token 付输入。

全量统计(1,513 条 cursor 记录):**782 条(52%)报 input < 1000**,而同批会话里
其他轮显示 10k–100k;其中 **183 条(12%)`cache == input`**(占比 1.0)。

### 根因:两个方向的误差,我先后各踩一次

`input` 基准有三个候选,前两个都错:

| 基准 | 错法 |
|---|---|
| `sim_total`(上一批用过) | 标定 10 条样本中位 **0.741**(少收 25%),重铺轮离群 **26.6 倍**(超收 27 倍) |
| `fresh_in`(这一批之前用) | 工具轮只量喂回的 tool_result,**低估四个数量级** |
| **`ctx_base + fresh_in`(现版)** | 只跟"我方实际喂过什么"走 |

两个方向的误差其实是**同一个判据的两侧**:上游那条会话里到底装着多少。

- `--resume` 老会话 → 上游确实有调用方给的那段历史 → 从 `sim_total` 起算
  (偏低约 25%,少的是 Cursor 注入的服务端 system ≈26k,方向安全)
- **新开会话(含重铺)** → 调用方照旧带全量历史,但我方只把最后一段用户输入喂了
  上去,那段历史上游根本没有 → 从 **0** 起算。标定里那条 26.6 倍正是这种轮次
- 续阶段 → `上一阶段的基准 + 上一阶段的输出`,会话又长了这么多

累加式对上游真实吃进去的量,重铺后自动归零重算,两个方向的病因都被挡住。

### 顺带修:`round` 让 cap 在小基准上失效

`(input × cap).round()`:input 10、cap 0.95 → 9.5 → **round 得 10** → 占比 1.0000,
余量归零,等于没夹。改 `floor`。上面那 7 条 `10/10` 就是这么来的
(两处都改:`estimated_usage` 与 `fallback_cache_from_sim`)。

### 影响:这是**涨价**

工具轮从"按几十 token 收输入"变成"按真实上下文收输入(其中约 95% 走缓存价 0.1×)"。
以 1585725 那条为例(输出 1,145):¥0.00143 → 约 ¥0.0085,涨 6 倍。

这个方向是对的 —— 上游每次内部调用确实重读整个会话,kiro 通道也是每轮报全量上下文
(它每轮重传历史,被迫如此)。**两条通道的客户账单必须可比**,否则客户拿 cursor 的
单价对 kiro 的账会对不上。但它确实是客户可见的价格变化,不是纯 bug 修复。

### 验证

- `cargo test -p gw-cursor`:**255 passed / 0 failed**
- 新增两条回归测试:①自估轮基准 = `ctx_base + 新增量`(锁四个数量级那个洞);
  ②新开会话首阶段 `ctx_base` 必须为 0(锁 26.6 倍超收)
- 既有那条「夹到 cap 而非全额」的测试改用 `floor` 期望值
- `cargo check --workspace --all-targets` 干净

### 遗留

- 真值轮拿到的仍是**整个 CLI 进程的累计值**,不是那一轮的量 —— 这是结构性的
  (`result` 只在进程退出时发一次),换 wire 驱动也一样(`turn_ended` 只在 turn
  结束时来,工具轮我方在 `tool_use` 处 break,上游根本不发)。真正的出路是官方
  事后对账 API(`/teams/filtered-usage-events`,按 `conversationId` join),未验证。
- wire 路径只解 `turn_ended` 的 field 1..3,**field 4 `cache_write_tokens` /
  field 5 `reasoning_tokens` 全丢**。生产抽查 6,430 条 `cache_creation_tokens` 恒 0。

## [cursor-cli-cache-calibration] CLI 驱动:接上计费旋钮 + 两次自我修正 — 2026-08-17

接上一批。用户指出上一批一个根本性口径错误:

> 「你这里说缓存＜输入,这是 openai 的计算方式,anthropic 协议下,输入是指抛去缓存的
> 输入,你看 kiro,那个是对的」

对的。上一批把「`cache < input`」当健康态,那是**落库口径(total)**;线缆是
**Anthropic 口径(input 已剔除缓存)**。顺着这条线查下去,发现 cursor 通道**从来没有接过
kiro 那套计费闸门** —— 三个旋钮、sim store 的 TTL,全都没接。

### 缺陷 4:`apply_hot_settings` 在 cursor 上是空实现

`gw-kiro/src/usage.rs::reported_cache_read` 那三个旋钮
(`cache_read_multiplier` / `cache_cap_ratio` / `cache_floor_ratio`)cursor 侧根本不存在,
面板改了没有任何效果。

修:`cache_sim.rs` 移植 `CacheBilling` + `reported_cache_read`(与 kiro 同公式:
`frac = hit/sim_total`,`reported = clamp(frac × total × mult, total×floor, total×cap)`),
`gw-cursor/src/lib.rs` 实现 `apply_hot_settings`(仅present字段)并让
`hot_settings_supported()` 返回 true —— 否则按上一批 `claude-dario` 那个 no-op 的教训,
面板会显示"已生效"而实际是哑的。

**⚠️ `multiplier` 在 cursor 路径上是死参数**。标定实测 `frac` 落在 0.79~0.998,
乘 1.8 必然越过 cap —— 有命中的样本 4/4 全部撞 cap。要调让利幅度只能动 `cap_ratio`。
(kiro 上 multiplier 有意义是因为它每轮重发全量历史,frac 分布低得多。)

### 缺陷 5:cursor 的 sim store 从未接 TTL 配置

`worker/mod.rs` 的启动与 30s 热重载**只同步 `gw_kiro` 的 store**,cursor 那份用编译期
常量 300s,而线上配的是 1800s —— **短 6 倍**,大量会话被误判冷启动。

`CacheSimStore` 当时连 setter 都没有(`set_ttl_secs` / `set_max_sessions` 本次补),
所以这个值在生产上从来改不动。第一条标定样本就是证据:上游真值命中 8180,
我方 `sim_raw_hit=0`。

### 缺陷 6:真值轮丢弃上游 cache 后不回落模拟值 —— 用户报的现象

上一批对不可信的上游 cache 采取「丢弃」,但**丢弃后没有回落到模拟器**。于是:
自估轮有缓存、拿到真值的那一轮缓存为 0 —— 客户看到的就是
**「突然完全没有任何缓存命中」**。线协议侧早就有这道回落闸(`chat.rs`),CLI 驱动没有。

实测代价:一条 `input=85951` 的记录,上游 cache 162,176 被丢弃归 0,而模拟器手里有
57,336 —— 客户被收 ¥0.02744,应收约 ¥0.0103,**多付 2.4 倍**。

修:`fallback_cache_from_sim()`,真值轮 cache 为 0 且模拟器有命中时回落,并夹到
`input × cap_ratio`。

### ❌ 我引入又撤掉的两个缺陷(必须记下)

**(a) 自估基准误用 `sim_total`。** 理由当时是"和 kiro 对齐"——错了:kiro 每轮重发
全量历史,所以它的上下文总量 == 上游 input;CLI 驱动不重发(历史在 `--resume`
会话文件里)。生产标定 10 个样本 `sim_total / upstream_input`:
min 0.042 / P25 0.609 / **median 0.741** / P75 0.847 / max **26.633** ——
9 个低估(系统性少收约 25%),而那个 26.6 倍出现在**重铺轮**(超收 27 倍)。
已退回 `fresh_in`(本轮真实喂进 CLI 的量)。

**(b) 缓存夹到 `input` 本身。** 结果 `cache == input` → 线缆 `input − cache = 0`,
既是"本轮 100% 命中"的假断言,又让整轮按 0.1× 计。生产实测
**57/63 条(90%)全额折扣**,740,153/822,784 = 90.0% 的输入按缓存价走。
**造成约 20 分钟(15:26–15:47)的真实收入损失,不可追回。**

坏估计的根因值得单独记:我拿**真值轮**的样本比例(19%)去推全体,而量在**自估轮**——
自估轮 `input` 只量本轮新增(几十~几千),模拟命中却覆盖全上下文(几万),两者量级差
几十倍。**样本选择偏差**:少数派轮次的比例不能外推到多数派轮次。

修:两处(`estimated_usage` / `fallback_cache_from_sim`)统一夹到 `input × cap_ratio`。

### 标定观测(仅记录不计费)

真值轮打一条 `cursor-cli:缓存标定样本`,同时记上游原始值、我方计费值、模拟命中、
模拟总量。**坑**:第一版读的是 `usage.real_cache_read_tokens` —— 那已经被丢弃逻辑清零,
于是"上游报了但被我丢弃"被记成"上游真值=0",算出假的 `ratio=0.000`。
改为在 `usage_from_result` **之前**捕获原始值。

`real_cache_read_tokens` 是对账列(admin `usage.rs` 拿它算真实成本),
**模拟值一律不写进去** —— 生产验证 `rc=0` 全部正确。

### 生产验证(`cache-cap-20260817`)

| 指标 | 报障时 | 上一版(b) | 现在 |
|---|---|---|---|
| 零缓存(用户报的现象) | 频发 | 0 | 0 |
| 全额折扣(整轮 0.1×) | — | 57/63 = 90% | 0/3 |
| 客户侧输入 | 常显示 0 | 常为 0 | 35 / 13 / 33,531 全为正 |

两条小额记录占比 **0.9499 / 0.9494**,精确卡在线上 `cap_ratio=0.95`,证明夹限生效。
`caio-router` / `caio-worker0` 全程未重建(`--no-deps`)。

### 遗留

- `cacheWriteTokens` / `reasoningTokens` 仍未计入(`cache_creation_tokens` 恒 0)。
  前者对应 Anthropic 的 `cache_creation_input_tokens`,通常比普通输入更贵 —— 计不计是定价决策。
- 生产 `floor_ratio=0.75` 意味着**冷启动零命中也按 75% 缓存价**,这是明确的让利。
- 根本问题没解:CLI `result` 事件只在**进程退出**时发一次,中途的
  `thinking`/`assistant`/`tool_call` 事件不带 token 字段,而一个 CLI 进程横跨调用方
  多个 HTTP 轮次(实测见过 21 次工具调用)。所以**末轮=真值(28%)、中间轮=估值(72%)**
  是结构性的,不是实现缺陷。要拿到每轮真值只能改走协议模拟(`agent.v1.AgentService/Run`)。


## [cursor-cli-cache-coherence] CLI 驱动:三个缓存计费口径 bug — 2026-08-17

同日第七批。用户原话:「只要有输入,就没有缓存命中,这是个严重的计费bug」。
查出来是**三个**独立的口径错误,外加一处对抗评审揪出的**发布顺序竞态**。

⚠️ 本批有一个**我方向搞错、被实测推翻**的修复(Bug 1),过程留在下面 —— 那类
「从聚合统计反推字段语义」的推理方式本身是错的,记下来防复发。

### 口径基础(三处判断都建立在这上面)

`ChatUsage.input_tokens` 的契约是**总输入(含缓存命中)**:

- 发客户前 `chat::delta_usage_json_impl` 做 `input_tokens = input − cache_read`
  (Anthropic 语义:input 只算未命中);
- 落库 `request_logs.input_tokens` 存 **total**;
- 计费 `reported_tokens = input + output`。

两个口径差一层 —— 搞混会得出完全相反的结论(见记忆 caio-cache-billing-and-hot-settings)。

### Bug 1:`cache_read > input` 让客户侧「输入」显示 0

生产实测(近 4 小时 grok-4.6 有缓存记录 127 条):**49 条(39%)`cache_read > input`**,
线缆侧 `input − cache_read` 的 `saturating_sub` 把 `input_tokens` 归 0 —— 客户面板
显示「输入 0 / 缓存 36 万」。真实样本 `id=1572918`:`input=72811 / cache=366720`;
最极端比值 **239 倍**(`input=54 / cache=12928`)。

**⚠️ 我第一版的诊断是错的,已被对抗评审顶回来。** 当时据这 39% 推断
「上游 `inputTokens` 是未命中新增」,于是改成 `input + cached` —— 那会给**每个正常轮次
凭空加一倍输入**。判据是实测单次调用(生产抓的原始 NDJSON,提示词「只回一个词:ok」
约 10 字符):

```json
{"inputTokens":6779,"outputTokens":35,"cacheReadTokens":6016,"cacheWriteTokens":0}
```

10 字符的提示词不可能有 6779 token 的**新增**输入 —— 6779 是总量(AGENTS.md/system 等),
其中 6016 命中。所以 `inputTokens ⊇ cacheReadTokens`,与"总输入"契约天然一致,
**原样填**。`cache > input` 不能反推字段语义。

真实成因:CLI 一次会话内部会发起多次模型调用,`cacheReadTokens` 是跨内部调用**累加**的,
`inputTokens` 不是 —— 两个字段不同口径,不可相减。

处理(**这里也走了一次弯路,两版都记下**):不去重构总量(重构不出可信值),对不可信的
cache 只能二选一 ——

- ❌ 第一版**封顶到 input**:不变式是恢复了,但 `cache == input` → 线缆侧
  `input − cache` **恒 0**,客户面板「输入」列照旧是 0。部署后实测 5 条里 **3 条(60%)**
  被封成全等,比修之前的 39% 还高 —— **用户报的现象一点没好**。而且 `cache == input`
  等于断言"本轮输入 100% 命中缓存",是假断言。
- ✅ 现版**丢弃该 cache(置 0)**:`input` 经单次调用实测证明可信,cache 与它不同口径、
  两者没有可信的相对关系,那就只留可信的那个。客户侧 `input − 0 = input`,输入列显示
  真实总量;代价是这些轮次不给缓存折扣(客户按全价付)。方向:**宁可少给折扣,
  也不谎报命中、更不把输入显示成 0**。

丢弃时 warn:既是上游口径异常的信号,也是这些轮次没拿到折扣的原因,必须可查。

顺带修:`real_cache_read_tokens` 原来漏填 → 面板「真实缓存」列恒 0(不可信的值同样
不进对账列)。

### Bug 2:自估轮 cache_read 结构性恒 0(我方贴钱)

CLI 驱动这条路**从来没接过 `cache_sim`** —— peek/commit 只写在 `chat.rs` 线协议侧。
工具回路的每一轮都是调用方独立的一次 HTTP 请求,发 `tool_use` 那一刻上游还没给
`result`,只能自估(`SsePhase::estimated_usage`),而自估路径的 `cache_read` 写死 0。

生产实测:grok-4.6 近 4 小时 674 条成功请求,**555 条(82%)缓存为 0**。
客户看到的现象就是「同一条会话里有的轮有缓存、有的轮一点都没有」。

修法:入口备好指纹材料(`SimRequest`),在 `start_conv` / `resume_conv` 里 peek 成
`SimSlot`,泵**独占**本阶段那一份、构造 `SsePhase` 时读它的 `cache_read`。命中是
**加**在自估新增量上 —— `in_tally` 记的是本轮真实喂进 CLI 的新文本,若改成从里面减,
`saturating_sub` 会把它归零,又变成「输入 0」。

### Bug 3:tools 双重计费(多收客户,自查发现)

写完 Bug 2 自查"加模拟缓存会不会双重计费"时发现真的会:指纹序列是
`[system][tools][各轮消息]`,而首轮 `in_tally` 是 `prompt + tools` —— **tools 在两边都有**。
命中覆盖 tools 时两者相加,把上万 token 的工具清单算了两次。

判据**不能**是"有没有命中":工具清单变更时前缀正断在 tools 块上(只剩 system 命中,
见 `tools_change_breaks_prefix_after_system`),那一轮 tools 确实是新发的、必须计费。
所以按**命中是否越过 tools 块**判(`SimSlot::covers_tools`,阈值 = system+tools 段
的 token 总量),为真时 `first_in_tally` 不再 push tools。

### Design Rationale

- **模拟值绝不写 `real_cache_read_tokens`**:模拟是计费策略、不是事实断言,对账列只认
  上游自报 —— 与 `chat.rs:2640` 的补偿闸同一条纪律。
- **槽的所有权跟着"哪一轮把结果喂回来"走,不放会话上**(对抗评审 high#2)。
  一个 `SimSlot` 描述的是**某一次调用方 HTTP 请求**,而 `CliConv` 是会话级、同
  `conversation_id` 的并发请求共享它 —— 会话单槽会被后来者覆盖,于是 A 的账单用到 B 的
  命中数、甚至 A 的 commit 提交了 B 的指纹(代际 CAS 只保护表的一致性,保护不了
  「谁的账单用了谁的槽」)。改成:泵持有本阶段的槽(局部变量,独占),
  下一阶段的槽经 `PendingSlot.responder` 通道随 tool_result 一起送进来。
  结构上不可能错配,也消掉了"装槽晚于唤醒"的竞态(两者现在是同一次 `send`)。
- **peek 尽量靠后**(对抗评审 high#3):代际号从读出到 commit 之间越久越易被推进。
  入口只造 `SimRequest`(未 peek 的材料),`resume_conv` 里**先校验 tool_result 匹配、
  后 peek** —— 错配的请求根本不算一次有效轮次,让它先读状态只会白白拉长竞态窗口。
- **commit 判据 = 「这个 Anthropic 响应已交付给调用方」**,不是「整条 CLI 会话成功」
  (对抗评审 high#1/medium#4)。tool_use 处交付了就提交:对调用方而言那是一次完整成功
  响应(有正文、有 tool_use、`stop_reason=tool_use`),它已据此计费,下一轮带
  tool_result 回来时那段历史确实在上游手里。之后断线/报错**不回滚** —— 那是下一轮的失败,
  且真发生时 `ConvRegistry` 分叉校验会换掉 conversation_id(= 换掉模拟键),旧条目自然
  命不中。两处收尾由此统一:提交时机不再取决于模型是否恰好走了 tool_use 分支。
- `covers_tools` 只在 `start_conv` 消费:续轮 `in_tally` 只含 tool_result,不含 tools,
  天然无重复。

### Notes & Caveats

- **这三处都改的是口径,不是定价**。Bug 2 修完客户付得更多(原来是运营方贴钱)、
  Bug 3 修完客户付得更少、Bug 1 只影响显示与缓存列上限。想借机让利应调 `floor_ratio`
  (记忆里 0.6→0.9 = 输入侧整体让利 41%),不要靠留着 bug 来实现。
- 存量记录不追溯修正(`request_logs` 硬限 10000 行,15.2h 就滚掉)。
- **过程教训**:Bug 1 第一版方向搞反了(把封顶写成了加法),靠的是**实测单次调用样本**
  才判出来 —— 聚合统计(39% 的记录 `cache > input`)证明不了字段语义,单次调用的
  原始 NDJSON 才行。对抗评审(`gpt-5.6-sol`)顶回了这一条,以及槽所有权 / peek 时机 /
  commit 判据三条。
- **仍未做**(评审提的、判为可延后):`cache_sim::peek` 目前返回 token 数,`covers_tools`
  是拿 token 阈值反推结构边界(`cache_read >= header_tokens`)。当前指纹语义下等价且有
  三态测试,但更稳的接口是让模拟器直接返回**命中的指纹条数**。若将来改成允许部分
  fingerprint 命中,这个判据会失真。
- **`cacheWriteTokens` 仍未处理**:上游 result 实测有第四个字段(见上面那条 NDJSON),
  而 `cache_creation_tokens` 至今恒 0。它对应 Anthropic 的 `cache_creation_input_tokens`,
  按量计费通常比普通输入更贵 —— 待评估是否计入。
- **`result.usage` 的累计范围已取证澄清(原为上线阻断,现已排除)**:开
  `CURSOR_CLI_DUMP_NDJSON` 抓真实会话后确认两件事 ——
  ① `cacheReadTokens` 跨 CLI **内部**多次模型调用累加而 `inputTokens` 只算末次,
  故 `cache > input` **只出现在走 MCP 桥的多轮会话**上(桥会话 `input=9844/cache=35840`;
  单轮会话 `input=50227/cache=5888` 一律正常);
  ② `result` **不跨调用方 HTTP 轮累计** —— 四条走桥的多轮会话(桥调用 6/6/4/4 次)
  其 result 都精确等于**单独一条** `request_log`,不是多轮之和。所以桥挂起期间各轮
  走自估、末轮走 result 真值,两者**不重叠、不重复计费**,评审的这条阻断级担忧不成立。
  ⚠️ 若将来改动收尾时机(让 result 补算整条会话),这条不变式即失效,须重新取证。
  取证后已撤掉转储变量并 `shred` 掉转储文件(含客户对话)。
- 新增 7 条回归测试(口径三态 + `covers_tools` 三态 + 无槽基线);全量 1,401 passed / 0 failed。

## [cursor-cli-notice-bootstrap] CLI 驱动:提示词与本轮 tools 解耦 — 2026-08-17

同日第六批。用户原话:「用户用 claude 总是说模型不会工具调用」。查出来是我方提示词
把模型劝退的 —— **87% 的会话被写进了一句否定断言**。

### 故障现场

线上 835 份 `AGENTS.md`,**729 份(87%)**内容是「你没有任何工具可用:没有网页搜索、
终端、文件读写」。只有 106 份带工具清单、103 份含 `GetMcpTools` 指引。

模型不是不会调工具,是**在服从指令**:它每轮重读这句话,然后据此拒绝调用。

### 因果链

CLI 驱动的 system 经 `AGENTS.md` 投递(CLI 原生 rules 位置),而:

- CLI **每轮重读**该文件 —— 这正是选它的理由:`--resume` 只喂对话增量、不含 system,
  所以文件通道不受"续轮输入被截断"影响(见 `clidrv.rs` 的 `--resume` 注释);
- 但 system 是按**本轮** `req.body.tools` 重算后**覆写**同一个文件的。

于是:

```
首轮  客户端带 tools  → 写入「你只能调用这些工具…」+ GetMcpTools 指引
续轮  客户端不带 tools → 覆写成「你没有任何工具可用」   ← 否定断言,污染整个会话后续
```

文件跨轮持久,一次错误覆写污染后续所有轮次。内容比对那层(`if read != system`)
防不住 —— 内容确实变了。87% 这个比例说明**续轮不带 tools 是常态**,不是偶发。

根子上的错:API 语义里「本轮缺省」与「确实无工具」**不可区分**,所以不能从空 `tools`
推出任何能力断言。

### Fixes

- **注入逻辑删掉 `tools.is_empty()` 分叉**(`lib.rs`),恒定注入同一段说明。
  这同时回答了"怎么保证只注入一次":内容不随轮变化 → 幂等 → `prepare_home` 原有的
  内容比对天然生效,不需要会话态记忆、也不需要"首轮才写"的判断。
  `tools` 仍传给工具桥(决定是否起 MCP server),只是不再影响文案。

- **文案改为稳定 bootstrap**,四段全是环境事实,**不含任何能力断言**:
  1. 工具由 gwtools(MCP)提供、**不在初始函数表里** —— 找不到是预期现象,
     先调 `GetMcpTools` 拿清单与 schema(不说这条,模型会判定网关在说谎然后放弃);
  2. **必须用清单里的完整名字**,给出可照抄的对照示例(`mcp__gwtools__read_file`
     而不是 `read_file`),并说清短名调用的后果是**静默截断**(实测最高频失败);
  3. 工具跑在**调用方机器**上,有完整读写与执行权限;
  4. ask 的只读限制**只管 CLI 自己的本地沙箱**(空临时目录,不是用户仓库)——
     别拒绝任务、别让用户切模式(他不在 Cursor 界面里)。

  顺带修掉一个长期隐患:代码默认值此前**没有** `GetMcpTools` 那段,该指引只活在
  生产 DB 热覆盖里 —— 任何让 overlay 失效的操作(如旧 admin UI 保存设置冲掉该键)
  都会让线上退回无指引版本。现在代码默认值自带。

### 设计取舍(经对抗评审修正)

原方案是「会话内单调」:把见过的工具清单记进 `CliConv`,续轮为空时沿用不降级。
评审否掉了它 —— 现状是**过度禁用**,单调合并是**过度授权**,后者风险更高(shell/
写文件/凭据类工具)。还有几条未考虑到的:同名工具 schema 变了不能当同一能力;
并发续轮无 revision/CAS 仍会乱序覆写;清单只增不减会让 prompt 越来越长。

采纳的结论:**持久 prompt 不保存瞬时能力状态,只教模型如何查事实**。真实清单由
`GetMcpTools` 返回,那是唯一权威 —— 消除了「`AGENTS.md` 声称一套能力、`req.body.tools`
声称另一套」的双重真相。评审另建议的 MCP 侧按会话过滤清单 + 网关层强制 allowlist
属权限模型改造,本次**未做**(独立加固项)。

### 注意

- **存量未清**:那 729 份被污染的 `AGENTS.md` 仍在磁盘上,进行中的老会话下一轮
  仍会读到 —— 新代码只在有请求进来时重写该文件。
- **DB 热覆盖仍生效并覆盖代码默认值**。线上那份内容接近(也讲 GetMcpTools),但
  **没有**新增的"完整名字 + 全名示例"那段。要让它生效需清空该键或同步更新。
- 新文案约 700 字且**无条件注入**,纯问答会话也会读到这段讲工具的说明。判断这个
  代价远小于 87% 污染,但它确实是笔开销。

### 测试

新增两条回归锁:必须含 `GetMcpTools` + 完整名字要求 + 全名示例 + 截断后果;
以及**禁止**出现「你没有任何工具可用」/「你只能调用这些工具」。242 项全绿。

---

## [cursor-cli-proc-leak] CLI 驱动:进程泄漏(循环等待 + 孤儿帮工)— 2026-08-17

同日第五批。用户原话:「38个进程没有回收这个问题也必须查清楚,这个问题很严重」。
查的时候已经涨到 **74 个进程 / ~13GB**(总内存 32GB,free 只剩 1.9GB)。

### Fixes

- **`CliConv::kill_procs` 是个空壳**(`fn kill_procs(&self) {}`,注释还写着"所有权在
  pump 里,kill_on_drop 兜底")。那个假设不成立,而且错法是**循环等待**:

  1. 泵因为「不是 CLI 自己退出」的原因 break —— `CLI_TIMEOUT` 240s 或桥连接中断;
  2. 调 `kill_procs()`,空壳,子进程还活着;
  3. 紧接着无限 `stderr_task.await` —— 那个 task 在读子进程 stderr **等 EOF**,
     子进程活着就永远不 EOF → **永久阻塞**;
  4. 泵任务永不返回 → `PumpArgs.cli`(`Child`)永不 drop → `kill_on_drop(true)`
     **永不触发**;
  5. 子进程只有被杀才会死,而唯一的杀手正卡在等它。

  **现场取证**:泄漏进程的 stderr 读端**全部**仍被 worker 持有(= 卡在第 3 步);
  6 小时内「会 break 但 CLI 不自退」的事件 **19 次**(12 次 `单轮超过` + 7 次 `桥连接中断`),
  与当时 >600s 的泄漏进程数 **13** 同量级。另有 26 次 90s 空闲掐流 —— 那个只掐调用方
  一侧的流、**不动泵**,泵会继续跑到 CLI_TIMEOUT,正是上面 12 次的来源。

  **修法**:`kill_procs` 真的去杀,而且杀**整个进程组**;收尾顺序改成
  「先杀组 → tokio `kill().await` 兜一手兼收尸 → 收 stderr **带 2s 超时**,超时 `abort()`」。
  即便 kill 因为任何原因没生效,泵也一定返回、`Child` 一定 drop。

- **孤儿 `worker-server`(33 个 × ~163MB ≈ 5.4GB)**。`cursor-agent` 会自己再拉起
  `node index.js worker-server` 帮工。SIGKILL **不传播**,只杀主进程会把帮工留下;
  而 worker 在容器里是 **PID 1**(`/proc/<pid>/status` 的 `NSpid: <host> 1` 实测),
  孤儿于是重挂到 worker 名下 —— 但 worker 是 tokio 进程,只 wait 自己的 `Child` 句柄,
  **不会收养/回收陌生孤儿**,于是永久驻留。
  **修法**:spawn 时 `process_group(0)` 让 CLI 自成进程组,`kill -KILL -- -<pgid>`
  一次收干净。不引 libc(与本文件 `chown` 的既有做法一致)。

### Design Rationale

- **为什么记进程组而不是 pid**:要连孙进程一起收。`CliConv.pgid` 在 spawn 后立刻填,
  会话淘汰(`CliConversations` 换新)与泵收尾两条路都靠它。
- **为什么 stderr 收集要带超时**:它是这次死锁的直接绞索。杀组之后正常会立刻 EOF,
  超时纯属兜底 —— 但**必须有**,否则任何"杀不掉"的新原因都会重现同一个死锁。
- **测试用真进程组验,不用 mock**:`sh -c 'sleep 300 & sleep 300'` 造一个"父+孙",
  断言孙进程跟着死。已验证它对旧空壳实现会**干净地 FAILED**(2s 内),不是空测试。
  测试里刻意**先判定再收尸** —— 反过来写的话 `child.wait()` 会等满 300s,
  表现为挂死而非断言失败(实测过)。

### Notes & Caveats

- ⚠️ **90s 空闲掐流不会终止会话**,只掐调用方这一侧的流。CLI 与泵继续跑到
  `CLI_TIMEOUT`(240s)才收 —— 这也是「`tool_result` 与挂起的 `tool_use` 不匹配」
  那个 400 的温床:会话还活着、挂起槽还在,而调用方那边已经重试并被**线协议驱动**
  接走了(线协议透传上游 id,形如 `call-<uuid>-0`,与 CLI 驱动的 `toolu_<32hex>`
  天然对不上)。同一个会话在两个驱动之间弹 = 我方问题,不是客户带错 id。
  **本批未修这条**:它要改工具回路的语义(掐流时是否连带终止会话),有把正常多轮
  循环搞坏的风险,单独评估。
- 存量泄漏进程需要**手动清一次**:>600s 的必然是泄漏(正常上限 = CLI_TIMEOUT 240s
  + 一次 PENDING_TTL 280s = 520s),清掉还能顺带解开卡死的泵。

## [cursor-cli-tool-round-usage] CLI 驱动:工具轮次不再记 0 用量 — 2026-08-17

同日第四批。用户报障原话:「有大量的空输出,这是为什么?但是用户说是有回复的」,
外加 new-api 面板截图 —— 一连串 `115,834 / 0`、`116,130 / 0`。

### Fixes

- **以工具调用收尾的请求,用量全部记 0**(占 cursor 全部请求的 **56%**)。
  `clidrv::SsePhase::finish_tool_use` 里硬写着 `{"input_tokens":0,"output_tokens":0}`,
  而且**全仓只有 `finish_done` 一处** push `StreamItem::Usage` —— 于是这些请求既不报给
  客户、也不落库。客户那边正文照收(所以"有回复"是真的),账面上是空的。

  生产实测(近 3 小时 1259 条,team4/5/6 + test/test1/ultra-test):

  | 收尾方式 | 请求数 | 落库用量 | 送出的正文+thinking 字符 |
  |---|---|---|---|
  | 成功 / `end_turn` | 545 | 全部 > 0 | 698,818 |
  | 成功 / `tool_use` | **632** | **全部 = 0**(仅 1 例外) | **226,736** |

  相关性 100%。yapi 侧显示「大输入 / 0 输出」是因为它的输入是自己数的,输出取我方 usage。

  **为什么结构上是 0**:抓 `cursor-agent` 原始 ndjson 确认,用量**只出现在整个会话结束时
  的那一条 `result` 事件**里(`thinking`/`assistant` 事件一个 token 字段都没有)。工具
  回路里每一轮是调用方独立的一次 HTTP 请求,发 `tool_use` 那一刻上游还没给数。

  **而且不是"延后到最后一轮结算"**:按会话深度(cache_read+input)分桶看
  `真值/估算` 比值 —— 无缓存 1.77 / 浅 2.28 / 中 3.06 / 深 2.74,**不随深度增长**。
  若是整会话累计,深桶会高出一个量级。所以那 22.6 万字符是真丢了。

  **修法**:工具轮次改自估(线协议侧早有 `estimate_usage_fallback` 兜同一个坑,CLI 驱动
  是后加的,漏了)。新增 `TokenTally` 累加 `thinking` + 正文 + **tool_use 参数 JSON**
  (纯工具轮的产出大头就是参数,正文可以是零个字),收尾时报进 `message_delta.usage`
  并 push `StreamItem::Usage`。

### Design Rationale

- **为什么攒字符再算一次,而不是每段调 `est_text_tokens` 相加**:那个函数对 ASCII 是
  `ceil(n/4)`,每段都向上取整;流式几百个 delta 累加下来光取整误差就能虚高一倍多。
  `TokenTally` 攒总数最后算一次,结果与"整段一次性估算"逐字节相同(有测试锁)。
- **校正系数 1.85 是标定出来的,不是拍的**。拿 901 条**有上游真值**的 `end_turn` 请求
  反标定:`est_text_tokens(正文+thinking+参数)` 只有真值的 **0.54 倍**(中位 0.58,
  P5=0.17 / P95=0.97),总量对齐需要 1.85。差额来源是**隐藏推理** —— 流里的 `thinking`
  是摘要,上游按完整 CoT 计费,与「加密 CoT ≈ 摘要 2.3 倍」那次测量同向。
- **系数只乘 output**。input 侧没有对应的隐藏量,乘了等于凭空多收。
- **input 只算"本轮新增"**:首轮 = prompt + 工具定义,续轮 = 喂回去的 tool_result。
  不含 system(走 AGENTS.md,CLI 每轮重读,在上游基本全是缓存命中)、不含会话历史
  (`--resume` 的历史在 CLI 自己的会话文件里,我方这轮并没送上去)。算进来会重复计费,
  还会搅乱 cache_read 口径。
- **cache_read 恒 0**,统一由 `cache_sim` 在收尾处给 —— 与线协议同一条纪律:同一个量
  不在两处估。

### Notes & Caveats

- ⚠️ **这是估算**,只保证总量口径对得上。单条请求可能偏 2 倍以上(见 P5/P95)。取值
  偏保守一侧(1.85 是总量对齐值而非 P75),宁可少收也不多收。重新标定的口径:
  `stop_reason='end_turn'` 且自身正文 ≥50 字符的样本算总量比。
- 线协议侧的 `estimate_usage_fallback` **没有**这个系数,大概率同样低估。没跟着改:
  那条路是生产主力且它的数字是历史计费值,动它要单独标定 + 单独评审。
- 顺手补了一处**可观测性缺口**:`take_pending_matching` 的 tool_use 错配以前只回给
  客户、不落日志(生产 grep 6 小时日志 0 命中,只能从 `request_logs` 的 400 反查)。
  现在打 warn 并带上双方 id —— 实测客户会带回自己框架生成的 `call-<uuid>-0`,与我方
  `toolu_<32hex>` 天然对不上,不打出来根本判不出是谁的问题。

## [cursor-tier-and-cli-default] 账号档位可见 + CLI 驱动转默认 + CLI 出口代理透传 — 2026-08-17

同日第三批。前两批见下面两条 —— 这批全部来自那之后 40 分钟的线上排查。

### Fixes

- **档位在后台完全看不出来**(用户报障原话:「为什么 team 号显示不出额度」)。三个当天
  新买的号在 20 分钟内被上游降级成 FREE,而面板上只有一片空白。三个原因叠在一起:
  1. FREE 档的 `GetCurrentPeriodUsage` **不给** `includedSpend`/`limit`(免费号没有
     套餐内额度),而 `parse_period_usage` 对此一律报错 → **整个配额查询失败** →
     额度栏空白,与"查询失败"长得一模一样;
  2. 档位回填拿的是 `q.currency`。那对 kiro 成立(currency 装的就是 `KIRO PRO` 这类
     档位名),对 cursor **不成立**:gw-cursor 写死 `currency = "USD"`,于是每个 cursor
     号的 `subscription_title` 都被填成字符串 `"USD"`;
  3. `warm_subscription_titles` 只补「缺这个字段」的号 → **填错一次永不复查**;而判付费
     用的是「`subscription_title` 存在且不含 FREE」——`"USD"` 不含 FREE,于是**降级号
     一直被当付费号**,继续被派发它已经没权限的模型。第 3 条正是 grok / composer 反复
     `ModelNotAvailable` 的下游成因。
  **修法**:`AccountQuota` 新增 `plan_tier`;cursor 按「有没有套餐内额度」判 `FREE`/`PAID`;
  FREE 不再报错(`used`/`limit` = 0 是事实而非未知);worker 回填**优先 `plan_tier`、
  `currency` 兜底**(kiro 走兜底分支,行为逐字节不变);`/health` 与账号表暴露该字段,
  FREE 直接打红标 + 悬浮提示「额度栏空白 ≠ 查询失败」。
- **CLI 驱动静默直连**(默认化路上踩出来的,比原问题严重)。`start_conv` 里 `env_clear()`
  之后只塞了 `HOME`/`PATH` —— 什么都不继承,所以**账号配的代理压根没进 CLI 子进程**。
  实测:三个代理号切到 CLI 驱动后全部失败(表象是 `cursor-cli 桥连接中断`),而记忆里
  直连出口的封号率是 **59.5%**(代理 0%)。也就是说漏这段不只是不通,是在烧号。
  **修法**:从 `extra.proxy` 派生 `HTTPS_PROXY`/`HTTP_PROXY`/`ALL_PROXY`(大小写两套)
  塞进子进程,回环走 `NO_PROXY` 免得把 MCP 桥绕进代理。

### Features

- **CLI 驱动成为默认**,线协议改成显式退出(`extra.driver="wire"` 单号退出、
  `CURSOR_DRIVER=wire` 整个 worker 退出;历史值 `"cli"` 仍合法 = 默认)。后台开关与
  表格徽章同步翻转:默认档不再打标,**只对退出线协议的号打标**(异常才该显眼)。

### Design Rationale

- **为什么把默认翻过来**。线协议给上游发的是**裸模型名**(`grok-4.6`),CLI 驱动发的是
  CLI 那套名字(`cursor-grok-4.6-high`)。当天 06:50 前后上游停止接受裸 `grok-4.6`、
  随后 `grok-4.5` 也一样,线协议上的三个号在几分钟内全部 `ModelNotAvailable`,而同一批号
  切到 CLI 驱动**立刻恢复** —— `cursor-agent --list-models` 实证上游只有
  `cursor-grok-4.x-*` 那一族。裸名那套是我方逆出来的、会被上游单方面收走;CLI 用的是
  **官方客户端自己在用的名字**,后者才是长期站得住的一侧。加上 CLI 驱动本来就带真实
  usage(含 `cacheReadTokens`)与 MCP 工具桥,没有理由再让它当可选项。
- **代理那段是实测过才写的,不是照惯例猜的**。cursor-agent 是 Node 程序,而 Node 默认
  并不认 `HTTPS_PROXY`,所以必须先证伪:把 `HTTPS_PROXY` 指向 `127.0.0.1:1`,
  `--list-models` 当场 `ECONNREFUSED 127.0.0.1:1`;不设则正常 →它走的是认环境变量的
  HTTP 客户端,`NODE_USE_ENV_PROXY` 不需要(加了也不变)。
- **档位只报能证实的二分,不猜档位名**。`GetMe` 实测只有 `authId`/`email`/`country`/
  `isEnterpriseUser` 这些,**没有会员字段**;`GetTeams` 对这些号返回 `{}`(所以它们
  根本不在任何 team 里,名字只是我方台账);其余 7 个猜测的方法名全 404。唯一稳的判据
  就是「有没有套餐内额度」,那就只报 `FREE`/`PAID`。
- **只缺一个金额字段仍然报错**。原注释的顾虑是对的(`unwrap_or(0)` 会把故障显示成
  零额度),只是漏了 FREE 这个第三种形态。所以判据收窄成「两个都缺 = FREE,只缺一个 =
  上游改了字段」,两个方向都有单测钉着。

### Notes & Caveats

- ⚠️ **部署必须连带清库**:已有 cursor 号的 `extra.subscription_title` 是被污染的 `"USD"`,
  不清掉新逻辑也不会复查(只补缺字段的号)。清理语句见部署记录(`json_remove` 该键,
  条件 `= 'USD'`,不动别的值)。
- ⚠️ **这批把每个 cursor 号都推上了 CLI 驱动**(除显式 `wire`)。每请求 spawn 一个
  `cursor-agent` 子进程,并发上限按号累加 —— 上线后要看一眼宿主内存。
- team1/2/3 当天的临时措施(`driver=wire` + 排 grok 的白名单)在部署后一并撤掉,统一回
  默认;它们仍是 FREE + disabled,不会因此吃流量。

## [cursor-cli-ws-isolation] CLI 工作区改每会话一份(修跨会话提示串味)+ 调用方侧空闲闸 — 2026-08-17

上一条 `[cursor-inject-routing]` 部署后盯了 30 分钟,盯出两件新事。

### Fixes

- **跨会话提示串味 + 工具闭集错乱**(真事故,线上取证)。`prepare_home` 把调用方**本次请求的
  system 提示**写进 `AGENTS.md`,而工作区是**每号一份**(`<home>/ws`)—— 同号并发的两个
  会话互相覆盖这个文件。`ultra-test` 同时在跑两种流量:一个无工具的角色扮演会话
  和多个带 24~98 个工具的 Claude Code 会话。取证时刻的 `AGENTS.md` 里装的是
  **「你没有任何工具可用」+ 角色人设**,而同期带工具的请求的 CLI 读的就是那份。
  后果在模型自己的 thinking 里:
  - 「已确认可用工具为 Shell、Read、Write 等 Cursor 本地工具,**gwtools 未直接列入 MCP 列表**」
  - 「注意到 **Ask 模式限制与 gwtools MCP 工具使用之间的冲突**,需按工作区规则处理」

  除功能错乱,这还是**跨会话的提示泄漏**:甲客户的 system 提示躺在乙客户的 CLI 工作区里。
  HOME 按号隔离(`account_uid` + 700)挡的是**号与号**,同号内的会话之间此前没有边界。
  **修法**:工作区改 `<home>/ws/<conversation_id>/`,`AGENTS.md` 与 `assets/` 随之隔离;
  `auth.json` / `mcp.json` / `permissions.json` 仍每号一份(它们本来就是号级的)。
  另加**迁移清理**:旧版那个 `ws/AGENTS.md` 恰好落在新工作区的**父目录**,而 Cursor 会
  往上层找 rules —— 不删等于把串味原地保留,所以见到就删。
- **调用方侧 300s 空等**。`resume_conv` 把 tool_result 喂进挂起槽后直接返回 `drain_stream`,
  而那个流**没有任何超时**:CLI 之后若一声不出,调用方一路等到 gw-app 的 300s
  `STREAM_IDLE_ABORT` 才收到一个空响应。实测占 tool_result 接续请求的 1.0%(切换前)~
  7.0%(切换后 30 分钟,n=43)。**修法**:`drain_stream` 加 90s 空闲闸
  (`DRAIN_IDLE_TIMEOUT`,与线协议 `chat::STALL_TIMEOUT` 同值),超时发一条带原因的
  错误并终止本次响应,**不动 CLI 进程**(调用方重试走 `cli_lookup`,该重铺就重铺)。

### Design Rationale

- **为什么不是去修 `CLI_TIMEOUT`**。它的注释写着「单轮 CLI 调用的硬上限……保证先于我方
  上层超时干净地杀掉进程」,读起来正是该管这件事的那个闸。但它量的**不是墙上时钟**:
  检查点在泵 `select!` 的 500ms tick 分支,而桥挂起是在 `call` 分支内部 await 一个 280s
  的 timeout,那段时间 select 停转、tick 不响 —— 挂起等待**不计入**预算。
  而这恰恰是**必要的**:一个 CLI 进程横跨调用方的多个 HTTP 回合,10 轮工具往返的墙上
  时钟轻易超过 240s;把挂起算进去会误杀正常的长工具回路。所以该加闸的地方是**调用方
  那一侧**,不是进程侧。注释已按实际语义改写(原注释是错的,会把下一个人引到误修上)。
- **GC 判据用显式写下的 `.last`,不用目录 mtime**。目录 mtime 只在增删目录项时变,一个
  跑了半小时、期间只读文件的活会话其目录 mtime 可能很旧 —— 拿它当判据会删掉**正在用的
  工作区**,而"CLI 的 cwd 被删掉之后会怎样"是另一场排查。没有 `.last` 的目录一律当过期
  (只可能是旧版残骸或写标记失败的残骸),当前会话用**路径**排除而不是名字比较。
- **会话 id 为空时落 `_noconv` 而不是退回父目录**。退回父目录会静默恢复串味;给一个确定
  的兜底目录,最坏情况是所有无 id 请求共用一个,但不会污染有 id 的会话。

### Notes & Caveats

- ⚠️ **磁盘**:每会话一个目录,靠 `WS_TTL`(2h,与 `SESSION_TTL` 同值)回收,回收在
  `prepare_home` 里顺手做。`chown -R` 仍作用于整个 HOME,目录多了会略慢 —— GC 保证有界。
- ⚠️ 上线后活跃会话的工作区会**换路径**(从 `ws/` 到 `ws/<conv>/`),CLI 的 `--resume`
  靠 HOME 下的会话存储而不是 cwd,所以续写不受影响;但**图片附件的绝对路径变了**,
  上一轮提示里引用的旧路径在新目录下不存在。影响面限于"刚发过图、下一轮立刻续问"。
- 本条与 `[cursor-inject-routing]` 的两条修复同属 CLI 驱动,`caio-worker-cursor` 独立容器,
  仍**不动 kiro 数据面**。

## [cursor-inject-routing] 分流中转注入的 `role:"system"` 消息 + CLI 驱动 ask 模式说明 — 2026-08-17

### Fixes

- **「grok 收到空消息」**(用户报障;线上从 02:11 `ultra-test` 切 `driver=cli` 起持续
  ~3 小时)。真实流量里客户端经中转会在**每条用户消息之后**再追一条
  `{"role":"system","content":"<total_tokens>15000000 tokens left</total_tokens>"}`,
  而 `chat::to_turns` 判 `is_user` 用的是 `role != "assistant"` —— 于是这条**每轮都变的
  预算计数器成了最后一轮**。两条路径同时被带偏:
  1. CLI 驱动的 prompt 取 `raw_turns.last()`,发给上游的整条 prompt 就是那行计数器。
     取证来自 grok 自己的 thinking:*"The user's message contains only a token count
     indicator."* —— 用户明明打了一大段,看到的回复是「你这条消息是空的」。
  2. `chat::last_tool_results` 要求末条是 `role=="user"`,尾巴是 system 就返回 `None`
     → 挂起的 MCP 桥调用**永远接不上** → 模型反复说「MCP 读取被中断了」,
     伴随 90s stall 与 `incomplete_stream`。
  **线协议那条路侥幸没露**:`fold_history` 无条件全量重铺,尾巴上多一行计数器无伤。
  所以症状表现为「切了 CLI 驱动才开始空」,极易误判成 CLI 驱动本身坏了。
- **修法(三处,均在 gw-cursor)**:
  - 新增 `chat::route_system_role_messages`,在 `Provider::chat` 的**第一行**原地改写
    `body`,四级分流:稳定前缀(SessionStart hook / CC 身份行)提升进顶层 `system`;
    动态噪声(预算计数器、task-tools 催促)丢弃;interrupted-user 取正文转 `user`;
    其余未知裹 `<system_context>` 转 `user` **原位**保留。无 `role:"system"` 消息时
    **一个字节都不改**(快路径)。
  - 新增 `chat::latest_user_input`:CLI 驱动的 prompt 取「最后一条 assistant 之后的
    整段」,不再取末条。正常形态下与旧行为逐字节相同。
  - `chat::affinity_key_from_body` 的锚点跳过 `role=="system"`:该函数由 worker 在
    `chat()` **之前**用原始 body 调用,那时分流还没跑;锚点取到每轮都变的计数器
    等于亲和归零。

- **模型自我审查成「只读」并要求用户切 Agent 模式**(同日同一用户报障的第二个问题)。
  CLI 驱动把 `cursor-agent` 钉在 `--mode ask`,模型据此拒活:生产 thinking 原文
  「当前处于 Ask 模式,仅可读取与分析,无法修改代码」,正文则要求用户
  「在 Cursor 把模式切到 Agent,再发一句 execute now」—— **用户根本不在 Cursor 里**,
  这条建议无从执行,任务就此卡死在"请你切模式 / 我不是 ask 模式"的死循环上。
  **`--mode` 无解**:`cursor-agent --help` 实证只接受 `plan` / `ask` 两个值、都是只读;
  写权限只能整个不传 `--mode`。
  **修法**:新增热配置 `cursor_cli_notice`,在**有工具声明**的 CLI 驱动请求上追加一段
  说明,把真相讲清楚 —— ask 的只读限制只作用于 CLI 自己的本地沙箱(那是个空的临时
  目录,不是用户的仓库),真正的执行通道是 gwtools 工具、跑在**用户自己的机器**上、
  有完整读写权限;因此不要拒活,也不要让用户去切模式。

### Design Rationale

- **为什么在入口一次性改写 body,而不是逐个函数打补丁**。被这条尾巴带偏的不止两处:
  `to_turns` / `history_fps` / `cli_eligible` / `fold_history` / `cache_sim` 全都假定
  `messages` 里只有 user/assistant。逐个加「跳过 system」是五处各自记得,漏一处就是
  下一次同类事故;在入口把不变量建立起来,下游一处都不用改。
- **两道修复刻意冗余**。`route_system_role_messages` 治根(已知注入形态),
  `latest_user_input` 是兜底:分流器的最后一档是「不认识就裹起来转 user」,那种未知
  注入照样会落在尾巴上。取整段而不是取末条,这一类只让 prompt 多一段说明,
  而不是把用户的话整条替换掉。回归测试里两条各自独立断言,证明**任一条单独**
  都能让用户的话回到 prompt 里。
- **动态噪声的判据收窄到「整条消息就是这个东西」**。放宽成子串匹配的话,用户正文里
  提到 `<total_tokens>` 就会让半条消息凭空消失 —— 静默丢用户内容比多带一行噪声坏得多。
  单测正反两向都钉住了。
- **没有把实现上收到 gw-core 与 kiro 共用**(`strip_rolling_fingerprints` 当初是那么做的)。
  kiro 那份 `converter::normalize::route_system_role_messages` 跑在 `Vec<Message>` 强类型上,
  共用要动 kiro 的转换管线,而 kiro 是生产主力面;这份跑在裸 `Value` 上,且多一条 kiro
  没有的规则(预算计数器)。两份口径若要合并,应作为一次独立改动、单独回归 kiro。
- **顺带把 `system` 拍平成字符串是安全的**:全仓 `get("system")` 只有 `extract_system`
  一处读它,而那本来就是拍平取文本。
- **为什么留着 ask 模式而不是放开写权限**。不传 `--mode` 会一并放开 CLI **自己**的
  写文件/终端工具,而它们作用在容器里那个空工作区 —— 模型会报告「已改好文件」,
  调用方机器上什么都没变。那是**幽灵编辑**:比"拒活"坏得多,因为拒活看得见。
  ask 模式是刻意保留的安全闸,代价(自我审查)用提示词补。
- **文案走热配置而不是硬编码**。`cursor_tool_guard` 的历史已经证明这类文案必然要
  反复调(第二版因为"点名禁掉的能力与调用方声明的工具逐字重合"把合法工具一起吓退,
  第三版才换成正向写法)。这段同理:上限 3000 字符、写侧就拦长度、空串=回内置默认、
  **校验失败保留上一份有效值绝不静默回默认**(否则一次误配置会表现成"文案悄悄变了一版",
  而效果正在被按小时对比)。
- **只在有工具声明时追加**。一个工具都没声明时模型确实什么也改不了,那时告诉它
  「你能改文件」才是骗它 —— 会换来一轮伸手抓不存在工具的空转。

### Notes & Caveats

- ⚠️ **kiro 也在吃这条噪声,本次没治**。kiro 的三级分流会把预算计数器归到「未知」
  → 裹 `<system_context>` 转 user 保留,于是**每轮多一条内容都在变的 user 消息**,
  前缀缓存在那个位置断掉。kiro 靠全量重铺不会像 cursor 那样功能性失效,所以本次
  按「别碰 kiro 数据面」的约束留着。要治就是给 kiro 的 `is_dynamic_system_noise`
  加同一条规则 —— 独立改动 + 独立回归。
- ⚠️ **上线后活跃会话各会重铺一次**:`history_fps` 的取材变了(注入消息不再计入),
  CLI 驱动的 `cli_lookup` 前缀比对首轮必然不匹配 → 走 Fresh 折叠重铺。一次性代价。
- ✅ **已部署**:镜像 `claude-all-in-one:inject-routing-20260817`(05:51 UTC 构建),
  `caio-worker-cursor` 05:52:49 UTC 起跑它。`caio-router` / `caio-worker0` /
  dario 全部未动(仍 `overflow-20260814-061145` / `poison-fix`),**kiro 数据面零变更**。
  线上验证:切换后带注入 `role:"system"` 的真请求成功返回(105k input / 7.3k output /
  6k cache_read),响应里既没有「消息是空的」也没有「Ask 模式 / 切到 Agent」;
  `ultra-test` 工作区的 `AGENTS.md` 已含新 CLI 说明与工具闭集。
- ⚠️ **本仓库的 `docker-compose.yml` 里没有 `worker-cursor` 这个服务** —— 它只存在于
  服务器侧那份(rsync 一直排除 compose,两份早已分叉)。别照着仓库这份得出
  「cursor worker 不受 compose 管理」的结论。
- 本次顺手消掉两处 compose 漂移(服务器侧):镜像标签从 `cli-driver-20260817-2`
  改成实际在跑的版本,并补上容器一直手工带着、compose 却漏了的 `CURSOR_DELTA_HISTORY=0`
  —— 漏这一条时任何人跑一次 `docker compose up -d` 都会**静默改掉行为**。
  容器此前是 `docker run` 起的(compose 不认它,`up -d` 会报名字冲突),
  本次 `docker rm -f` + `compose up -d --no-deps worker-cursor` 之后由 compose 接管。
- ⚠️ **重启把 `pro3` 从内存禁用里放了出来**:它的 `TooManyFailures` 是进程内状态,
  重启即清,于是欠费号重新进轮转、3 发 0 成功。已 `PATCH disabled=true` 落库止血
  (这次是持久状态,重启不会再复活)。**下次重启 cursor worker 前先看一眼禁用池里
  有没有靠内存态压着的死号。**

## [cursor-driver-switch] 驱动形态成为后台可切项 + `data/` 穿越权限修复 — 2026-08-17

### Fixes

- **CLI 驱动上线即 EACCES 的根因**:`clidrv::start_conv` 用 `cmd.uid()` 降权 +
  `cmd.current_dir(ws)`,而 Rust `std` 的 `do_exec` **先 setuid 再 chdir**;CLI 的 HOME
  在 `/app/data/cursor-cli/<acc>/ws`,而上一条 CHANGELOG 要求的 `data/` **700**
  让降权 uid 连 `/app/data` 都穿不过去 → `chdir` EACCES,被报成「启动 cursor-agent 失败」。
  **不是二进制权限**(它是 755;`docker exec -u <uid> -w <ws>` 能跑通,因为 docker 的
  `-w` 是 root 身份先 chdir 再降权 —— 正好反证顺序)。共享 nobody 时同样挡,与每账号
  uid 改造无关。
  **修法(运维侧,已在 139 生效)**:`chmod 711 data/`(o+x 可穿越、不可列目录)+
  `chmod 600 data/gw.db data/state.db`。安全性质不变,已逐条实测:自己 HOME 能 chdir、
  `control.db`/`gw.db`/`state.db` 全 denied、`ls /app/data` denied、换个 uid 进不了别号 HOME。
- `models.rs::list_reports_real_context_windows` 漏取 `CATALOG_TEST_LOCK`,与热追加
  测试撞车偶发 `34 vs 35`(实测三跑必中一次)。补锁后连跑三轮 221/221。

### Features

- `PATCH /admin/api/accounts/{id}` 新增定点字段 **`driver`**:`"cli"` = 子进程驱动
  官方 cursor-agent,`""` = 清除回线协议,缺省不动。走 `merge_account_extra`,绝不碰
  凭据;认不出的值 **400**(fail-closed —— 静默收下会让 UI 显示与实际跑的驱动不符)。
  `GET` 侧顶层回显 `driver`。
- 后台账号编辑弹窗加「上游驱动」两档切换(**仅 cursor 家族**显示:别的 provider
  读不到 `extra.driver`,露出来只会误导);账号表 provider 列给 CLI 驱动的号挂徽章 ——
  同一 provider 下两种上游形态,出问题时第一件要分辨的就是「这号走哪条路」。
  切换约 30s 内经 worker sync 生效,不用重启。

### Design Rationale

- **为什么用一个权限位修,而不是改代码。** 三条候选:①`chmod 711 data/`;②把 CLI HOME
  挪出 `data/` 挂成独立卷;③自己接管 fork/exec 让 chdir 排在降权之前。选 ① 的理由是
  它零代码改动、**立即生效不用重启**(正扛生产流量的容器不能动),而且安全性质可以
  **逐条实测**给出结论:`o+x` 只给穿越、不给列目录,`control.db` 仍是 600 root。
  ② 要改 compose + 迁移已有 HOME,③ 等于自己重写 `Command` 的 spawn 序列 —— 两者
  的风险都远大于一个权限位。代价见 Caveats:①是**运维侧状态,不在代码里**。
- **`driver` 走定点 merge 而不是整块 `extra` 替换**,与 `proxy_url` / `priority` /
  `queue_enabled` 同一条纪律:整块替换会把「打开弹窗那一刻的凭据快照」写回去,
  期间若发生过 OAuth 轮换就把新 token 冲掉。定点合并绝不碰凭据字段。
- **认不出的驱动名为什么 400 而不是静默回落线协议。** 读侧语义是
  `opt_str("driver") == Some("cli")`,即**除 `"cli"` 以外的一切值都会跑线协议** ——
  静默回落不是可能,是必然。那样后台写着 CLI、实际跑线协议,排障的人会先去怀疑
  上游而不是怀疑自己填错了字。fail-closed 把错误钉死在写入侧。
- **UI 只对 cursor 家族显示这个开关**:`extra.driver` 只有 `gw-cursor` 读。给 kiro 的号
  露出一个写了也没用的旋钮,比不给更糟 —— 它会让人以为 kiro 也能这么切。
- **表里挂徽章而不是新开一列**:这个信息只有一个 provider 有,单开一列要动表头和
  所有行的排布,而「哪条上游路径」本就是 provider 的属性,挂在 provider 列旁边语义更贴。

### 2026-08-17 生产实测

`chmod` 后先用 pro3 的 uid 手工直跑 CLI(不经调度器):进程起来、`apiKeySource=login`、
模型解析成 `Cursor Grok 4.6 High`,倒在 `ActionRequiredError: unpaid invoice` —— **号欠费**,
不是代码。换有额度的 `ultra-test` 同法直跑:思考帧 + 正文 + `result.usage`
`{input 3068, output 41, cacheRead 9728}` 全回来,3.75s 收尾。

随后把 `ultra-test` 切到 `driver=cli` 接生产流量,15 分钟:**grok-4.6 55 成 / composer-2.5
37 成 / grok-4.5 8 成 1 败(99%)**,`cursor-cli:模型调用桥接工具` **27 次** —— MCP 桥回路
在容器里也成立。`选号失败:并发已满` 从约 12 条/分降到约 7 条/分。

### Notes & Caveats

- ⚠️ **本地 spawn 失败被记进账号健康**:`scheduler.rs` 把 `UpstreamErrorKind::Other`
  计入 `failure_count`,3 次即 `TooManyFailures` 停用且**不自动恢复**。于是「我方文件
  权限配错」把 pro3 判成了坏号(02:11:36 三连 EACCES → 自动禁用,至今未回轮转)。
  同段注释里 `Overloaded`/`ModelNotAvailable` 都刻意不惩罚账号,spawn 失败该进那一档。**未修。**
- `pro3` / `test1` 上游均报 unpaid invoice;cursor 池实际可用并发只剩 `test`(2)+
  `ultra-test`(4),这是 503 的真正来源,属付款/补号范畴。
- ⚠️ **`data/` 的 711 是运维侧状态,不在代码里。** 换机、重建卷、或照着上一条
  CHANGELOG 的「部署要求 `data/` 700」重做一遍,CLI 驱动就会**再次** EACCES,
  而报错文案指向 cursor-agent、不指向权限位。要根治只有两条路:把 711 写进部署
  脚本/文档,或把 CLI HOME 移出 `data/`(那时 `data/` 可以回到 700)。
- ⚠️ **本条的 API/UI 改动尚未部署。** UI 内嵌进二进制、admin 平面跑在 `caio-router`
  上,所以后台要看到这个开关必须**重建镜像并重启 router** —— 而 router 是 kiro 的入口。
  重启时必须照抄 worker0 的 env(`KIRO_LEGACY_WIRE=1` + `KIRO_THINKING_IN_HISTORY0=1`),
  并且**不能用 `docker compose up -d`**:compose 里 worker-cursor 仍钉
  `cli-driver-20260817-2` 且缺 `CURSOR_DELTA_HISTORY=0`,一跑就把已上线的 cursor worker
  退回旧镜像。线上 `ultra-test` 的 `driver=cli` 目前是直接 SQL 写进 `extra` 的,
  与本次 API 写入的形态**逐字节等价**(`json_set(extra,'$.driver','cli')`),
  所以先不部署也不影响它继续跑。
- 重建 router 会一并把 `b1ed1ab` 之后的累积改动带上 kiro 数据平面。已核过影响面:
  **无 DB 迁移**(共享 `control.db` 不会被写进旧二进制不认识的 schema)、
  `Provider::poll_token_updates` 是**默认空实现且 gw-kiro 没覆盖**、kiro 编译单元里
  只有 `converter/normalize.rs` 改成委托 `gw-core/normalize.rs`(501 条测试锁死逐字节等价)。

## [cursor-cli-hardening] CLI 驱动安全/完整性加固(对抗审查共识落地) — 2026-08-17

### Security

- **每账号独立 uid**(替代所有 CLI 共用 nobody):`clidrv::account_uid` 按
  account_id 派生稳定 uid(100_000..500_000),HOME chown 给该 uid 且 700。
  共用 nobody 时同 uid 下 700/600 不构成边界 —— 被 prompt 注入的 A 号 CLI 能
  `cat` 走 B 号的 auth.json(评审共识 S0-1)。
- **桥 socket 收口**:0666 → 0600 且属主=本账号 uid。共享 nobody 时任何被注入
  的 CLI 都能连上别人的桥 socket 注入工具结果(S0-2);bridge/ 目录本就藏在
  700 的 HOME 里(路径不可达),socket 属主是第二道。

### Fixes

- **token 双写者捕获**(评审共识:唯一不靠流量/攻击者、定时自炸的一条):
  CLI 自刷新会回写 auth.json,号库里的旧 refresh_token 随之作废 —— 不捕获的话
  gw-app 下次 OAuth 刷新 invalid_grant,号被永久误判死。现两道捕获:
  `prepare_home` 按 JWT exp 对账(文件新→上报;号库新→覆写文件)+ 泵任务每 5s
  观测 auth.json 变更;gw-app sync 循环经新增的 `Provider::poll_token_updates`
  取走,CAS(同 token 跳过 / expires_at 不更新跳过)后增量落库。
- **桥挂起槽按 tool_use_id 键控**(S1-7):`resume_conv` 只消费 id 匹配的
  tool_result,错配显式报错且保留槽位 —— 静默喂错结果是语义损坏,比报错严重。
  `chat::last_tool_results` 替代旧拼接版,严格形态必须带 id。
  (跨客户串话此前已由 cursor 专属的 client-key 亲和命名空间挡住,见
  `affinity_scoped_by_client`,本次未改。)
- 思考帧一律透传(不再看客户端开没开 thinking):丢思考 = 丢进展信号,还会让
  stall 看门狗误判掐流。
- CLI 模型映射一律非 fast 档(fast 变体加急计费,成本高):grok-4.6/4.5 →
  `cursor-grok-4.x-high`,opus-5 → `claude-opus-5-thinking-high`,
  composer-2.5 去 -fast。

### Test

- 新增 6 条:uid 派生稳定性/区间、prepare_home 双向对账(文件新上报/号库新
  覆写/同 token 不重写)、pending 键控消费(错配保留槽位)、last_tool_results
  严格形态、adopt_provider_token_updates 落库 + 拒旧回声。

## [cursor-cli-driver] cursor 通道子进程驱动(包裹官方 cursor-agent CLI) — 2026-08-17

### Features

- 新增 `gw-cursor/src/clidrv.rs`:以子进程驱动官方 `cursor-agent` CLI 作为上游。
  每号独立 HOME(token 写 `auth.json` 即登录,CLI 自刷新),`-p stream-json`
  逐事件转 Anthropic SSE,`--resume` 跨进程续会话(服务端持史实测成立),
  usage 为上游**真实值**(含 cacheReadTokens)。
- 新增 `--mode cursor-mcp-bridge`(`gw-cursor/src/mcpbridge.rs`):MCP stdio 桥,
  把调用方声明的 tools 经 `gwtools` MCP server 暴露给 CLI;调用挂起→网关发
  tool_use→调用方带回 tool_result→CLI 续跑,输出桥接到新响应(live bridge)。
- 图片:附件落每号工作区 + 提示词带路径(ask 模式只读工具读图,实测识别正确);
  PDF 沿用文本层抽取内联。
- 会话分叉防线:逐轮指纹前缀校验,分叉换新会话重铺(见 PROTOCOL §20.2)。
- 线协议侧新增 CLI 形态(`cli.rs`,`CURSOR_PROFILE=cli`,默认关):单轮已实测
  出字+正常收尾;续轮持史缺「哈希回显」握手,留作后续攻坚(§20.2/§20.5)。
- 开关:账号 `extra.driver="cli"` 或 `CURSOR_DRIVER=cli`(灰度用),默认关,
  线协议 IDE 形态不变。

### Security

- CLI 固定 `--mode ask`(read-only),绝不加 `--force`;MCP 白名单
  `{"mcpAllowlist":["gwtools:*"]}` 只放行桥工具。
- ask 模式仍允许只读 shell:CLI/桥子进程降权 **nobody(65534)** 运行;
  **部署要求 `data/` 700、`control.db` 600**,否则模型可 `cat` 走号库。
- 镜像烘入 cursor-agent 2026.08.11-e8db854(钉版本,官方 tarball,约 +200MB)。

### Test

- 单测:全 workspace 1487 条全绿(新增 NDJSON 事件分类/去重、CLI 帧结构、
  会话通知识别、提交回显收尾等 9 条)。
- e2e 真号(宿主机直跑):纯文本三轮记忆(8866→9000 推理正确)、MCP 桥工具
  回路(桥挂起→tool_result→续答,哨兵串往返)、多轮+工具混合、图片理解,全过。

## [cursor-cch] cursor 会话键被滚动指纹污染:缓存命中恒 0 的根因 — 2026-08-15

### Features

- 新增 `gw-core/src/normalize.rs`:`strip_rolling_fingerprints` 的**唯一实现**。
  gw-kiro 的同名函数改为薄委托(501 条测试原样全绿 = 逐字节等价),死代码删除。
- `gw-cursor::chat::extract_system` 现在剥掉 Claude Code 的滚动 billing 指纹行。
- 新增 `delta_history()`:`Continuation` 相位下**只发本轮**,历史交服务端。
  开关 `CURSOR_DELTA_HISTORY`,**默认关 —— 且已被真号实测否决,不要开**(见下)。

### Design Rationale

- **根因**:Claude Code 在 system 顶部拼一行
  `x-anthropic-billing-header: …; cch=<5位16进制>;`,`cch` 是**每请求都变**的 body 哈希。
  `gw-cursor` 的 `extract_system` 是裸透传,而它的产物同时喂给三处:会话亲和键
  (→ 上游 `conversation_id`)、发给上游的系统提示、`cache_sim` 的指纹。于是:
  会话键每请求都变 → 账号钉扎失效 + `conversation_id` 每轮都变 →
  `ConvRegistry::phase_for` **一次都没返回过 `Continuation`** → 缓存指纹每轮都变、
  命中率恒 0。kiro 早有这道处理,cursor 一直没有。
- 代码里记着 Continuation 实测把命中率从 **32.6% 拉到 49.8%**(单轮最高 98.7%)——
  那份收益 ccmax 流量从来没吃到过。这次修完才第一次可达。
- **`delta_history` 为什么默认关**:开了之后我方不再重传历史,正确性完全押在
  「服务端真按 `conversation_id` 存住了前几轮」这个**我方无法直接验证**的假设上
  (只能靠模型答得对不对间接判断)。押错 = 模型静默丢上下文,比"贵但对"糟得多。
  代码默认因此保持关闭。

### 2026-08-15 上线实测:cch 修复✅ / delta 模式❌(已关)

镜像 `cch-20260815-030821`,只换 `caio-worker-cursor`(router / worker0 / 其余 22 个容器未动)。

**探针 A —— cch 修复,通过。** 不带 `metadata.user_id`、每轮 `cch` 都不同的 Claude Code
形状请求发两轮:两轮落在**同一个账号**(`ultra-test`),`cache_read` 从 2176 涨到 6656。
会话键不再被滚动指纹搅动,前缀缓存第一次真的开始命中。

**探针 B —— delta 模式,否决。** 稳定 `metadata.user_id` 连发三轮(①记住 4712
②聊件无关的事 ③问那个数字),同一构建只翻开关:

| `CURSOR_DELTA_HISTORY` | 第三轮回答 |
|---|---|
| `1`(只发本轮) | 「这次对话里我没收到过你要我记住的数字」❌ |
| `0`(内联全量) | 「4712」✅ |

**服务端在我方这种请求形态下并不替我们持有历史。** 已把容器改回 `-e CURSOR_DELTA_HISTORY=0`。

**连带订正 `PROTOCOL-agent-run.md` §17.3**:那句「两个事实跨 4 轮全部答对 →
历史确实在服务端」有混淆变量 —— 当时请求里**同时内联着全量历史**,分不开
「服务端记得」和「我们自己贴了」。上面这次是第一次把内联那份拿掉的干净检验。
于是 §17.3 的 98.7% 命中要重新归因:收益来自**内联文本被前缀缓存**,
与服务端存不存历史无关。有状态声明真正买到的是**缓存命中**,不是**免传历史**。

### Notes & Caveats

- ⚠️ **上线瞬间所有命中该指纹行的 cursor 会话会一次性迁移**:会话键变了 → 账号钉扎重来、
  服务端会话冷启动。命中率会先掉一下再回升。带显式 `metadata.user_id` 的客户端
  (如 opencode)不受影响 —— 它们本来就绕开内容哈希。
- **`fold_history` 与 `phase` 此前完全无关**(第一版我误判成有因果):`ctx.phase` 只传给
  `build_frame0` 决定环境块/预算表,从不决定要不要折。所以 cch 修复**治不了模型绕圈** ——
  那需要 `delta_history`(本次已备好,待实验后开启)。对抗评审 Skeptic 指出了这个误判。
- 在 `Continuation` 相位下折叠曾被我判为**矛盾状态**(一边声明「历史在你那儿」,
  一边又内联同一段历史 = 喂两遍),`delta_history` 就是为消掉它而写。
  实测之后这个判断也要收回:服务端根本没在存,所以内联**不是**重复,而是唯一的来源。
- ⚠️ **grok 绕圈因此仍未解决。** 「一坨长文本」这个形状目前**没有替代品** ——
  免传历史走不通(本条),repeated `1.2.1` 走不通(上游只回心跳)。
  剩下的着力点是**改渲染本身**:分隔符/角色标记,或把历史挪进 `1.2.1.2` 上下文块的
  独立条目(这才是「结构化报文」在本协议下唯一可能的形态,尚未验证)。
  已上线的 `TOOL_LOOP_NUDGE` 治的是另一个病(本轮只有工具返回、一个问题都没有)。
- 剥离判据锚定 `x-anthropic-billing-header:` **行首**(不依赖 `cc_*` 字段名,CC 升级会改)。
  代价:用户正文里若有以该前缀开头的行也会被删。这是 kiro 长期沿用的取舍
  (它的测试就锁了"中间行也剥"),沿用不改。

## [openai-wire] cursor 通道开 OpenAI 入口(chat/completions + responses)— 2026-08-14

### Features

- **`gw-core/src/openai/`**:新的边界适配层,八个文件、零新依赖(复用已有 `sha2`/`uuid`)。
  - 入站:`chat_req.rs` / `resp_req.rs` 把两种 OpenAI 请求转成 Anthropic Messages body;
    `inbound.rs` 放两者共用的零件(tools / tool_choice / 内容分片 / 采样参数 / 工具返回值)。
  - 出站:`chat_out.rs` / `resp_out.rs` 各一个流式状态机 + 一个非流式折叠;
    `usage.rs` 管两种用量形状,`error.rs` 管错误形状。
- **worker 多两条入口**:`POST /v1/chat/completions`、`POST /v1/responses`,
  **只在 `provider.family() == "cursor"` 时挂载**。转换完立刻接回 `handle_chat`
  (原 `messages` 的主体),选号 / 会话亲和 / 租约 / 重试 / 计费 / 请求日志一行没改。
- **router 多两条转发**:复用 `forward` 的全部逻辑,只把 worker 侧 URL 参数化;
  `parse_session_id` 增加 `gw_core::openai::session_hint` 回退(OpenAI body 取不到会话)。
- **`/v1/models` 同时给两套字段**:追加 `object`/`created`/`owned_by`,原 Anthropic 字段
  一个没动。一个端点同时喂饱 Anthropic SDK 与 NewAPI 的「获取模型列表」。

### Design Rationale

- **内部 IR 仍是 Anthropic,只在边界适配** —— `docs/ARCHITECTURE.md:194` 早就定了这条。
  cursor 上游本来就供 gpt / grok / gemini / kimi / glm(目录 33 项),客户却只能用
  Anthropic 协议去要它们;而 kiro / ccmax 的主链路全程 Anthropic、零转换,那是刻意保住的
  资产(thinking 签名透传、cache_read 计费),不该被 OpenAI 入口稀释。所以**只给 cursor 开**。
- **顺带砍掉一道有损转换**:今天 caio 对 NewAPI 说 Anthropic,NewAPI 再用
  `to_oai_chat_resp.go` 转成 OpenAI —— 那个转换器只认 5 种事件、没有 default 兜底
  (我方 2026-07 为保活帧实测过)。我方原生说 OpenAI,这一跳连同它的损耗一起消失。
- **闸门是结构性的,不是文档约定**:路由按 family 条件挂载,非 cursor 的 worker 上这两条
  路径根本不存在(实测 404)。策略单拎成 `mount_openai_wire()` 以便被测试点名。
- **出站只在最后一步分叉**:新增 `Wire` 枚举一路传到 `finish_response`,`Wire::Anthropic`
  分支原样调用旧函数。所有选号/重试/收尾逻辑共用一份,不复制第二条链路。
- **`reasoning_content` 而不是丢掉思考**:OpenAI 官方 ChatCompletions 没有思考字段,
  用 DeepSeek 系带起来的事实标准(NewAPI 认它);Responses 侧走 `reasoning_summary_text`。
- **请求日志与语料链路口径不变**:入库的仍是**转换后的 Anthropic body / Messages**,
  与 kiro 同一种形状,yapi 那边不用适配。

### 对抗评审(3 个 codex 视角)改掉的东西

评审报了 2 个 high + 一批 medium,**两个 high 都是真的**:

- **[高] 传输 EOF 被当成协议成功终态**(Architect#1 / Skeptic#1)。上游半截断流(没发
  `message_stop`)时,原实现给 chat 补 `finish_reason:"stop"` + `[DONE]`、给 responses 补
  `response.completed` —— 把一个**可检测的截断**变成了静默的假成功。已改成:没见过
  `message_stop` 就 EOF → 报错终态(见 `error::truncated_stream_payload`)。变异测试验过。
- **[高] cursor 的会话身份不含客户维度 = 跨客户串话**(Minimalist#1)。cursor 是唯一有
  服务端会话续写的上游(`ConvRegistry::phase_for` → `Phase::Continuation`,且
  `CURSOR_STATEFUL` 默认开),而 conversation_id 是 `hash(system + 第一条 user)` 纯内容派生。
  同组两个客户只要 system 与开场白相同,就会续在对方的服务端会话上。**这条在本次改动前
  就存在**(Anthropic 入口打 cursor worker 同样成立),但把入口开给 NewAPI 这种多租户中转
  正好凑齐碰撞前提。已在 worker 侧给 cursor 的亲和键补上客户 key 维度
  (`affinity_scoped_by_client`),kiro / dario 不动(它们没有服务端续写,共享键只影响缓存命中)。

其余已改:`/v1/models` 的 OpenAI 字段收窄到只在 cursor worker 追加(共享端点不该为一种协议
改另一种协议的响应);保活不再抢在 `response.created` 前面、也不再出现在终止事件之后;
终态之后的事件一律不再产出帧;`max_tokens` 截断改发 `response.incomplete` 且**不给半截工具
调用补 done**;`Response` 对象改为从 IR 反推真实请求参数(并写明三处有损);
`tool_choice:"none"` 改成**真的撤掉 tools**(上游只读 tools,光写字段是摆设);
空工具输出给占位、分片数组走同一套映射(两者原样透传都会被上游拒);
无 tools 却要求必须用工具 → 400;重复的块起始被忽略;快照按 `output_index` 排序;
旧版 `function_call` / `role:"function"` 往返打通;router 的 404 能力转移改为逐个排除、
不再一次就宣告失败;router 与 worker 边界前的错误(401/503/502/畸形 JSON)也换成 OpenAI 形状。

**驳回**:「删掉 Responses 省 39% 代码」(用户明确要的就是这个协议);
「`max_tokens`/`temperature` 等参数 cursor 不读,应当拒绝」——
Anthropic 入口今天对同一个 provider 同样是静默忽略,让 OpenAI 入口更严没有道理,
只有 `tool_choice:none` 是能兑现的,已单独兑现。

### Notes & Caveats

- **`max_tokens` 缺席时我方补 64k**。Anthropic 侧它必填,OpenAI 侧可选。补一个小值等于
  替客户截断回答,而截断在流式里表现为「话说一半就停」——最难被认出是网关干的那种故障。
- **`previous_response_id` / `item_reference` 明确 400**,不静默忽略:忽略等于把「续上一轮」
  变成「重新开一轮」,客户端拿到一个上下文凭空丢失的回答,查都没法查。
- **Responses 回传的 `reasoning` 条目收下即丢**。cursor 通道拿不到可回放的加密 CoT
  (见记忆 `caio-thinking-blob-extraction`),把 summary 当 thinking 塞回去会得到一个
  **没有签名**的思考块,Anthropic 家族上游直接拒收。
- **`stream_options.include_usage` 为假时不发用量帧**:严格按 `choices[0]` 解析的客户端
  会被空 `choices` 噎住。为真时用量帧必须排在 `[DONE]` **之前**,否则 NewAPI 记 0 用量。
- **保活帧按线缆分形态**:chat 发空 delta chunk,responses 发 `response.in_progress`。
  都不用 SSE 注释 —— 注释在标准解析器里被跳过、不会变成下游事件,客户端照样判定空闲。
- **`reasoning_tokens` 恒 0**:Anthropic 线缆把思考文本算进 `output_tokens`,不单独给数。
  报一个猜出来的值会让客户按它对账,比留 0 更糟。
- **托管工具(`web_search` / `code_interpreter`)静默跳过**:cursor 上游没有对应物。
  为一个我方不认识的工具把整条请求打回去,比丢掉它更糟。
- **尚未对真实上游跑过流式 happy path**:本机没有可用 cursor token 的号,且真发请求要消耗
  用户订阅额度。协议形状由单测(罐装事件序列逐帧断言)+ worker 级 SSE 字节断言覆盖;
  路由、鉴权、转发、错误形状已在本机实跑验证。

## [affinity-r4] 会话亲和:修完对抗评审两轮的高危 + 一个观测计数器 — 2026-08-14

### Features

- **溢出伙伴不再被临时候选覆盖**(第三轮 [高])。`select_id` 的溢出分支现在**再分一次
  原因**:伙伴仅仅并发满 → 本轮临时另选、**不写回** `ent.overflow`;伙伴真失效 → 才换人
  并记住。之前是无条件写回,于是「固定伙伴」在高并发下沿全池漂移,契约等于没实现。
- **上迁目标恰好是现任伙伴时对调,而不是留下重复 ID**(第四轮 [高],三个评审员各自
  独立复现)。时序 `primary=lo, overflow=hi` → 上迁 → 两个字段双双指向 hi → 此后 hi 一忙
  就被同时当成「主号忙」和「伙伴忙」,伙伴槽位被自己占死。处置是把旧 primary 换到伙伴位
  (第五轮 [中] 的建议:清空会让下次溢出按分层 LRU 另挑陌生号,平白多触达一个身份)。
- **`affinity_primary_if_only_busy` 改为委托 `entry_ok_ignoring_busy`**(第三轮 [中])。
  手写版与共享版有真实语义差异(无条件拒 `probation`、跳过 RPM 懒清理顺序),
  「判据不可能漂移」的目标名存实亡。判据只能有一个来源。
- **`sync_accounts` 删号时同时清匹配的 `overflow`,且两张表在同一临界区改**
  (第三轮 [低] + 第四轮 [中])。原先只在被删号是 `primary` 时删整条亲和项;
  且 `drop(entries)` 在亲和清理之前 —— 两个重叠的 `sync_accounts` 之间有窗口能让
  「新 B + 旧 overflow=B」复活本轮要消灭的陈旧伙伴关系。
- **新增 `affinity_spill_total`**(`QueueStats` → worker `/health` 的 `queue`):
  「两个当前槽位双双并发满、本轮临时用第三个号」的事件计数。只观测,不拦截。

### Design Rationale

- **砍掉了做了一半的 `session_account_cap` / `_enforce` 硬上限**(第四轮三人一致 [高])。
  cap 检查只长在 `partner_only_busy` 这一个分支上,而真实失效路径(primary 死了改钉、
  伙伴死了换人)全部绕过它,`AffinityEntry` 也不记历史触达集合 —— 那个「上限」拦不住
  它声称要拦的东西,却引入了三条 high(误报 503、`AllBusy` 语义污染、`busy` 陈旧)。
  加上实测触发面 **0/224 会话**(峰值在途并发 13 / 总槽位 36),决定只留最便宜的先行指标。
- 计数器口径**故意保守**:每事件 +1 而非每请求、`try_lease` 可能还会失败、`busy` 表示
  「本请求此前抢 permit 失败过」而非「此刻仍忙」—— 它是趋势信号(0 → 非 0 = 该补号),
  不是台账。

### Notes & Caveats

- ⚠️ **`affinity_hold_ms=0` 不等于回到改造前的行为**(第五轮 [高],原文档撒了谎,已改)。
  固定溢出伙伴**没有开关**、始终生效:`0` 只关掉「等 primary」这一段。
  想整体回滚只能换镜像。
- ⚠️ **`affinity_spill_total` 有已知盲区**:伙伴真失效被换人、primary 真失效被改钉,
  这两条路上会话都确实多触达了一个身份,但计数器不涨。真实「一个会话触达几个号」
  只能从 `request_logs` 按未加盐逻辑会话键重算(脚本 hopnow.py)。
  测试 `dead_partner_is_replaced_and_remembered` 把这个盲区**显式钉住**了。
- **知情不改的两条**(第五轮 [中],均为既有架构性质,不是本轮引入):
  1. 亲和状态在**选号**阶段就提交(改 primary/overflow、记 rebind),而真实触达发生在
     `try_lease` 成功之后;失败只退 RPM、标 busy,不回滚亲和。要修得引入「租约成功后
     提交」的事务边界。
  2. 每次带 session 的选号都在持 `entries` + `affinity` 双锁时对**全表**做 TTL `retain`,
     高基数 session 下是 O(N²) 并把 acquire 串行化。TTL 只限时间不限条目数。
- `sync_accounts` 的测试只覆盖**顺序**契约,没构造真正的并发窗口(把 `drop(entries)`
  移回去测试仍会绿)。写确定性竞态测试代价过高,这里靠代码注释交接。
- 四条不变量都做了**变异验证**(逐个破坏 → 对应测试 FAILED):忙伙伴不覆盖、
  上迁对调、改钉作废旧伙伴、hold=0 不改钉。534 测试通过,亲和相关 20 条连跑 5 次无抖动。


## [已完成] 账号级可用模型白名单(规格存档)

> 状态:**已于 2026-08-13 按本规格完整落地并发版**(镜像 `allowlist-20260813-105344`,
> 见下方 [model-allowlist] 条目)。本节保留为设计规格存档;
> 文末「另外两条待办」(内建截断伪装成功 / 协议内回失败结果)**仍是待做**,交接给下一位。

### 需求

Cursor 账号的**模型权限不齐**:一部分号只有 Cursor 自家模型(`composer-2.5` / `default`)
和 `grok-*`;claude / gpt 这些第三方前沿模型要另外的计费额度。现状是每个 claude 请求都
可能落到没权限的号上,换来一次上游拒绝(`ERROR_RATE_LIMITED_CHANGEABLE`,带
`autoSwitchToModel`)。调度层的动态学习会把 `(号, 模型)` 记 6h 不可用并换号,
**但那是先赔一次首包前失败才学会**:每个新上线的号、每次 6h TTL 过期都要再赔一次。
静态白名单把这次失败挪到调度之前,一次上游请求都不发。

用户原话:「有的只支持 grok 和 composer,有的都支持,需要配置每个账号哪些模型可用,
不然都支持的账号压力太大,只支持 grok 的又分摊不到压力。」

### 已有的半成品(2026-08-13 随 cursor-tool-guard-hot 一起部署,休眠)

- `crates/gw-cursor/src/models.rs`:`allow_list()` + `pub fn account_supports(account, requested)`
  —— 读 `extra.models`,收逗号串与 JSON 数组,支持 `前缀*` 通配,**判定前先过
  `to_cursor_model()` 归一**(白名单写上游侧名字,客户发来的是带日期后缀/族别名的名字)。
  已有 5 个测试(未配=不限 / 只有自家模型的号挡 claude / 归一口径 / 全名精确不吃前缀 /
  大小写与数组写法)。
- `crates/gw-cursor/src/lib.rs`:`CursorProvider::account_supports_model` 已调它。
- **刻意没进 `CURSOR_ACCOUNT_SCHEMA`**(那里留了一段注释说明原因)→ 后台不渲染输入框
  → 没人能设 → 休眠。测试 `schema_declares_credentials_and_anti_correlation_fields`
  里有一条 `assert!(!has("models"))` 钉着这个状态,做的时候记得一起改。

### 要改什么

1. **改名** `extra.models` → `extra.model_allowlist`。它是通用路由策略,不是 cursor 私有。
2. **纯匹配器挪到 `gw-core`**(如 `account.rs`:`pub fn model_allowlist_allows(account, upstream_model) -> bool`)。
   **不要**新增 `normalize_model_id` trait 方法 —— 各 provider 在自己的
   `account_supports_model` 里与核心匹配器求**逻辑与**,并复用它**真正发包时用的那个映射函数**:
   - cursor:`cursor_native_support(..) && core::model_allowlist_allows(a, &to_cursor_model(m))`
   - kiro:**暂不接线**(保留 FREE-no-opus 原逻辑)—— 用户硬约束「不影响 kiro」
   - ccmax(claude-subprocess)/ dario:**完全不接**
   理由:`Provider::account_supports_model` 的语义已经是「该账号最终能否服务该模型」,
   在 worker 外面再挂第二个判断,等于把最终答案拆给两个所有者。
3. **规范存储 = JSON 字符串数组**;CSV 只当 UI 输入与旧配置兼容。
4. **fail-open 只适用于「字段不存在」**,其余一律在**写侧** fail-closed:

   | 情况 | 行为 |
   |---|---|
   | 字段缺失 | 不限(兼容存量,绝不能因元数据缺失把健康号摘出池) |
   | 合法非空列表 | 按列表限制 |
   | `null` | 规范化成**删除该键** |
   | 空串 / 空数组 | 保存时拒绝,或明确规范化成删除键 —— **不能**解释成「不限」 |
   | 类型错 / 通配符不在末尾 | 400 拒绝更新 |
   | 真要「全禁」 | 用账号 `disabled`,不要靠白名单表达 |

   这条是 gpt 评审里最要紧的一条:运维填了个空值,本意多半是「全禁」,当成「全放」是要出事的。
5. 通配符**只许出现在末尾**;文档里写明 `grok*` 会自动放行未来的 Grok 型号
   (所以它不是严格静态白名单)。
6. **写侧接口**:加一个定点 PATCH 字段,照 `proxy_url` / `priority` 那个模式
   (`crates/gw-app/src/admin/accounts.rs` 约 1482–1508 行)走 `st.store.merge_account_extra`,
   **绝不碰凭据字段**。⚠️ 整体替换 `extra` 那条路**走不通**:`GET /accounts` 回来的凭据是
   `***` 掩码,读不回来也不能原样发回(写侧会拒绝含掩码的回传值)。
   记得把新字段加进 `has_patch` / `poke_workers_sync` 的触发条件(同文件约 1521 行),
   否则改完要等 worker 最多 30s 的周期 sync 才生效。
7. **前端**:`admin-ui/src/features/accounts/components/CursorAccountDialog.tsx` 是**手写字段**
   不是 schema 驱动的,所以要手动加输入框;加完再把 `FieldSpec` 放回
   `CURSOR_ACCOUNT_SCHEMA` 并改掉上面那条 `assert!(!has(...))`。
   **必须适配手机端**(断点从窄往宽写、宽内容裹 `overflow-x-auto`、375px 自查)。
8. **一个独立的危险点**(与白名单无关,但同一片代码):`models.rs::to_cursor_model` 把
   **未知模型名归一成 `default`**。于是只要白名单含 `default`,任意拼错的模型名都会放行,
   客户端**静默拿到另一个模型**。要把「客户端明确要 `default`」与「未知名字回退 `default`」
   分开,后者直接 400。

### 实现约束

- `account_supports_model` 由调度器**在锁内**对每个候选账号调用(`worker/mod.rs` 约 2374 行),
  所以必须**无副作用且快**:只读 `extra`,不查上游,别在里面分配 `String`
  —— kiro 那边是 370 个号的规模。
- 部署铁律:rsync **绝不** `--delete`(排除 `config/`、`data/`、`docker-compose*.yml`、
  `*.bak*`、`.git/`、`target/`、`node_modules/`、`admin-ui/dist/`);`docker build --network=host`;
  改前端要**重建整个镜像**(UI 内嵌二进制)。本次只需重启 `router` + `worker-cursor`
  (compose 第 11 / 41 行),`worker0`(kiro,第 23 行)与 dario(78 / 92 行)一律不动。
- 本地构建/测试必须限内存:
  `systemd-run --user --scope -q -p MemoryMax=10G -p MemorySwapMax=0 cargo …`

### 另外两条待办(同一片代码,优先级低于上面)

1. **内建截断仍伪装成成功**。`gw-cursor/src/chat.rs` 的 `builtin_truncated` 分支照样发
   `stop_reason=end_turn` + `message_stop`,于是 worker 判 `saw_message_stop=true` →
   **请求日志记 200 成功**,还 `h(true)` 确认了会话。12h 内 296 次半截回答在面板上全绿,
   只能靠用户报障发现。要改成不发正常 `end_turn`(发 SSE error 或换 `stop_reason`),
   并按未完成口径落库。注意首包已 committed、换不了号,目标只是「日志里是红的、
   客户端知道被截断」。**会让面板成功率掉下来,那是真实数字**(已跟用户说明)。
2. **协议内回失败结果**(根治内建工具收口)。要先用 `CURSOR_DUMP_TOOL_FRAMES=<目录>`
   环境变量抓几十帧真实内建调用帧(**需要重启 worker-cursor**,用户已同意开这个口子),
   定死 `1.2.2.<N>` 的身份枚举与**请求侧客户端回执消息形状**,然后在同一条 BiDi 流里
   回一个「工具不可用,请改用 `gwtools-X`」的失败结果,让模型在**本轮内**自己纠偏
   (纠偏次数封顶 1~2 次防循环)。
   ⚠️ **绝不猜字段号**:回执被上游忽略 = 90s 心跳死等,比现在的瞬间收口更差。
   已知线索:这些帧在**工具通道 `1.2.2.<N>`** 而不是 exec 通道(收口日志 preview 尾部有
   `<uuid>-N-<4字符>` 的 `1.2.3` 签名);exec 通道的字段号已有 schema 实证可参照
   (`2=shell, 3=write, 4=delete, 5=grep, 7=read, 8=ls, 9=diagnostics, 11=mcp, 14=shell_stream`,
   客户端回执同号),但**两条通道的回执消息不一定是同一个**。
   收口日志已经带上 `cap=`(认出的能力)与不认识的字段号,是这一步的现成线索。
   做完之后还有第三步:能可靠映射时直接把内建调用**翻译**成调用方工具的 `tool_use`,
   把失败变成无感成功。

## [model-allowlist] - 2026-08-13

账号级可用模型白名单(`extra.model_allowlist`)按上方规格完整落地并发版;
顺带补齐 cursor 账号三条用量展示与「收到」刷屏文案修正。
镜像 `claude-all-in-one:allowlist-20260813-105344`,重启 `caio-router` + `caio-worker-cursor`,
kiro(worker0)与 dario 容器不动。

### Features

- **白名单核心**:纯匹配器挪到 `gw-core::account`(`MODEL_ALLOWLIST_KEY` +
  `model_allowlist_allows`,`前缀*` 通配仅限末尾);cursor 侧 `account_supports`
  改为 `cursor_native_support && core 匹配器` 求逻辑与,kiro / dario / ccmax 零接线。
- **语义定稿**(规格第 4 条,写侧 fail-closed):字段缺失/null = 不限;
  空表/类型错/中置通配 = 400 拒绝;规范存储小写 JSON 字符串数组,CSV 只当 UI 输入。
- **未知模型名不再静默回退 `default`**(规格第 8 条危险点):
  `resolve_cursor_model` 未知名返回 None → chat.rs 报 `bad_request_visible` 400,
  拼错模型名的客户端拿到明确错误而不是悄悄换模型。
- **写侧**:`PATCH /accounts/{id}` 定点字段 `model_allowlist`(照 `proxy_url` 模式走
  `merge_account_extra`,绝不碰凭据;空串=清除写 null),`normalize_model_allowlist`
  校验+规范化,进 `poke_workers_sync` 触发条件,`redacted_view` 顶层回显数组。
- **前端**:`EditAccountDialog` 加输入框(不传=不动/空串=清除/变更才进 patch),
  `types.ts` 补 `AccountRow.model_allowlist` 与 `UpdateAccountPayload.model_allowlist`,
  i18n 中英文案;schema 放回 `model_allowlist` FieldSpec。
- **cursor 三条用量**:`usage.rs` 解析 `autoPercentUsed` / `apiPercentUsed` 装进
  `QuotaWindow`(label "auto"/"api",非法值不造窗口),`AccountTableRow` credits 分支
  渲染百分比窗口 —— 此前只显示 on-demand 美元一条。
- `fold_history` 注入文案加「不要重复致意」,修「收到」刷屏根因。

### 验证

- `cargo test --workspace` 全绿(452 通过;`cursor_login_transient_failure_keeps_session`
  并行偶发失败、单跑通过,与本批改动无关的既有 flake)。
- admin-ui:`tsc --noEmit` + vite build 通过,dist 重建后嵌入二进制(sha256 `b9d2650f…`)。
- 生产写侧端到端:设置 `"default, composer*, GROK*"` 落库为
  `["default","composer*","grok*"]`;`gr*k`(中置通配)400;空串清除回显 null,
  测试账号已恢复不限,无脏数据。worker health 正常(8 账号)。

### 注意事项

- `grok*` 会自动放行未来的 Grok 型号,不是严格静态白名单(规格第 5 条,刻意为之)。
- 真要「全禁」用账号 `disabled`,不要靠白名单表达。
- 上方规格存档里的「另外两条待办」(内建截断伪装成功 / 协议内回失败结果)仍未做。

## [cursor-tool-guard-hot] - 2026-08-13

Cursor 内建工具护栏改版 + **文案热更新**;内建截断后下一轮向模型纠偏。

### 背景

Cursor 的内建工具(终端、读写文件、代码库检索、网页搜索)是**服务端自带**的:
哪怕我方一个工具都不声明,模型照样会调,而反代执行不了 —— 只能收口。
worker-cursor 12 小时日志:外部工具成功 **3119** 次,内建工具收口 **302** 次,
其中 **296 次发生在已出字之后** —— 客户端收到「半截回答 + 正常 end_turn」,
无错误、无工具调用。用户报障原话是「工具调用总失败」,看到的就是这个。

落盘 preview 里模型要调的是 `echo …; grep -nE …`(shell)和「按符号名+目录搜代码」,
**全都是调用方已经声明过的能力**(Claude Code 的 `Bash` / `Grep`)。第二版文案
(「声明的工具都是真的,但不要调内建的终端/文件读写/网页搜索/代码库检索」)在这种
局面下**自相矛盾**:被点名禁掉的能力与调用方的 `Bash`/`Read`/`Edit` 逐字重合。

### 改动

- **护栏文案 v3**(`gw-cursor::chat::builtin_tool_guard`):从「禁什么」改成三条正向信息
  —— ① 逐个列出可调用工具的**全名闭集**;② 能力→工具**替代表**(按调用方实际声明的
  名字生成);③ 策略句。工具个数的硬阈值换成 1200 字符预算。
- **能力匹配去掉纯子串档**,只留「全等 > 去符号全等 > 前缀(≥4字符)」:子串会让
  `Thread` 命中 `read`、`TodoWrite` 抢到「写文件」。宁可不出替代行也不指错工具。
- **文案热更新**:`SystemConfig.cursor_tool_guard` + `SystemSettings` overlay
  → worker 启动初载 & 30s 设置环 → `gw_cursor::set_tool_guard_policy()`
  (`cursor_extra_models` 同款先例)。**只有策略句是配置**,闭集与替代表由代码生成。
- **可观测**:`/health` 回显 `cursor_guard_rev`(4 字节指纹,**不回显全文**);
  内建收口 WARN 带 `guard_rev` + `cap`(认出的能力)。
- **下一轮纠偏**(`TruncationNotices`):内建收口按 conversation 记一笔,下次同会话
  请求在用户消息末尾补「上一轮你调了本环境不提供的内建工具,请改用 `gwtools-X` 重做」。
- 写侧校验:`PUT /admin/api/settings` 拦 `cursor_tool_guard` 超长(2000 字符)。

### 设计取舍(gpt-5.6-sol 对抗评审)

- **不做 `{tools}`/`{redirects}` 占位符模板**:动态部分的结构不会变,变的只是那几句
  自然语言。模板一旦拼错会**静默丢掉整个闭集**,而闭集是这道护栏最硬的一半。
- **默认文案不含后果威胁、不点名 Cursor 内建工具**:「否则回答会被截断」可能让模型
  对合法工具也变保守、或转头在正文里解释网关环境;点名内建能力则重演 v2 的自相矛盾。
  两者作为**热配置里的实验变体**存在(改设置即可试),不作第一版默认。已钉成测试
  `默认护栏不含后果威胁也不点名内建工具` —— 防止后人「顺手把话说狠一点」种回旧病。
- **校验失败保留上一份有效值**,不静默回默认:护栏效果正按 `guard_rev` 分桶比对
  收口率,悄悄换一版会让那份数据作废。
- **纠偏认不出能力时不指名工具**:字段号只认抓包实证过的 `.1` 终端 / `.4` 读文件,
  其余记日志返回 None。「你上次调了内建终端」这种自信的错话比模糊的实话更容易带偏模型。
- **纠偏不挂在 `ConvRegistry` 上**:那张表被 `CURSOR_STATEFUL` 门控,而半截回答在两种
  模式下都在客户端历史里 —— 挂进去等于让一个退路开关顺手关掉一个不相干的修复。
- **命名空间隔离**:设置项走 `cursor_` 前缀、代码全在 `gw-cursor`、apply 函数对非
  cursor worker 是无害 no-op。kiro / claude-dario / claude-subprocess(ccmax)零改动。

### 注意事项

- 这是**缓解不是根治**。根治要在同一条 BiDi 流里回一个「工具不可用」的失败结果,
  让模型在**本轮内**自己纠偏。但内建调用在**工具通道 `1.2.2.<N>`** 而不是 exec 通道
  (preview 尾部有 `<uuid>-N-<4字符>` 的 `1.2.3` 签名),客户端回执消息形状未定;
  **猜字段号的代价是回执被忽略 → 90s 心跳死等,比现在的瞬间收口更差**。
  下一步靠 `CURSOR_DUMP_TOOL_FRAMES` 抓实物定死。
- 已知未修:内建截断仍发 `stop_reason=end_turn` + `message_stop`,于是 worker 判
  `saw_message_stop=true` → **请求日志记 200 成功**。那 296 次在面板上是全绿的,
  只能靠用户报障发现。单独一条待办。
- 账号级模型白名单(`extra.models`)逻辑已在 `models::account_supports` 并接上
  `account_supports_model`,但**刻意未进 account schema**(后台不渲染):空值/非法值的
  fail-open 语义与字段命名还没定稿,先上会让人照一版将来要改的语义配一遍。

### 验证

- `cargo test --workspace`:**1335 通过 0 失败**。
- 顺带修一个真 flake:文案测试原本读进程全局策略句,与热配置测试互相顶
  (「单跑全绿、全量跑随机红」)。拆出纯函数 `builtin_tool_guard_with(tools, policy)`
  后文案断言完全不碰全局。

## [cursor-hot-catalog] - 2026-08-13

Cursor 模型目录支持**热追加/覆盖**:加模型、试新模型不再需要重新部署。

### 背景

cursor 上游不提供菜单查询,模型表(含每模型参数)只能内置在代码里
(`gw-cursor::models::catalog`,2026-08-10 对齐真机菜单 33 项)。代价是每加一个
模型都要改代码、构建镜像、切容器 —— 08-13 加 grok-4.6 时又走了一遍全流程,
而它本来只是一次「上游认不认这个名字」的试探。

### 改动

- `gw-core`:`SystemConfig.cursor_extra_models: Vec<ExtraModelSpec{name, params, menu}>`
  + `SystemSettings` 同名 overlay 字段(整表替换语义,与 warmup_group_policies 一致)。
- `gw-cursor`:进程级 `EXTRA_MODELS`(RwLock)+ `set_extra_models()`(lib 单点导出);
  `catalog()` = 内置目录 + 热追加项(**同名整体覆盖**,参数一起换)。
  `menu=false`(默认)= 探测位:可被点名、出现在 `/v1/models`,但**不进** `1.14`
  清单(每个 Run 请求都带的那份)——热配置只允许试,菜单位要回代码对齐真机快照
  才转正(08-13 gpt-5.6-sol 一审高危:未证实条目混进 1.14 会污染所有模型的请求)。
- `gw-app`:`apply_cursor_extra_models()` 接进 worker 启动初载与 30s 设置环
  (cache_sim 同款先例);非 cursor worker 是无害 no-op。空名条目读侧丢弃。
- admin-ui 无需改动:设置页按字段构造 patch,不会冲掉这个键。

### 验证

- `cargo test --workspace`:14 套件 1315 通过 0 失败(新增:
  热追加覆盖/追加/probe 标志保留/清空恢复;frame0 编码层探测项不进 1.14;
  测试间并发用 CATALOG_TEST_LOCK 串行)。
- 线上热配验证:DB 写探测项 → 30s 内 `/v1/models` 可见 → 清空即消失。

## [cursor-on-demand] - 2026-08-12

Cursor 账号的**超额(on-demand / usage-based)额度**在面板里可读可改:账号表新增
「超额(已用/上限)」列,点击即开设置弹窗(预设档 / 自定义金额 / 关闭)。

### 背景

`ultra-test` 在 08-11 事故里被 2 RPM 卡住,同期 `test` 号是另一种死法:**$20 账期
烧穿**后上游直接拒。套餐额度面板早就看得见,但「超额开没开、上限多少、已经烧了
多少」只能去 cursor.com 网页翻 —— 号一多就等于没有监控。这一版把它搬进面板。

### 上游协议(解包客户端 + 真号实测得到,非文档)

`aiserver.v1.DashboardService`(与已有 `GetCurrentPeriodUsage` 同一个 service):

- `GetHardLimit` → `{hard_limit, no_usage_based_allowed}`。**proto3 零值不序列化**,
  所以 `no_usage_based_allowed` 缺省 = false = **已开启**(不是"未知")。
- `SetHardLimit`:开启 `{hardLimit:N, noUsageBasedAllowed:false,
  preserveHardLimitPerUser:true}`;关闭 `{noUsageBasedAllowed:true, ...}`。
- **两个接口单位不一致**,是这一版最容易踩的坑:`hard_limit` 是**美元整数**,
  `GetCurrentPeriodUsage.spendLimitUsage` 是**美分**。内部统一归一成美元。
- `hardLimit = 2147483647`(i32::MAX)是**不限额哨兵值**,不是真上限 —— 照原样显示
  会变成 "$2147483647",故单独归一成 `unlimited`。

### 改动

- `gw-core`:`OnDemandQuota{enabled, limit, used, unlimited}`,挂到 `AccountQuota.on_demand`;
  Provider trait 加 `on_demand_supported()`(默认 false)/ `set_on_demand_limit()`
  (默认 Unsupported)—— 别的 provider 零改动。
- `gw-cursor`:三端点调用抽出共用 `dashboard_call()`;`account_quota` 合并
  `GetHardLimit`。**这一跳失败只 debug 日志、不让整个配额查询失败**:套餐额度是账号页
  主信息,不该被一个附加字段拖垮(超额栏退化成"—")。已用金额只有用量接口有,
  合并时从上一步结果接回,否则会被覆盖成 0。
- `gw-app`:`POST /accounts/{id}/on-demand`(worker + admin 顺序扇出)。
  `limit_usd` 校验 `0..=i32::MAX`,**超范围直接拒不静默截断**(会把"设 $50"变成别的数);
  `0`/`null` = 关闭。走 `quota_sem`(控制面对上游并发只此一处),成功后回读并刷配额缓存
  (否则运维要等一个 TTL 才看到新值,会以为没生效)。
- **写失败不计失败池**:一次计费设置被拒与 chat 可用性无关,不该让号进冷却。
  上游拒绝原文(如 `Payment method required`)从 Connect `details[].debug` 抽出透传到面板。
- admin-ui:超额列**仅 cursor 视图出现**('all' 混着多种 provider,给非 cursor 行挂一列
  「—」像是查不到而非不支持);已用达上限 80% 标黄;未开启显示「关」、离线/未采集显示
  「—」(不显示假的 $0)。中英文案齐全。

### 已知限制

- 超额上限**只支持整数美元**,因为上游字段就是 i32。
- 未绑支付方式的号开不了:上游回 400 `failed_precondition` /
  `onDemandPaymentMethodRequired`,**客户端同样开不了**,不是本功能的 bug ——
  需先去 cursor.com/dashboard 绑卡。线上 `ultra2` 正处于此状态。

### 验证

- workspace 1312 测试全绿;gw-cursor 新增 9 条(含实测响应做金标准样例:
  `{"hardLimit":75}` + `individualLimit:7500` 美分,正好覆盖单位不一致这个坑)。
- admin-ui tsc + build 通过,60 测试全绿(新增 7 条覆盖 off/unlimited/缺省上限/
  80% 阈值/美元格式)。
- 真号实测:`ultra-test` 设 `hardLimit=75` 成功并回读一致。

## [group-warmup-and-rpm-wait] - 2026-08-11

两件事同源于 2026-08-11 CUR 组 503 事故:唯一合格的 cursor 号 `ultra-test`
被 **2 RPM** 卡住时,客户请求一秒不等直接 503,日志还报「组内所有账号均已禁用」。

### 根因(与最初怀疑的「亲和钉死」不符,代码走读后订正)

1. 那 2 RPM 不是账号配置,是**全局新号暖机**(默认开,rank ≥ 100、适应期 2h@2rpm)
   按 rank 一刀切,cursor 订阅号也被限 —— 暖机本是为 kiro 补货新号设计的
   (2026-08-10 ha7477062)。
2. 「等 RPM 窗口腾名额」的分支绑死在排队模式预算(`queue_wait_ms`,默认 0)上,
   没开排队的组直接跌落 `AllDisabled`(文案谎称"全禁用",客户 503)。

### 改动

- **RPM 闸等待与排队解耦**:新预算 `scheduler.rpm_wait_ms`(默认 10000,热调,
  钳到 ≤ RPM 窗口 60s),与 queue_wait 取大;等待不消耗换号预算、分片 ≤200ms
  重选、等待者上限 64(RAII 名额,超出快速失败,防突发堆积+惊群抢锁)。
  预算耗尽仍等不到 → 报新变体 **AllRpmLimited**(503 与客户端文案不变,
  但日志不再谎称「全禁用」或「并发满」)。
- 混合「A 并发满 + B 被 RPM 闸住」时,busy 等待分支不再烧 attempts 抢跑,
  让给 RPM 分支(不耗预算、分片重选,期间 permit 释放照样命中)。
- **按分组暖机策略** `scheduler.warmup_group_policies`(热调,设置页可视化编辑):
  `{组名: {rpm, hours}}`。命中的组完全接管(单期,到期毕业;`hours=0` = 该组
  显式关闭暖机,全局开关也压不住);未列出的组走全局两期,kiro 补货保护不变。
  GroupView 带上分组名,暖机口径跟随请求的成员视图。
- 设置写侧宽松化:overlay 里残留「更新镜像写入、本版本不认识」的键不再锁死
  普通写入(只拒**本次 patch** 里的未知键;显式 `null` 可清残留)——回滚
  旧 router 后设置面板仍可用。
- 设置页新增「新号暖机」卡片(全局开关 + 两期参数 + 分组策略编辑器)与
  调度卡的 rpm_wait_ms 输入;此前暖机只有 yaml/API、面板不可见。

### 即时处置(运维,非代码)

- `ultra-test` 优先级 100→0(高优豁免暖机),CUR 组容量当场恢复;
  `test`($20 账期烧穿,fable 被限)手动禁用。

### 验证

- gw-app 447 全绿(+4:策略接管/豁免/更严、RPM 等待、AllBusy 分类),
  workspace 全套件零失败;admin-ui tsc + 53 测试全绿。

## [kiro-legacy-wire-switch] - 2026-08-11

`KIRO_LEGACY_WIRE=1`:Kiro 上游报文**整体**退回 2026-07-28(`58b6f27` 对齐 1.0.212)
之前的线缆形态。默认关 = 现状一字节不变。gw-kiro 相关测试全绿。

### 背景

08 上旬 PRO+ 付费号批量 TEMPORARILY_SUSPENDED(实测 0/35 申诉恢复 = 永封)。
周末调查(`.ban-investigation.md`,E1–E8)未锁定根因,但"07-28 线缆形态变更引入
新指纹"在可疑内因里排位靠前:旧形态(UA 0.12.155 时代)跑了几个月无大规模封号。
本开关是最低成本的可逆判别实验 —— 不用 git revert,这两周的新功能
(ListAvailableModels/模型目录/RPM 闸门/conversationId 加盐等)全部保留。

### 总开关协调的维度(只支持两种自洽形态,杜绝混搭)

| 维度 | 默认(1.0.212) | legacy(0.12.155 时代) |
|---|---|---|
| UA 自报版本 | 1.0.212 | 0.12.155 |
| body 顶层 `agentMode` | 必发 `"vibe"` | 不发 |
| `additionalModelRequestFields` | 按策略发 | 不发(改走旧文本标签) |
| 当前消息空 `userInputMessageContext` | 省略 | 补发 `{}` |
| 思考强度载体 | 结构化字段 | `<thinking_mode>/<thinking_effort>` 标签 |
| 配额/profiles 控制面域名 | management.*.kiro.dev | q.*.amazonaws.com |

### 实现要点

- 新增 `gw-kiro::wire_profile`:总开关与形态说明的单一出处。形态切换全部落在
  **构造点与 serde 谓词**上(chat.rs 置 None、conversation.rs 谓词读开关),
  不做序列化后改写 —— struct 字段顺序就是线缆字节顺序(与真客户端 key 顺序
  逐字对齐过),二次 Value 往返会排成字母序,反而偏离金标准。
- legacy 下旧文本标签词表同步回旧:`max` 写作 `xhigh`(max 是 1.0.212 才进
  enum 的档,旧线缆从未出现)。该翻译**只**随总开关生效;单独开
  `KIRO_LEGACY_THINKING_TAGS` 的混搭路径保持既有字节(max 原样)。
- `KIRO_LEGACY_THINKING_TAGS` / `KIRO_LEGACY_QUOTA_ENDPOINT` 两个细粒度开关
  保留原语义(只翻单一维度,作应急);总开关与它们是 OR 关系。

### 刻意的例外(功能保留,不回退)

- `ListAvailableModels` 继续走 management 域(旧域无此操作;admin 手动触发,
  不在逐请求热路径)。
- conversationId 按账号加盐(08-07 防关联修复)不回退。
- 思考预算下限 8192、默认档位 high 是策略(值),不是协议形态,保留。

### 部署注意

- 开关只需加在 **worker**(报文在 worker 侧生成);router 不碰上游。
- 生产 worker 开着 `KIRO_THINKING_IN_HISTORY0=1`:若当前 env 没有
  `KIRO_LEGACY_THINKING_TAGS=1`(即现在不发标签),开总开关会让标签重新注入
  history[0] = 缓存前缀第一块字节变化,在途会话下一轮全量 miss 一次。低峰切换。
  若当前已设 `KIRO_LEGACY_THINKING_TAGS=1`,则前缀无变化,随时可切。

## [cursor-quota-and-adaptive-thinking] - 2026-08-10

两件事:① Claude Code `thinking.type=adaptive` 导致无首字挂满 5 分钟;
② Cursor 账号表接官方账期用量(`GetCurrentPeriodUsage`)。

### adaptive 无首字 / upstream stalled 300s

生产请求 `thinking: {type:"adaptive"}` 时,收侧只认 `enabled` → 不透传 `1.4`,
但思考帧仍刷新 `last_progress`,90s 心跳 watchdog 失效;客户端等到 gw-app
`STREAM_IDLE_ABORT`(300s)才报 `upstream stalled: no event for 300s`。

- `client_wants_thinking` 认 `enabled` **与** `adaptive`
- 未透传的思考不再刷新 `last_progress`
- `apply_thinking_pref` 对 `adaptive` 显式开上游 `thinking=true`

### 官方额度表(方案 A)

- `gw-cursor::usage`:只读 `DashboardService/GetCurrentPeriodUsage`(美分→美元)
- `CursorProvider::account_quota` 走账号专属出口(与 refresh/chat 同 IP)
- admin-ui:`quotaKindForProvider('cursor')` → `credits` 列展示剩余/上限美元

## [cursor-tool-use-usage-fallback] - 2026-08-10

cursor 通道 `tool_use` 收口时输入/缓存为 0 的计费空洞。gw-cursor 相关测试覆盖。

### 背景

生产 `claude-fable-5` 近两小时成功请求里约 39/40 条 `input_tokens=0`、
`cache_read=0`,只有 output —— new-api 面板显示「0 / N」。根因:外部工具轮
反代一问一答会在 tool_use 处 `break`,上游通常不给 `1.14` 用量帧;旧回退只估
`output = chars/4`,输入与缓存恒为 0。有 `1.14` 的 end_turn 轮不受影响。

### 修复

- 无上游用量时按请求体(system + turns + tools)chars/4 粗估 input;多轮把非本轮
  前缀估成 `cache_read` / `real_cache_read`。
- 工具/内建收口前若同帧已带 `1.14`,先收下再 break(合帧不丢)。
- 上游自报用量同步写入 `real_cache_read_tokens`(cursor 无模拟/真实双轨)。

## [cursor-model-catalog-33] - 2026-08-10

gw-cursor 模型目录 8 → 33 项。gw-cursor **120 项测试通过**。

### 背景

旧目录是 2026-08-07 抓包逆向时的客户端菜单快照(Run 1.14 字段,仅 8 项),
`claude-opus-4-8` / `claude-haiku-4-5` 这类名字会被按族归并到 5 系/composer,
面板 `/v1/models` 显得"模型很少"。

### 来源与口径

新目录取自真机 Cursor 客户端 `state.vscdb` 内服务器下发的
`availableDefaultModels2`(33 项,含每模型 parameterDefinitions)——比抓包
快照新且权威。参数默认值按菜单声明范围选取(claude 系 thinking=true/effort=high、
gpt 系 reasoning=medium、gemini-3.6-flash effort=medium 等)。目录已有的精确名
原样透传(不再归并),目录外名字仍按族归一。菜单=上游提供,能否实调取决于
账号计费状态(pro 号第三方前沿模型超量会降级 grok-4.5),这是既有上游行为。

## [low-priority-warmup] - 2026-08-10

低优先新号暖机(动态有效 RPM)。全量 **1230 项测试通过**(+10)。

### 背景:补货新号上线即被鲸客流量灌死

2026-08-10 `ha7477062` 封号排查:restock 补货的新号一上线就全速进轮转,
被鲸客的瞬时流量几分钟打到 `TEMPORARILY_SUSPENDED`。上游对新号的节奏容忍
远低于老号,而既有 `rpm_limit` 只能逐号手配 —— 补货号落地即高优流量的
泄洪口,等不到人去配。

### 方案(gpt-5.6-sol 评审修订定稿)

低优先号(调度 rank >= 100)按号龄(`accounts.created_at`,对补货号≈上游
激活时间)获得更低的**有效 RPM 上限**;高优号(rank < 100,成员边 @0 的
POWER/PRO MAX 等)**完全不受影响**:

- 适应期:号龄 0 ~ 2h,有效 RPM = 2
- 爬坡期:号龄 2 ~ 24h,有效 RPM = 6(期边界左闭右开:恰好 2h 进爬坡,恰好 24h 毕业)
- 毕业:> 24h,恢复账号自身配置
- 有效 RPM = **min(账号 `extra.rpm_limit`, 暖机上限)** —— 暖机只收紧,
  绝不放宽账号已有的更严上限。

### 实现要点与取舍

- **单点抽象防口径分叉**:`CredentialState::effective_rpm_limit(rank, warmup, now)`
  是所有 RPM 判定/记账的唯一入口,`eligible_ids` / `tier_hold_wait` /
  `queue_tier_hold_wait` / `select_id` 原子预留+退还 / `queue_probe` /
  `queue_stats` / 状态快照全部走它;`rpm_*` 方法族改为显式接收算好的上限,
  编译器保证没有调用点能绕过。
- **rank 口径**:有分组视图的调用点用成员边 rank(与选号分层同源,同号在不同
  组视图下跟随该次请求的层);无视图的点(`queue_stats` / `status_snapshot` /
  `note_upstream_call`)用 `default_rank` 兜底 —— 只影响观测/记账,60s 滑窗自愈,
  真正的准入判定(生产每请求都带视图)不受影响。取舍写在
  `effective_rpm_limit` 注释里。
- **`Account.created_at`**(serde default 0):0 = 未知号龄(accounts.yaml 降级
  加载/手工构造)→ 按已毕业处理,**fail-open 到旧行为**,绝不因元数据缺失把
  老号当新号限流;时钟回拨按号龄 0(吃最严档)。`load_owned_accounts` 直通
  DB 列;token 刷新走 clone 回写,created_at 天然存活。各 crate 测试桩补
  `created_at: 0`,既有调度测试零改动。
- **不改并发、不改 tiered_lru 排序、不改会话亲和**:RPM 达限后的等待/迁移/503
  行为完全沿用既有定频语义(软状态,不进禁用统计,不触发 AllDisabled)。
- **配置热更**:`scheduler.warmup_*`(enabled 默认 true,phase1 2h/2,phase2 24h/6)
  走 settings overlay(`SystemSettings` 同名字段),30s 内生效;RPM 上限 clamp ≥1
  (0 = "一次都不许"的笔误会静默停掉所有新号,想停有总开关)。
- **可观测**:`AccountStatusSnapshot` 加 `warmup_phase`(null=不暖机/高优,
  0=适应期,1=爬坡期),`rpm_limit` 字段改报**有效**上限(含暖机 min)——
  经 /health → router 聚合 serde 直通,前端本期不改。

### 测试(+10)

- `warmup_phase_cap_period_boundaries`:三期边界(恰好 2h/24h 左闭右开)。
- `warmup_phase_cap_exemptions_and_edge_cases`:高优豁免、未知号龄 fail-open、
  时钟回拨吃最严档、开关关闭。
- `warmup_never_relaxes_stricter_configured_rpm`:min 语义(配置 10→2、配置 1→1、
  老号 10→10)。
- `warmup_caps_new_low_priority_account_old_account_takes_over`:打满退出合格集、
  老号承接、快照三字段。
- `warmup_phase2_allows_six_per_minute`:爬坡期 6 格,第 7 次溢出。
- `warmup_exempts_high_priority_new_account`:高优新号连打 8 次不限。
- `warmup_disabled_means_no_cap_for_new_accounts`:开关关闭 = 旧行为。
- `warmup_tuning_hot_update_takes_effect`:update_tuning 热关立即不限。
- `settings_warmup_apply_to_overrides_and_preserves`(gw-core):overlay 覆盖/保留语义。
- `load_owned_accounts_carries_created_at_for_warmup`(gw-store):号龄列直通。

### 审查修复(gpt-5.6-sol 暖机复审,1 高 2 中;+3 测试)

- **[高] 同一 lease 的追加调用能突破暖机硬上限**:`note_upstream_call` 从「只记不拦」
  改为**原子准入**——同一把 entries 锁内检查有效上限(含暖机),达限返回 `false`
  且不多记,调用方不得硬发。各调用点语义:token 刷新重试被拦 → 不上报失败
  (rt 刚证有效,按 TokenInvalid 报会永久误禁)、直接换号;profileArn 修复重试被拦 →
  按「无法修复」落入统一失败处理;Overloaded 退避被拦 → 按退避用尽返回最近一次
  过载错误(对外 529,客户端自动重试);web search 续轮被拦 → 优雅降级收尾
  (已得内容照常返回,保 usage,`OnUpstreamCall` 改 `Fn() -> bool`);人工探针被拦 →
  如实回 `stage: "rpm_limited"`。
- **[中] 追加调用的 rank 口径与准入同视图**:`note_upstream_call` 加 `view` 参数,
  生产链路(刷新重试/修复重试/过载退避/web search 续轮)全部传请求的成员视图 ——
  「成员边低优但 extra.priority 高优」的号,追加调用不再被 default_rank 错误豁免、
  不再漏记真实调用。仅人工探针(无分组上下文)传 None 按 default_rank。
- **[中] RPM 退还竞态**:预留改发**句柄**(entry 内递增序号,deque 存
  `(Instant, u64)`),`select_id` 随选中 id 返回句柄,`rpm_refund` 按句柄精确删除 —
  不再无脑 `pop_back`(并发下尾部可能是别人的真实调用,退错 = 替别人的超频开窗)。
  退还也随之不再需要回传视图重算口径。

### 复审修复(gpt-5.6-sol 第三轮,1 高 1 中 1 低;+1 测试)

- **[高] profileArn 修复验证被拦会误杀刚修好的号**:验证调用被 RPM 闸门拦住时,
  上一版把修复前的 e2 透给统一失败处理 → 按 TokenInvalid 上报 → 永久误禁
  `invalid_refresh_token`。暖机一期 RPM=2 下这是**确定性路径**(首发 1 格 →
  刷新重试第 2 格遇旧 ARN 403 → 新 ARN 发现成功 → 验证调用达限)。修为照
  token 刷新分支语义:被拦 ≠ 修复失败,不上报、释放 lease 换号,预算/时限
  用尽才透原始错误。
- **[中] 过载退避的记账时机**:准入(检查+记账)从退避睡眠**之前**挪到睡眠
  完成后、调用发出前 —— 任务在睡眠期被取消不再留下从未发送的假 hit,
  滑动窗口也不会按过早的预留时刻提前放出名额。
- **[低] 补 worker 调用点分支测试**(`heal_retry_blocked_by_rpm_does_not_report_or_disable_account`):
  mock provider(chat 恒 403、refresh 成功、强制发现 ARN 成功)直调 messages
  handler,断言验证调用未发出(chat 计数 = 2)、账号未禁用、无 TokenInvalid
  上报、被拦调用未记账。这是本仓库第一个 handler 级 mock provider 测试。

## [account-models-local] - 2026-08-10

「账号可用模型列表」收尾 + 修一个线上确诊的扇出超时 bug。全量 **1220 项测试通过**(+5),
admin-ui **53 项**(+4),`bun run build`(tsc strict)绿。

### 背景:面板看不到「这个号还能用哪些模型」

model-marks 落地后,状态徽章能告诉你「这号被逐模型拒过 N 个」,但回答不了
「那它到底还能用哪些」——要看档位静态支持表 × 已学标记的乘积,只能翻代码心算。
本次把它做成一个端点 + 一个弹窗。

### 账号可用模型清单(纯本地,零上游)

- **worker**:`GET /accounts/{id}/models/local` 已先行落地(见 worker/mod.rs
  `models_local`)——静态目录 × `account_supports_model` 档位支持 − 已学
  INVALID_MODEL_ID 标记,纯内存读,毫秒级;目录外的标记(上游新模型/变体名)
  单独透出不丢掉。「可用」口径是**本地认知**:未观察到拒绝 ≠ 上游保证,
  上游真相仍靠「拉目录 / 探针」。
- **admin 扇出**:`GET /admin/api/accounts/{id}/models/local`,与 quota/models
  同款顺序扇出(2xx 透出,404 问下一个,其余记首个错误;全不持有 → 404
  「没有 worker 持有该账号」)。
- **面板**:kiro 行操作区新增「查看模型」图标按钮(其它 provider 无此概念不显示),
  弹窗三态展示:可用 / 被标记(附剩余重探时间 `formatMarkTtl`)/ 档位不支持,
  外加目录外标记区;三态推导 `deriveModelAvailability` 是纯函数(标记剩余 0s
  仍算标记中,与后端 `is_none()` 对齐),配 Vitest。

### 修复:quota/models 扇出 2s 超时误报「没有 worker 持有该账号」

线上确诊:`models_account`/`quota_account` 扇出用 `st.http`(2s 管理面聚合 client),
而 worker 侧要真打上游控制面(拉目录/查配额),>2s 是常态 —— 传输超时在扇出循环里
等价于「该 worker 离线」,全部轮完误报 404,直连同一 worker 却是 200。probe 端点
注释里早有同款踩坑记录(用 120s)。修法:三个扇出共用 `control_plane_http()`
(30s;控制面 GET 正常秒级,30s 兜住抖动又不挂死管理页),`st.http` 的其它用途
(聚合 /health 等需要快速跳过离线 worker 的场景)不动。回归测试用 3s 慢应答
worker 锁死:旧 client 必现的误判,现在拿到 200。

### 审查修复(gpt-5.6-sol 二审,2 中 3 低)

- **[中] models/local 扇出误用 30s client**:纯本地端点毫秒级返回,串行扇出时一个
  挂住的 worker 会把面板请求拖 30s。改回 2s 的 `st.http`(与其它纯本地管理面调用
  一致),30s 只留给真打上游的 quota/models;两处注释同步改正。
- **[中] 弹窗数据新鲜度**:全局默认 staleTime 是 30s(此前注释误写 0),重开弹窗
  可能展示旧缓存。`useAccountModelsLocal` 加 `staleTime: 0` + `refetchOnMount:
  'always'`(每次打开必重新取快照);i18n 剩余时间表述从「倒计时」口径改为
  「查询时快照」口径(zh/en)。
- **[低] 慢应答回归测试挪位**:models/local 改回 2s 后,3s 慢应答测试打在它上面
  必挂;移到真正出过事的 quota 扇出(3s 慢应答假 worker 打 `POST /accounts/{id}/quota`,
  断言 200 而非 404)。
- **[低] 弹窗标题名不副实**(列表含不支持/被标记条目):zh 改「模型支持情况」、
  en 改 "Model availability",集合公式文案改为「静态目录 + 档位支持判断 + 已学标记」。
- **[低] 前端类型过宽**:`AccountModelEntry` 的 `display_name`/`mark_remaining_secs`
  由可选改为必填可空(`string | null` / `number | null`),与 worker 实际输出对齐;
  测试桩同步补齐。

## [model-marks-observability] - 2026-08-10

封号潮排障实录(2026-08-10):唯一活号 `kiro-apikey-d8ac5a33253d` 面板显示「正常」
(未禁用/无冷却/额度 9988/10000),实际服务全 503——它对上游的每个模型都被
`INVALID_MODEL_ID` 拒绝(上游"半死":配额接口还读得出、模型调用全拒),caio 按设计
打了 `(账号,模型)` 不可用标记并绕开,但标记是纯内存、面板和 runtime API 完全看不到,
排查只能翻日志。本次把这个观测盲区补上。全量 **1214 项测试通过**(+2),
admin-ui **38 项**(+2)。

- **runtime API 暴露标记**:`AccountStatusSnapshot` 新增 `model_unavailable`
  数组(模型名 + 标记剩余重探秒数),经 worker `/health` → router 聚合 → 面板,
  全链路 serde 直通,旧 worker 缺字段时前端按"无标记"降级。
- **面板**:状态徽章旁新增橙色「模型不可用×N」徽章,title 逐模型列出剩余重探时间
  (`formatMarkTtl`:<1h 显分钟,否则显小时)。
- **救号按钮覆盖该状态**:挂着模型标记的号也会出现救号按钮,且 worker 侧
  `reset_account` 现在**一并清除该号的模型不可用标记**——否则 reset 返回成功、
  号仍被标记挡在选号池外直到 6h TTL 自然过期(与此前清 RPM 窗口同款理由,
  锁顺序 entries → model_unavailable,与选号谓词一致)。

### 审查修复(gpt-5.6-sol 一审)

- **[高] 剩余重探秒数方向写反**:`t.saturating_duration_since(now)`(标记时刻 − 现在)
  恒饱和为 0,导致过期标记滤不掉、剩余时间恒为 TTL。修为 `now - t`,并补
  「剩余递减 + 过期消失」的针对性测试(旧断言只查"在范围内",抓不住方向错误)。
- **[中] 自动退役被配置停用遮蔽**:退役落库 `disabled=1`,而 `deriveAccountStatus`
  先判 `row.disabled`,红「已退役」标签成死代码。修为 runtime 在线且报
  `suspended_retired` 时优先展示退役,并补优先级/分桶测试。

## [suspend-lifecycle] - 2026-08-10

封号调查(证据见 HANDOFF-2026-08-10-ban-investigation.md)的落地修复,
按 gpt-5.6-sol 方案审查的三轮意见做全。全量 **1212 项测试通过**(+15)。

### 背景:死号每小时整批复活,把客户原文泼向尸体

生产实锤:一个真实客户请求 32 秒内被泼到 12 个账号(11 个复活中的死企业号 +
1 个活 PRO+)。机制链:suspend 固定 3600s 冷却 → 到期整批复活 → 客户端重试的
新请求挨个踩中刚复活的尸体。同一段客户原文反复跨身份重放,是喂给上游风控的
池关联证据。同时 ~100 个假冷却号让 restock 的 healthy 计数频繁触底
(`min_healthy=1`),3 小时白买 6 个号(¥30/个)。

### suspend 生命周期状态机(仅 kiro 启用)

- **指数退避 + 抖动**:第 k 次连续 suspend 冷却 = `suspended_cooldown_secs * 2^(k-1)`,
  封顶 `suspended_backoff_cap_secs`(默认 24h),±20% 确定性抖动(FNV(账号,档位),
  不引 rand 依赖)——「整批死号同一秒复活」的同步性被打散。
- **复活观察期(probation 单飞)**:冷却到期不再满血回轮转,并发限 1,直到一次
  **完整成功**(非流式折叠成功 / 流式走到 `message_stop`)才满血;再失败吃下一档
  退避。观察期占用的号在取号分类里按"忙"认领,不掉进 AllDisabled 误报 503。
- **世代闸门(suspend_gen)**:lease 快照选号时的世代;旧世代在途请求的迟到
  成功/失败一律不改写新状态——同批并发里第一个 suspend 计击后,其余回声与
  迟到成功都不再干扰退避(审查阻断 #1/#2)。
- **自动退役**:连续 `suspend_retire_strikes`(默认 3,0=关闭,可热调)次复活失败
  → 持久禁用 + `disabled=1` 落库,面板「已退役」,restock 直接判死(可回收),
  不再每小时复活闪烁。
- **状态落库**(新表 `account_lifecycle`):退避档位/retry_at/退役标记全部持久化,
  每次状态转换即入队、worker sync 循环落库;重启按库水合——不重抽抖动、
  不让"已退到 24h 档"的号因发版提前复活(审查阻断 #5)。
- **人工恢复原子化**:面板「启用」= `disabled=0` + 删生命周期行,单事务
  (`restore_account`);worker sync 的配置翻转路径同步清运行态 + 前进世代 +
  入队清零行兜底(审查阻断 #4)。
- **provider 作用域**:只对 kiro 启用(「0/35 不可恢复」的实测只覆盖 Kiro 号);
  dario 等 provider 经 `suspend_policy_scoped` 闸保持旧的平冷却、不退役,
  构造与 30s 热更两条路径同一口径(审查阻断 #6)。

### 排队号不再因空响应下线

空响应达阈值冷却保留给非排队号;排队号(企业共享池)的空响应多是跨租户拥挤的
上游抖动,下线只会缩小健康池、让 restock 把假冷却当缺口一直买号(与 429 同哲学:
唯一 hold 下线信号是 403 suspend)。真毒报文由 poison_memo 多账号确认机制兜底
(EmptyResponse 本来就不换号,不扩大喷洒)。

### 面板

新状态标签:封禁冷却(带剩余秒)、已退役(红)、复活观察期(橙)。
顺带修了「运行态封禁冷却被显示成已停用」的歧义(原 deriveAccountStatus 把
`temporarily_suspended` 归并进"已停用")。

### 配置

`system.yaml` / 设置面板新增:`suspended_backoff_cap_secs`(默认 86400)、
`suspend_retire_strikes`(默认 3,0=不退役恢复旧行为)。均支持热更。

## [cursor-review-fixes-2] - 2026-08-07

把审查里剩下的 12 条做掉。全量 **1148 项测试通过**。

### ⭐ 所有 provider 的错误体都缺 `error.type`(不只 cursor)

用户实测撞到:opencode 报 `Type validation failed … path:["error","type"] … expected string,
received undefined`,**真正的 message 被吞掉**。根因是除 `Overloaded` 外,
`upstream_error_response` / `sse_error_payload` 都只发 `{"type":"error","error":{"message":…}}`,
而 Anthropic 协议里 `error.type` 是**必填**;按 schema 严格校验的客户端连错误都解析不了。
**这条影响生产的 kiro / dario,不只 cursor。**

新增 `error_type_for_status`:type 跟着状态码走(400→`invalid_request_error`、
401/403/404/413/429→各自的、503/529→`overloaded_error`、其余 5xx→`api_error`),
两条对外路径共用。`sanitize_upstream_error_payload` 在上游没给 type 时也补 `api_error`。

⚠️ **两条既有测试断言的正是这个 bug**(「非过载错误不带 error.type」「不该凭空造 type」)。
原来的顾虑「造错 type 会误导客户端重试」合理,但结论反了 —— 根本解析不了比 type 不精确糟得多。
已改断言并在测试里写清为什么。选 `api_error` 是保守解:不指认客户的 key/请求有问题。

### 同一条原则只执行了一半:工具帧吞掉同帧正文

`is_tool_call` 的判断原先排在正文发射**之前**,而 usage 帧那里特意写了「用量必须最后判」
的防御。protobuf 层不阻止上游把最后一个正文增量和工具调用并进同一帧,所以工具帧那条路上
那一帧的正文会被静默丢掉,客户收到一个看起来正常的 `tool_use`。已把工具判断移到发射之后。

另外:内建工具收口发生在**已出字之后**时,仍按 `end_turn` 收尾(首包已 committed,报错也
换不了号),但这**确实是截断** —— 加了 `builtin_truncated` 标记 + warn,否则它在日志里
和一次正常收尾完全一样。

### 其余

- **空 `tool_use.id` 不再放行**:合成 `call_<uuid>` + warn(工具名解出来了说明模型真想调,
  丢掉比 id 难看更糟;我方从不把 call id 发回上游,合成是安全的)。
- **`expires_at` 回写**:不写它,gw-app 的 `has_fresh_token` 会把号当「永鲜」→ 从不主动刷 →
  每个过期号先吃一次 403,而 403 的分类本身就是雷区。
- **403 不再一律判 `TokenInvalid`**:只有 401 和**结构化** Connect 错误才动账号健康;
  裸 403(ALB/CF 拦 IP 的 HTML 错误页)判 `Other` —— 坏的是 IP 不是号。
- **`config_fetch_gate` 改按账号单飞** + 失败负缓存(60s)+ 缓存键带**身份指纹**。
  全局一把锁会把 N 个各有独立代理的冷号串成 N×6 秒,任一个卡住整池排队;
  失败不缓存则 api2 故障期每个请求各付一次完整超时。指纹进键 = 换凭据自动换条目。
- **`send()` 等响应头加 60s 超时**:配代理的号用的 client 故意不设总超时(流式),
  代价是「上游收了连接但永不回话」时永久占住并发租约,而 `STALL_TIMEOUT` 在流循环里进不去。
- **SSE `message_delta.usage` 补齐 `input_tokens` / `cache_read_input_tokens`**:
  此前客户在 SSE 里看到输入恒 0 却按非零输入被计费,拿响应对不上账单。
- **`ConvRegistry`**:`stateful` 改构造期读一次(不再每请求查环境变量);关闭时**完全不写表**
  (原先每次成功都插入 + 全表 retain,纯浪费还把所有流的收尾串在一把锁上);
  锁改毒化即恢复(原先 unwrap,毒化后每个请求都 panic = provider 整体下线)。
- **`config.rs` 时区透传**:原先硬编码 `Asia/Shanghai`,账号配别的时区就造出一个
  **内部自相矛盾**的指纹(unary 报上海、推理报美西),比配错更可疑。
  `client-type: ide` 保留(与推理的 `glass` 不同是抓包实物),但注释写清了。
- **traceparent 换全熵随机**:UUIDv4 的第 13/17 位被版本/变体位钉死,那是一个
  **每请求广播两次**的稳定特征。加 `getrandom` 依赖,并加测试抽 200 个 id 检查位分布。
- **落盘开关启动告警**:`CURSOR_DUMP_REQ` / `CURSOR_DUMP_TOOL_FRAMES` 会把含用户对话全文的
  帧明文写盘且不清理;构造时 warn 一条,免得排查完忘了摘。
- **思考被丢弃时留痕**:正文块已开时后续思考只能丢(Anthropic 不允许 thinking 排在 text 后),
  但原先是静默的 —— 现在计数 + debug 日志。
- **`validate_account` 对 cursor 生效**:此前 gw-app 只对 `claude-dario` 调它,
  cursor 的加载期校验是死代码。代理配错的号现在挡在池外 + 启动告警。
- **模型归一化留痕**:实际发上游的模型名与客户请求的不同时记一条 info
  (SSE 回显仍是客户请求的名字 —— 协议要求如此,客户端按它对账)。
- **缺价目不再静默算 0**:`price_for` 只认 opus/sonnet/haiku,而 cursor 广播 8 个 id。
  **不猜价**(定价是业务决策),但有 token 却算不出成本时 warn 一条。

### ⭐ 实测顺带确认两件事

**① Cursor 的 `1.14.1`(input)确实包含 `1.14.3`(cache_read)。**
实测:`input_tokens=26483` / `cache_read_input_tokens=26376`,差 107,而我方请求体只有 1429B。
差值量级吻合。这与 caio 全局口径(`input` 是总上下文、`cache_read` 是子集,计价方相减)
**一致**,不存在重复计费。此前审查把这条列为「作者相信为真但没证明」,现已证明。

**② Cursor 会注入约 26k token 的服务端系统提示,每个请求都算进 input。**
一句「27 乘 43」也报 26447 input。好消息是缓存命中率 99.6%,边际成本小;
但它是一个**每请求的固定地板**,且客户按它被计费 —— 客户拿自己的请求长度对不上账单。
sonnet 价目下约 $0.008/请求。**这是定价要考虑的输入,不是 bug。**

### 没能确认的一条

**thinking 透传这一轮没能复现。** 上游连续两次请求 `thinking_len=0`(一个 `1.4` 帧都没发),
我方的丢弃计数是 0 —— 所以**不是我方吞掉的**,是上游没发。上一轮实测它发过
(11–19 字的简短推理摘要)。判别方法:看日志的 `thinking_len=` —— 大于 0 却没到客户端才是我方 bug。


## [cursor-review-fixes] - 2026-08-07

对抗审查(codex Architect + kimi Skeptic,两个独立模型)+ 自审的产出。**结论是 REJECT**:
前三条高危两位审查员各自独立找到,不是风格问题。本条记修掉的部分。

### ⭐ 工具回路发散:两个独立病因

现象:grok 在 opencode 里对同一个文件连调 **9 次** `read`,请求体每轮只涨 1.2KB;
另一次是一串 `→Read … [limit=, offset=]`,参数值全空,模型反复自我纠正「参数改为数值类型」。

**病因一:数值参数解成空。** `1.2.2.15.1.2.2` 那层不是我方猜的包装,是标准
`google.protobuf.Value`(`null=1, number=2/double, string=3, bool=4, struct=5, list=6`)。
抓包实物只出现过 field 3,于是解析被写死成"只读字符串档";模型传 `limit: 200` 时值在
**number 档(fixed64 double)**,而 `protobuf::Reader` 那时把 fixed64 的字节**丢掉了**
(只记"有个 fixed64")。结果 `limit=""` → 工具失败 → 模型换个说法再试 → 永不收敛。
现在 Reader 带出 fixed64/fixed32 原字节,`ToolCall.args` 从 `Vec<(String, String)>` 改成
`Vec<(String, serde_json::Value)>`,数字/布尔/嵌套对象/数组全部原型透传;
认不出的档位解成 `null` 而不是空串(空串会被工具当成"给了个空值"而静默做错事)。

**病因二:折叠把问题弄丢了。** 工具回路第二轮,Anthropic 请求的最后一条 user 消息
**只有 tool_result**,`fold_history` 折完之后本轮用户消息是一段裸的 `[工具返回]…`,
**一个问题都没有** —— 原始请求被埋进 `<conversation_history>`。模型看到「工具返回了但
没人问我什么」,最合理的动作就是再调一次工具。现在识别出这种中间轮,把**用户最近一次
真实提问**复述回本轮并附一句「据此继续、不要重复调用已返回结果的工具」。

顺手:`tool_use.id` 去掉换行 —— 上游 call id 是 `call-<uuid>-N\nfc_<uuid>_N` 两段用换行连的,
原样交给客户端会让 id 校验失败,还把日志劈成两半(排查时看着像日志损坏)。

### ⭐ 一次 api2 超时能把健康号永久禁用(自己上一版改出来的)

链条:`GetServerConfig` 失败 → 上一版加的退化「过期旧值 > 空串」→ 空 `config_version`
→ `config.rs` 自己写明的完整性门回 `resource_exhausted` → `trailer_to_error` 判
`QuotaExhausted` → scheduler 的处置是 `disabled_until = None`,**持久、不自愈、要人工 reset**,
连排队等待都不给。api2 连续抖动就是整池挨个阵亡。

那段注释写的是「取不到不该打挂聊天」—— 意图对,后果比它替换掉的 `?` 坏得多。
现在:取不到就返回**可重试错误**(`Run` 还没发出,不算 committed,gw-app 会换号);
过期旧值也不再兜底(同一条软封路径)。另外把「Update Required」签名从 `QuotaExhausted`
里摘出来判 `ServerError` —— 完整性门与真的额度耗尽**共用同一个 code**,而错判方向是
不对称的:把额度耗尽当完整性门只多试一次,反过来是账号死掉。

### 出口代理构造失败改 fail-closed

原先 warn 一声回退默认出口。理由「配置写错不该让号彻底不可用」是反的:账号配了 `proxy`
就是在声明要独占出口,回退等于**把本该隔离的号并到同一 IP**,而这是已实测的封号维度
(同出口 59.5% vs 独立代理 0%)。gw-dario 早先的对抗审查就同一问题已定案 fail-closed,
同一个安全边界不该因 provider 不同换结论。现在拒绝 + `validate_account` 加载期就报出来。

### 会话亲和键(`affinity_key`)

不覆盖它的后果不是「少一层优化」:worker 是 `affinity_key.unwrap_or_default()` 装
`CallCtx.session_id`/`cache_key`,默认 `None` → 两个都是空串 → `conversation_id` 恒为 `""`。
于是上游 `1.5` 发零长度字符串(真客户端是 UUID)、`x-blob-encryption-key`/`x-fs-client-key`
从「每会话一把」退化成「每账号一个常量」——两条都是稳定的可区分指纹;`ConvRegistry`
所有会话挤在同一个键上,`CURSOR_STATEFUL=1` 下是**跨用户串话**。

现在按首条用户消息 + system 派生稳定键(与 gw-kiro 同思路),并在 provider 内过一遍
`conversation_uuid`:保证非空、保证 UUID 形态、把调度用的分组前缀折进哈希(那是 caio
内部命名空间,不该进上游报文)。

### 四道体积闸

`gunzip` 单帧 64MB、帧声明长度 64MB、单附件 12MB、附件合计 24MB、PDF 抽取总量 4MB +
流数 512。此前 `pdf::inflate` 的 16MB 只是**单流**上限:100 个合法流累计 1.6GB,
而这段文本随后还要进 prompt、protobuf、gzip,峰值是它的数倍,且全在首个 await 之前
同步做完 —— **爆炸半径是整个 worker 进程,连带同进程的 kiro 流量一起死**。

### 测试:改掉「测自己镜像」的那几条

kimi 点名:tool_call 相关用例用与解析器**同一套常量**造帧再解,字段号整套写错也照样绿。
现在把 2026-08-07 抓到的两帧实物(外部工具 / 内建工具)存成
`tests/fixtures/*.bin`,`include_bytes!` 直接解;`google.protobuf.Value` 的嵌套
struct/list 用**手写字节**断言。helper 保留但注释写明它只能证明编码解码互逆。

全量 **1143 项测试通过**。

### 文档订正

`PROTOCOL §12.5` 的嫌疑清单里第 2(不发 `1.25`)、第 3(第二轮仍发首轮形态)**都已经做掉了,
做完仍然挂起** —— 已标为排除,只剩 FileSync 一条。旧清单会让下一个人重做已完成的工作
并误以为 `1.25` 还没验。另注明:此前该实验走 `examples/e2e.rs`(传真实 `session_id`),
而生产路径当时 `conversation_id` 恒空,重跑前先对齐。

### 审查里**尚未**处理的(按分级留档)

`is_tool_call` 在正文判断之前 break(同帧正文被静默丢,而 usage 帧特意防了这一手 ——
同一条原则只执行了一半)、`parse_tool_call` 允许空 `id` 放行、`refresh_auth` 不回写
`expires_at`(`has_fresh_token` 对缺失字段视为永鲜 → 从不主动刷 → 每次先吃 403)、
403 一律判 `TokenInvalid`(出口 IP 被拦时坏的是 IP 却禁了号)、`config_fetch_gate`
是 provider 全局锁而缓存按账号分片(N 个冷号串行,单账号卡住阻塞全池)、
`send()` 等响应头无超时(配代理的号无界,会永久占住并发租约)、
SSE `message_start.usage.input_tokens` 恒 0(客户看到输入 0 却按非零输入计费)、
模型归一化在选号之后且不回写身份(执行一个模型/回显另一个/计费第三套命名空间)、
`ConvRegistry` 三处 `lock().unwrap()` 毒化即 provider 下线、
`config.rs` 硬编码 `Asia/Shanghai` 与 `client-type: ide`(账号配别的时区就造出一个
**内部自相矛盾**的指纹,比配错更可疑)、`pricing::price_for` 只认 opus/sonnet/haiku
而 cursor 广播 8 个 id(其中 6 个算出来成本为 0)。


## [cursor-thinking-and-guard] - 2026-08-07

### thinking 透传

`1.4.1` → Anthropic thinking 块,**只在调用方请求了 thinking 时发**。块索引改成顺序分配
(thinking / text / tool_use 都可能缺席,而 Anthropic 要求索引连续不重复)。

### ⭐ 内建工具护栏:「卡住」的真正原因

Cursor 的内建工具(终端/读写文件/网页搜索)是**服务端自带**的 —— 哪怕我们一个工具都不
声明,模型照样会调。实测:问「帮我查今天的新闻」→ 模型说「先确认今天日期,再查最新新闻」
→ 去调 `date '+%Y-%m-%d'` → 我方只能收口。**用户看到的就是一句没头没尾的计划,像卡死了。**

修法是往系统提示追加运行环境约束(有工具/无工具两版)。同一个问题现在变成一段完整回答
(说明无法联网 + 三条替代做法),内建工具收口 0 次。

### Features

- thinking 块(`thinking_delta`),按 `body.thinking.type == "enabled"` 开关。
- `message_start` 抽成函数:思考先到 / 正文先到 / 纯工具调用三条路径都要发它。
- `builtin_tool_guard()`:系统提示层的内建工具护栏。

### Design Rationale

- **没请求 thinking 就不发 thinking 块。** 客户端要么报未知块类型、要么把推理当正文显示。
- **thinking 必须排在 text 之前,且 text 开过就不再回头开 thinking** —— 否则块顺序倒置。
- **护栏放系统提示而不是别处**:它能改行为是实证过的(PINEAPPLE 实验)。

### Notes & Caveats

- 护栏是**缓解不是根治**:提示层的东西,模型仍可能偶发绕过。根治要么代执行内建工具
  (安全上不可接受 —— 等于跑模型选定的 shell 命令),要么找到关掉服务端内建工具的开关
  (`1.2.1.2` 的 `.32..50` 布尔位是候选,未逐位试过)。
- thinking 块**不带 signature**。Anthropic 原生 thinking 块回传时要签名;
  只做展示不回传的客户端没问题,要把 thinking 塞回历史的客户端可能拒。

## [cursor-media] - 2026-08-07

### 图片与 PDF 打通,两者都是内联的

`PROTOCOL` §5 猜的「附件走 FileSync blob 上传」**是错的** —— 带图/带 PDF 的真包里
一次 `FSSyncFile` 都没有,字节就在请求里。

- **图片** → `1.2.1.1.3`(此前记成「恒为空字符串」的那个字段,其实是附件容器)。
  ⚠️ 里面还有一层 `.1` 包装,少了服务端回 `internal`(结构错的信号)。
- **PDF** → `1.2.1.2.20` = `{1: 路径, 2: 内容}`,而**路径是连接键**:必须同时写进
  用户文本和 ProseMirror 的 `mentionNode`,否则模型不知道有附件。

### ⭐ 但服务端根本不读 PDF 内容 —— 这件事决定了实现

真客户端的流程是:模型**调终端工具跑 `pdftotext "<路径>"`**,由客户端在真实磁盘上
执行再回传。**反代不能走这条路** —— 答应内建终端工具调用等于在我们的服务器上执行
模型选定的任意 shell 命令。

所以 gw-cursor 自己抽文本层(新 `pdf.rs`),以 `<document path="…">` 塞进 prompt。

### Features

- `run::ImageAttachment` / `run::DocAttachment` / `run::Media`,Anthropic 的
  `image` / `document` 块 → 对应字段。
- `run::image_dims`:只读 PNG/JPEG 头取宽高(**不解像素**,所以没有解压炸弹面)。
- `pdf.rs`:FlateDecode + `Tj`/`TJ`/`'`/`"` 串抽取,带 **16MB 解压上限**。
- ProseMirror 的 `mentionNode`,以及路径写进用户文本。

### Design Rationale

- **`source.type != "base64"`(如 `url`)一律跳过。** 替调用方下载 URL 是我方主动出网,
  不该悄悄做。
- **抽不到文本时明确告诉模型「无法抽取,请直接告知用户」。** 不说的话它会反复尝试
  调工具读文件,而我们每次只能收口 —— 用户看到的是反复的半句话。
- **宽高解不出来就填 0,不猜。** 猜错的尺寸会让上游的图像分块算错。
- 空附件容器与图片**不能同时发**:`1.2.1.1.3` 出现两次时读侧取到的是空的那个。

### Notes & Caveats

- PDF 抽取**不支持**:扫描件/图片型(需 OCR)、非 Flate 压缩(LZW/DCT)、
  CID 自定义编码字体(可能乱码)、多栏阅读顺序(会串行)。
- 图片走原始字节内联,大图会显著撑大请求体;尚未接 `gw-kiro/src/image.rs` 那套
  四档压缩 + 解压炸弹护栏(那是跨 crate 抽取的活,见统一转换模块规划)。
- 实测:64×64 红/蓝图 → 「红色、蓝色」;探针 PDF → 哨兵串;0.7MB 真论文 → 标题正确。

## [cursor-tool-use] - 2026-08-07

### 工具调用打通,opencode 的 agent 能用了

此前 agent 类任务会在模型想动手时被截断。现在完整回路:声明 → 调用 → 结果 → 续答。

**工具身份是 protobuf 字段号,不是名字。** Cursor 内建工具是闭集枚举
(`1.2.2.1` 终端 / `1.2.2.4` 读文件,不带名字),装不下任意 Anthropic 工具名。
所以调用方的工具必须声明成 "MCP 工具"(`1.2.1.2.7`,5 个字段全必填,命名空间用
`gwtools`),它们的调用才会回到带名字的 `1.2.2.15`。

### Features

- 请求侧:Anthropic `tools` → `1.2.1.2.7`(`run::ToolDef`)。
- 响应侧:`1.2.2.15` → Anthropic `tool_use` 块 + `stop_reason: "tool_use"`(`run::parse_tool_call`)。
- 回路:`tool_use` / `tool_result` 块渲染成文字进折叠历史,模型据此续答。
- 内建工具调用**认得出但不转发** —— 转发会给调用方一个它没有的工具名。

### Design Rationale

- **不用请求侧 `field 2` 中途回传。** 真 IDE 在同一条流里回工具结果,那要求把流挂着
  等调用方执行完;反代是一问一答的,把结果当成下一轮的上下文更贴合。代价是模型看到的是
  "工具返回了这些文字"而不是结构化 tool_result,实测不影响它续答。
- **call_id 原样带回。** 里面嵌着 `\n`(`call-<uuid>-N\nfc_<uuid>_N`),重新生成就对不上;
  JSON 转义后在线上完全合法。
- **内建工具只收口不转发。** 认出来是为了不陪它等满 watchdog;不转发是因为调用方
  没有 `run_terminal_cmd` 这种工具,给了它只会报未知工具名。

### Notes & Caveats

- 参数值目前只支持**字符串**(上游 `1.2.2.15.1.2.2` 只见过 `3`=string 这一个分支)。
  嵌套对象/数组参数的编码分支未知,复杂 schema 的工具可能失真。
- image 块仍不支持(要先摸清 Cursor 侧的图像字段)。
- 实测:`get_weather` 单工具闭环;opencode agent(`ls` + `Read`)16s 完成。

## [cursor-stateful-turns] - 2026-08-07

### 首轮与后续轮是**两种形态**,混用会让服务端无视内联历史

同一会话连抓两轮真包(98858B vs 2121B,相差 47 倍)后才看清:

| | 首轮 | 后续轮 |
|---|---|---|
| `1.1` 环境/预算块 | **空** | 612B(预算表报账) |
| `1.2.1.2` 内联上下文 | **97895B 全量** | 不发 |
| `1.2.17.9` 环境详情 | 无 | 有 |
| 历史 | — | 服务端按 `1.5` 自持 |

`1.5` 是 conversation_id(两轮相同),`1.25` 是轮次 id(每轮变)。

**多轮怎么通的(别记错因果)**:上游**不认** repeated `1.2.1` —— 请求里只要多于一条,
就是与缺 `1.2.1.2` 一模一样的静默挂起。所以历史靠 `fold_history()` 渲染成
`<conversation_history>` 文本塞进当前这一轮。代价是上游看到的是"一条很长的用户消息"
而不是结构化对话,但模型确实读得到(实测多轮答对)。
两形态(首轮 `1.1` 空 / 后续轮带预算表)是另一件事,它修的是"服务端以为历史在它那边"。

### Features

- `run::Phase{Opening,Continuation}`:两形态分别构造;`ConvRegistry` 记「这个会话在这个号上
  已经开过场」,匹配不上(新会话 / 换了号)自动降级回首轮全量形态。
- 工具调用识别:上游发起工具调用(顶层 `field 2` + `1.8.1=17`)时**主动收口**,
  不再陪它等到 watchdog。90s 挂起 → 6s 返回。
- `GetServerConfig` 加 single-flight + 失败不阻断 + TTL 120s→30min。

### Design Rationale

- **工具调用必须主动收口。** `tool_use` 没实现之前,陪上游等的代价是每个「需要动手」的
  请求都挂满 90 秒。有字就交出去、没字就明确报错,都比静默挂着好;报文前缀打进日志,
  攒的是下一步映射 `tool_use` 的规格(已看到 `call-<uuid>` / `fc_<uuid>` / 参数 `ls -la`)。
- **`GetServerConfig` 失败不该打挂聊天。** 它只是个握手下发的版本号,不是凭据,
  而 `api2.cursor.sh` 实测会偶发超时+单次要 5–6 秒。原先一个 `?` 让抖动变成用户可见失败。
- single-flight 用 async 锁:必须跨 await 持有才挡得住并发,std 锁跨 await 会让 future `!Send`。

### Notes & Caveats

- **`tool_use` 仍未实现**,所以 agent 类任务会在模型想动手时被截断(返回半句)。
  这是当前唯一的功能缺口,规格已备。
- 本地测试配置:`config/{accounts,instances,system}.yaml` + opencode 的 `cursor-local` provider。
  账号 `max_concurrency: 6`(opencode 一轮并发发 4 个,默认 2 会撞「并发已满」)。
  ⚠️ 真 IDE 不会同时发这么多,并发数本身是一处可被服务端区分的特征。

## [cursor-run-unblocked] - 2026-08-07

### 起因：请求被接受，却永远不生成

`gw-cursor` 合成的 `agent.v1.AgentService/Run` 能让上游回 **200**、回一帧会话回显
（证明它解析并接受了我们的会话），然后每 **10 秒**发一个 4 字节心跳，**永不产出文本**。
没有错误码、没有超时、没有 trailer —— 只看返回值查不出任何东西。

此前的排查方向全是**加法**（补开场帧、逐字照抄 19 个字段、换模型、half-close 与否），
全部无效。这次换成**减法**：拿当天新抓的一份真请求（27 帧 / 28KB），一刀一刀往下削，
看哪一刀让「出字」变成「只心跳」。

### 结论：请求与响应各错一处，都不报错

**① 请求：`1.2.1.2` 必须在场，哪怕长度为 0。**

| 请求 | 大小 | 结果 |
|---|---|---|
| 会话块含 `1.2.1.2`（空） | **446B** | ✅ 正常生成 |
| 同上，去掉 `1.2.1.2` | **445B** | ❌ 200 + 会话回显 + 永远心跳 |

两个请求**差一个字节**。服务端拿这个字段判定「本轮上下文已声明完毕」，缺了它就一直等。
内容可以完全为空 —— **它是握手信号，不是数据**。

顺带纠正：环境/系统提示那一块的路径是 `1.2.1.2`，不是文档记的 `1.2.17.9`（字段号是对的，
挂错了地方）。`1.2.17` 里只放 blob 哈希，**绝不能发** —— 一发服务端就会去取我们从没通过
FileSyncService 上传过的 blob，直接 `invalid_argument: Failed to resolve request context blobs`。

**② 响应：正文在 `1.1.1`，`1.4.1` 是思考。**

两者结构完全相同（`{1: 文本, 2: 1}`），只差字段号。解错了同样不报错：请求成功、有字出来，
但出来的是模型的推理过程，真实回答一个字都收不到。实测「只回答 PINEAPPLE」那次，
`1.4.1` 是「用户要求…」，`1.1.1` 才是 `P` / `INE` / `APPLE`。

### Features

- 帧0 会话块补上 `1.2.1.2`，挂在**最后一轮**（与真客户端一致，也避免几十 KB 重复）。
- `parse_frame()` 取代 `parse_text_delta()`，正文 / 思考 / 用量分开。
- 认 `1.14` 用量帧收口，并用上游自报的 token 数取代 `chars/4` 粗估。
- 系统提示透传实测有效（`1.2.1.2.25`）；不发时服务端会自己合成一份。

### Design Rationale

- **`1.2.1.2` 不受 `RunShape.context_block` 控制。** 那个开关是给二分实验用的，
  管的是「里面装什么」；字段本身是出不出字的开关，被开关关掉过一次就够了。
- **`1.14` 必须当收尾信号。** BiDi 流不会自己关，上游发完用量就只剩心跳。
  不认它的话每个请求都要挂到客户端超时 —— 表现是「答完了却一直转圈」，
  而在反代里这等于每个请求占着一个连接直到超时。
- **思考绝不能混进正文。** 拼进回答里客户会看到「用户要求我…」这种自言自语。
  目前是丢弃（`RespFrame.thinking` 已解出待用），透传成 Anthropic thinking 块是后续。
- 减法实验的每一步都留成了回归测试：这类 bug 的共同特征是**不报错**，
  只有把「当时的对照实验」钉成断言，重构时才会当场变红。

### Notes & Caveats

- 端到端实测出字并正常 `end_turn`：`grok-4.5` / `composer-2.5` / `default` 三个模型。
  `claude-*` / `gpt-*` 仍是计费额度问题（`ERROR_RATE_LIMITED_CHANGEABLE`），与协议无关。
- 仍未做：tool_use、thinking 透传、图像、文件附件（L2 FileSync）。
- 抓包手法与完整字段表见 `crates/gw-cursor/PROTOCOL-agent-run.md` §11。

## [restock-hunt] - 2026-08-06

### 起因:速刷号变稀缺,货一上架就被别人买走

补货的常规轮询是 **30 秒一轮**。这个间隔在「随时有货」的时代是对的 —— 那时
`min_healthy` 一破,下一轮就能买到。号变稀缺之后同一个间隔变成了纯粹的落后:
上架到售罄可能只有几秒,而我们平均要等 15 秒才去看第一眼。

近 7 天有 **854 轮**「水位已破、闸门全过、就差有货」,其中 2026-08-04 一天连断
**4.7 小时** —— 那期间流量全落到贵号池(¥0.068/积分 vs 自购号中位 ¥0.021)。

### 「缺货」与「不该买」是两种状态,只有前者值得提速

这是本次改动的全部判断依据。原先两者在代码里都只是一句 `reason` 字符串,
所以先给 `Decision` 加了结构化的 `out_of_stock`(**闸门全过、就差有货**),
而不是拿人读的那句话去匹配 —— 改一个字就会让提速悄悄失效,
而失效的表现是「一切正常,只是又没抢到」。

三种情况严格分开:

| 情况 | 处置 | 为什么 |
|---|---|---|
| 一件货都没有 | **提速到 5s** | 每多等一秒都是纯损失 |
| 有货但过不了闸门 | 不提速 | 闸门不会因为多问几次就放行 |
| 询价全部失败 | **不提速**(退回 30s) | 最可能的原因恰恰是对方在限流我们 |

第三条是**自动退避**:对方开始 429 时,抢货模式自己就退出了,不需要另写一套限流处理。

### Features

- **抢货模式**:缺货时轮询间隔切到 `hunt_interval_secs`(默认 **5s**),有货、
  闸门变化或询价失败即退回 `poll_interval_secs`。`hunt_max_secs` 可设时长上限,
  **默认 0 = 一直抢**(断供能连着 4.7 小时,那时最不该歇)。
- **事件回调(webhook)**:`notify_url` 非空时,「连续缺货超过 `notify_after_secs`」
  「抢到号了」「熔断」三类事件回调它。按域名自动适配企业微信 / 钉钉 / 飞书 / Slack,
  其余发通用 JSON(`text` + 结构化字段平铺)。逐事件节流 `notify_min_gap_secs`。
- **`POST /admin/api/restock/wake`**:外部到货监控的回调入口。与 `buy-now` 的唯一
  区别是 **`force = false`** —— 闸门全部照常。
- 面板:抢货实况(已抢多久 / 探测几次 / 是否已通知)进 `GET /restock/state` 的
  `hunt` 字段,页面顶部显示蓝色横幅与「抢货中」徽标。

### Design Rationale

- **抢货期间不写决策流水。** 5 秒一轮 × 4.7 小时 = 三千多条一模一样的
  「所有货源都没有库存」。那不只是占地方,它会把决策流水**这个工具本身**毁掉。
  改成进入/退出各记一条,带次数与时长 —— 两条比三千条更能回答「刚才断了多久」。
- **正因为不写流水,才必须有 `hunt` 实况字段。** 否则面板会连着几小时一动不动,
  与「真的卡住了」无法区分。该字段带心跳,进程被 SIGKILL 留下的残留会被判成
  `null` ——「永远显示正在努力」是最难察觉的一种错。
- **租约 TTL 一律按常规轮询间隔算,绝不用抢货间隔。** 否则 TTL 会缩到 15 秒,
  而退出抢货后要睡 30 秒 —— 租约在睡眠中途过期,另一个 router 顺势接管,
  两边轮流当 leader。提速反而会把互斥搞坏。
- **通知按域名自动认方言,不给下拉框。** 选错的后果是**静默不通知**
  (企业微信收到飞书报文回 200 + `errcode: 93000`),而这个功能的全部意义
  就是「出事时人能收到」。同理:HTTP 200 也要查 `errcode`,只看状态码会让
  「配错地址」表现成「一切正常但从来收不到」。
- **每轮抢货只叫一次人。** 断供期间每 30 分钟响一次的下场是被静音,
  而静音之后真正要紧的熔断通知也一起收不到了。
- **`wake` 不 force。** 一个盯着上架页面的脚本不知道我方的日预算、需求速率、
  熔断状态,让它 force 等于把花钱的判断交给外部。
- 抢货状态机抽成纯逻辑(`restock/hunt.rs`,12 个测试):它的每个分支都要
  几十分钟到几小时才在生产上表现出来,「抢了一整晚但从没通知」和
  「每 5 秒发一条通知」都是真实可能的写法,而两者都要等下一次断供才看得见。

### Notes & Caveats

- **两家货源都没有到货通知能力。** 2026-08-06 核过 drop 前端 bundle:
  API-Key 那套接口只有查询与下单,`/api/v1/*` 只认会话 cookie(会过期,
  不适合无人值守)。所以「对方通知我」这条路走不通,`wake` 是留给
  「别人替我盯着」的。
- **请求密度是 6 倍。** 5s × 2 家 ≈ 每家 12 次/分钟。对方若开始限流,
  询价会失败 → 自动退回 30s;但若表现为静默降速而非报错,需要人工把
  `hunt_interval_secs` 调大。这是本次改动唯一需要盯的指标。
  实际生效间隔取 `min(hunt_interval_secs, poll_interval_secs)` ——
  抢货永远不该比常规轮询更慢,顺带这也是关闭提速的方式(调到 ≥ 轮询间隔)。
- `notify_url` 会在 `GET /restock/params` 里原样回显(机器人 key 在里面)。
  与供应商密钥不同,它必须可见才能编辑,而该端点本就在 admin 鉴权之后。
  错误信息里的 URL 已被剥除,不会进 `/logs`。
- 参数全部走 `restock_params` KV,**没有动 `SystemSettings`**,回滚地板不受影响。

## [settings-effective-echo] - 2026-08-06

### 起因:「我保存了设置很多时候不生效」查不下去

运行参数存 DB 的 `settings` 表,worker 每 30s 轮询后热应用。链路本身是好的
(实测 `cache_floor_ratio` 0.6→0.9 在 **11:51→11:52** 生效,worker 连续运行 18.4 小时
`restarts=0`,全期 0 次解析失败)。真正的问题是**面板上只有应然值,没有实然值**:

- 面板显示的是「库里的 overlay 叠 YAML 基线」算出来的**应该是什么**
- 决定计费与调度的是各 worker 自己轮询、应用之后的**实际是什么**

于是这两件事完全无法区分:①真的没生效;②生效了,但在 30s 轮询窗口内看的、
或看的是保存前的历史日志行(用户这次遇到的正是②——截图是 11:46,生效于 11:52)。

更糟的是链路里有两处**静默**失败,长得和「保存成功」一模一样:

1. overlay 解析失败 → `continue` → 每 30s 跳过一轮、保持旧配置,只有一行 error 日志
2. 本版本不认识的字段 → 被无声忽略(该 worker 镜像比写库的那个旧),表现为
   「别的设置都生效,就这一个不生效」

### Features

- worker 新增 `SettingsSync`(最近一次**成功应用**的时刻 / 错误 / 未知字段 /
  应用后真正在用的值),启动期与 30s 轮询两处写入,`/health` 回显 `settings` 段。
- admin `GET /settings` 扇出问各 worker,把实然值挂在同一份响应的 `workers` 字段。
  **`PUT` 不带** —— 刚写完库时 worker 还没轮询到,回一份必然过时的值只会误导。
- 面板在**改设置的同一页**新增一张卡:逐 worker 显示「多久前同步」「同步是否报错」
  「未知字段」以及**与应然值对不上的字段**(全对时不列)。

### Design Rationale

- **成功才刷新时间戳**:失败时 `applied_at` 停住不动,「现在 − applied_at」就是
  配置僵了多久 —— 这是静默失效唯一的外部特征,必须让它可读。
- **比对 worker 回报的全部字段**,而不是一份手写清单:手写清单漏掉的字段会得出
  「一致」这个错误结论,而漏掉的恰恰是没人想到的那个。
- **新鲜度阈值取 90s**(轮询周期的 3 倍):每轮还要先跑账号同步并抢锁,一轮超 30s 很常见。
  宁可迟报也不能误报 —— 天天变红的健康 worker 会让整张卡失去可信度。

### 对抗审查(kimi)修掉的问题

- **[high] 旧镜像 worker 会被显示成绿色「一致」**,正好是本功能唯一想抓的场景:
  它的 `/health` 没有 `settings` 字段 → 前端拿到 null → 走「无差异」分支渲染绿勾。
  改为服务端单独标 `stale_image`,前端红色单列。
- **非 2xx 响应也可能带可解析的 JSON 错误体**,当成正常回包会把坏掉的 worker 显示成
  健康。加 `status().is_success()` 校验。
- **文案错误**:原写「重启才会恢复」,实际库里 JSON 恢复可解析后下一轮就自愈。
- **`inject_workers` 重建响应体**(序列化→解析→再序列化)有 Content-Length 与
  「body 非 object 就 panic」的边角。改成 `respond_with_workers` 多传一个参数。
- **时钟回拨**会让 `age_secs` 变负并被渲染成「从未成功同步」;区分 `applied_at==0`
  与时钟异常。
- React key 由 `instance` 改为 `group#instance`(instance 配重时会合并渲染)。

判定为误报、已核实的三条:①`unknown` 是 `BTreeMap`,顺序稳定,告警去重不会失效;
②admin 的 http 客户端有 **2s 全局超时**,一个挂住的 worker 拖不垮 `GET /settings`;
③保存后走 `invalidateQueries` 重新 GET,卡片不会消失。

### 第二轮对抗审查(kimi 三视角)修掉的问题

三个视角**独立指向同一组**核心问题,说明不是风格分歧:

- **[medium×3] 回显的是「打算应用的值」而非「真正在用的值」**。两处会让面板报假「一致」:
  ① `if let Ok(sv) = to_value(&full)` 序列化失败时 provider 被跳过,而 scheduler 与
  cache_sim 照常更新、快照照记成功;②**`claude-dario` 的 `apply_hot_settings` 是 no-op**
  —— 它的 provider 级设置(缓存计费)改了永远不生效,但 worker 算得出「有效配置」照样报,
  面板会渲染绿色「一致」,把原本要抓的 bug 原样重演。
  修:`Provider` 新增 `hot_settings_supported()`(默认 `false`,与 `apply_hot_settings`
  的默认 no-op 同进退,只覆盖其一就是撒谎),KiroProvider 覆盖为 `true`;序列化失败写进 `error`。
- **[medium×3] 手写 9 字段清单本身就是新的盲区**,而卡片注释还在批评这个模式。
  改用 `SystemSettings::from_effective` 全量回显(30 项),同时删掉前后端两份手工清单。
  `default_proxy` 传 `None` —— 代理 URL 可能带 `user:pass@`,而这份数据要上浏览器。
- **[medium×3] 扇出复用 `/health` 把设置页和上游耦合了**:该端点会对配额缓存陈旧的账号
  触发 `getUsageLimits`,于是「有人打开设置页」= 「对付费账号打上游」。这个号池对封禁
  很敏感。新增轻量端点 `GET /settings-sync`(只读一把 RwLock),扇出改打它。
- **错误串脱敏**:serde 的类型错误会把出错的**值**嵌进消息,而那个值可能是
  `socks5://user:pass@host` —— 这条串要一路上到浏览器。入库前过 `redact_proxy_url`。
- **保存后的正常收敛窗不再报红**:每次 PUT 后 ≤30s 必有 drift,一律标红等于训练用户
  忽略红色。改成黄色「收敛中」,同步僵住才上红。
- **`SystemSettingsPatch` 排除 `workers`**:它是只读回显,留在可写类型里的话,任何把 GET
  数据摊开进 patch 的写法都会被写侧的未知字段保护用 400 挡掉**整次保存**。
- 「一致」文案在同步僵住 / 有未知字段 / provider 不热应用时一律不显示(信号自相矛盾)。
- `store=None`(库打不开)的降级 worker 现在会说明原因,而不是永久标红却毫无线索。
- 删掉 `warned_unknown` 这份与 `settings_sync.unknown` 重复的手工同步状态。

判定为误报、已核实的三条:①`unknown` 是 `BTreeMap`,顺序稳定,告警去重不会失效;
②admin 的 http 客户端有 **2s 全局超时**,挂住的 worker 拖不垮 `GET /settings`;
③保存后走 `invalidateQueries` 重新 GET,卡片不会消失。

### 上线时实测撞到的第四个问题

首次上线后核对生产数据发现:`caio-worker-dario` 跑着 3 天前的镜像,`/settings-sync`
返回 **404**,而原实现把「非 2xx」一律当成抓取失败 → 面板显示**离线**。但它 `/health`
是 200,进程活得好好的。「连不上」与「连得上但没这个路由」指向完全不同的运维动作
(去看容器 vs 重建镜像),报错方向反了会把人指到错的地方。

抓取结果改成三态 `Fetched::{Unreachable, NoData, Body}`:**答得出话就说明进程活着**,
只是镜像旧。上线后三种状态各自渲染正确:

| worker | 状态 |
|---|---|
| worker0 (G0) | 绿色,30 项全部一致,18 秒前同步 |
| worker1 (DARIO) | 在线 + **镜像过旧**(而不是「离线」) |
| worker2 (EXP) | 离线(确实没在跑) |

### Notes & Caveats

- 本次改动在 **worker** 里,所以 worker0 必须重建重启 —— 会打断在途请求。
  (第二轮修的是 admin 侧映射,只重启 router,不再打断 worker。)
- `caio-worker-dario` 仍在旧镜像上,面板会持续标它「镜像过旧」。这是**准确的**:
  它的 provider 级设置本来就不热生效。等它重建后会转成 `provider_hot: false` 的红字说明。

## [restock-supplier-priority] - 2026-08-05

### 起因:「两家的号价值等价」这个前提被证伪

多供应商上线时选家规则是**纯按单价升序**,写在代码注释里的理由是「实测两家的号
价值等价(都是 KIRO POWER、都按墙上时钟死)」。当天晚些时候的数据推翻了它:

| 货源 | 号数 | `temporarily_suspended`(Kiro 侧封号) |
|---|---|---|
| drop(us-east-1) | 29 | **0** |
| kiroapp/eu | 12 | **12** |

其中 `kiro-apikey-025ed93fd7df` 到手第一个请求就 502、**零次成功**即被封;同一分钟
买的 drop 号至今在正常服务。10 个 EU 号还在 17:47–17:49 两分钟内**同时**停摆,
是整批被封而不是各自寿终。两批号跑在同一台机器、同一出口 IP、同样的并发参数。

结论:便宜 48% 的货架买到的号会被封。**封禁率既观测不到、也编码不进单价**,
纯比价规则必然持续选中它。

### Features

- **货架档位 `priority`**(数值越小越优先,与账号优先级同向)。排序键从
  `(价格, id)` 变成 `(档位, 价格, id)`,即 `Shelf::sort_key` 与 `rank_shelves`。
- **细到货架的覆盖 `shelf_priority`**:出问题的是 `kiroapp/eu` 这**一个货架**,
  `kiroapp/us` 是同一家的另一批号。只能整家降级的话,要么误伤 US、要么放过 EU。
- **档位由引擎按名册回填**(`registry::shelf_priority_of` → `Engine::survey_all`),
  供应商适配器一律留 0:一家货源不该自己声明自己有多重要。
- **快照新增 `next_pick` / `next_pick_why`**:「下一单会买哪个」由 `choose_shelf`
  本人回答并写进快照,前端只渲染。
- 面板:每个货架显示**生效档位**,名册可编辑本家档位与逐货架覆盖。

### Design Rationale

- **档位是软优先,不是硬绑定**。首选档缺货/熔断/超上限/余额不足时自动落到下一档。
  drop 常年 0 库存(近 7 天 854 轮无货),硬绑定等于亲手制造多供应商要消除的断供 ——
  断供的代价(客户报障)高于买到一个可能被封的号。
- **档位是人写的,不自动加权**。「这家的号能不能用」目前只能靠人看封禁数据判断,
  自动加权会把一次抽样噪声放大成长期偏置。
- **缺省 0 = 全部同档 = 退回纯比价**。老名册没有这两个字段,`#[serde(default)]`
  解析成 0,与本次改动前同序;`SupplierCfg` 没挂 `deny_unknown_fields`,所以
  **旧镜像读新名册**也只是忽略未知字段。回滚两个方向都安全。

### 对抗审查(kimi,三视角)修掉的问题

- **PUT 缺省语义不对称会静默回滚整个改动**:`priority` 原本是 `#[serde(default)] i64`,
  而同结构里的 `api_key` 是「缺省保留原值」。部署瞬间浏览器里缓存的**旧版面板 JS**
  保存一次名册(哪怕只想改日上限)就会把所有档位归零 → 排序退回纯比价,只留一条
  warn 日志。改成 `Option<i64>` / `Option<BTreeMap>`,与 `api_key` 同语义。
- **面板自己算「下一单会买谁」会说谎**:排序能复刻,余额/单价上限/日上限/
  `unit_cost_veto` 这些闸门复刻不了。三个审查员独立指出同一条。改为后端给
  `next_pick`,前端删掉自己那套判断;展示排序补齐 `unit_price_cny > 0` 过滤与定序尾键。
- **读路径不去重导致余额张冠李戴**:`choose_shelf` 的余额表按 `supplier_id` 收集,
  名册里两条同 id(只能绕过 admin API 直接改库产生)会让后一家覆盖前一家的余额,
  于是**用 A 的余额批准 B 的购买**。`parse_roster` 加去重。
- **软优先只测了一半**:原先只测「缺货落档」,补上「有货但过不了花钱闸门」——
  生产上后者更常见(首选家余额见底 / 本家日上限到顶)。

判定为误报、已核实的两条:①「可能存在绕过 `survey_all` 的 `choose_shelf` 调用点」
—— 全仓只有一个生产调用点且 surveys 确实来自 `survey_all`;②「旧镜像读新名册会
解析失败」—— `SupplierCfg` 未挂 `deny_unknown_fields`。

### Notes & Caveats

- **代码合入 ≠ 行为改变**。现网名册没有档位字段 → 全 0 → 仍是纯比价。
  必须在部署后配置名册,否则首选依旧是 ¥15.43 的 kiroapp/eu。
- **降级的自我封闭**:eu 降档后几乎不再被买 → 不再产生封禁样本 → 没有机制发现
  它什么时候变好了。目前只能靠人定期手动试一单,没有过期提醒也没有探针单。
- 单价上限要容得下首选档:现网 `max_price_usd=6.0` × `rate_cap=7.2` = ¥43.2,
  drop ¥26.36 过得去。若哪天上限被按 eu 价位调低,首选档会每轮被价格闸门否掉、
  静默落回 eu,线索只在决策流水的 whys 里。

## [restock-multi-supplier] - 2026-08-05

### 起因

补货只认 drop 一家。近 7 天有 **854 轮**「水位已破、闸门全过、就差有货」——
2026-08-04 一天连断 4.7 小时,那期间流量全落到贵号池(¥0.068/积分 vs 自购号中位 ¥0.021)。
接第二家 `kiroapp.io` 的理由是**缺货冗余**,不是价差(两边贴身跟价,且 EU/US 各有涨落)。

面板侧的直接问题:库存/单价/余额三张卡显示的是「drop 一家」,多一家之后这三个数
不再有单一含义,而只显示一家的后果是 —— 那家充足、另一家钱包见底导致买不到号,
面板上却一切正常。

### Features

- **`Supplier` 契约**(`restock/supplier.rs`):各家把库存表达成一组 `Shelf`、把钱表达成 CNY,
  引擎只比 `unit_price_cny`。购买结局是四态 `BuyOutcome`(Ok / Conflict / Fault / Unknown)
  而不是 `Result` —— 「结果未知」是唯一会丢钱的分支,必须能被单独表达。
- **选家只有一条规则**:`engine::choose_shelf`,**能买的里面最便宜的那个**。纯函数,
  11 条单元测试钉死。所有花钱闸门(单价上限、单位成本、余额、全局日上限、单家日上限)
  在这里逐货架跑一遍,过不了就换下一个货架而不是整轮放弃。
- **kiroapp 适配器**:计价单位是 credits,走 `credits → USD → CNY` 三段折算;
  分 `us` / `eu` 两个货架(价格与库存独立);错误体是扁平 `{"error":"中文串"}`;
  竞争失败是 403/404 而非 drop 的 409。
- **每家独立熔断** `restock_breaker:{id}`:一家 key 失效不再连累另一家。
- **每家独立日上限** `daily_cap_cny`(0=不限):限制单家的敞口。
- **订单记货源**:`restock_orders` 增 `supplier`/`shelf`/`region` 三列(`ensure_column` 热升级)。
  在途订单靠 `supplier` 才知道该向谁对账。
- **面板**:新增「货源」卡 —— 逐家余额(含原生单位)、逐货架库存与单价、今日花费与上限进度、
  熔断/询价失败状态,按价格升序排列(**与引擎选家同序,第一行就是下一单会从哪买**);
  名册可在面板增删改,保存后**下一轮生效不用重启**。密钥只写不回显。

### Design Rationale

- **名册存 DB 而不是 `system.yaml`**:后者挂 `deny_unknown_fields`,加字段会让回滚到
  旧镜像变成启动失败。旧镜像读不到这个键就回落「只有 drop 一家」,与本次改动前等价。
- **金额一进引擎就必须是 CNY**,折算只许发生在适配器里。两家用同一个上限汇率,
  所以绝对值偏高(fail-closed)但**比价精确**(倍率对消)。
- **不做同轮 fallback**:一个货架竞争失败后本轮就结束,等下一轮(30s)重选。
  号活 ~50 分钟,30s 的代价可忽略,而少一条分支就少一处花钱的路径。
- **对账进花钱锁**:一轮对账可能跑过 leader 租约(TTL 90s),第二个 router 会对同一批
  pending 再重放一次。两个执行体同时重放一个幂等键,对方幂等只要有缝就是双扣。

### 对抗审查(gpt-5.6-terra × Skeptic/Architect)修掉的问题

1. **drop 对账把真实扣款记成 ¥0**(两个 lens 都判 high)。drop 不下发单笔扣款额,
   对账路径又拿不到买前余额 → `spent_cny = 0`,一笔真钱从日预算消失、当天可多买一单。
   改为 fail-closed 回落**下单时的限价**。
2. **单家日上限形同虚设**:原先只在「已花 ≥ 上限」时屏蔽,没把本单算进去。
   上限 ¥20、已花 ¥19、本单 ¥15 会照买到 ¥34。
3. **名册改动会让在途订单变孤儿**:删/停用/改名一家之后,它的 pending 单永远找不到
   客户端去对账。`PUT /restock/suppliers` 现在直接 409 拒绝这类改动。
4. **查单解析失败被当成「订单不存在」**:200 但响应截断/字段升级时,一笔**已扣款**的
   订单会被判死、释放预算、永不重试。改为 `Unknown`;且「没找到」只有在
   `total <= page_size`(确认看全了)时才算数,否则保持在途。
5. **对账重放 `replayed=false`(已发生二次扣款)只打日志**:现在置 `double_charged`
   并**熔断该货源** —— 幂等失效意味着「重放安全」这个前提没了,而整套对账建立在它上面。
6. **重放丢了 `region`**:kiroapp 不带 region 会回落 US,要么判成另一张订单(二次扣款),
   要么取回一批错区域的 key。`Supplier::reconcile` 现在接 `shelf`。
7. **连败计数只增不减**:一次 401、几百单成功、再一次 401 会被算成「连续 2 次」而熔断
   一家健康货源。购买成功即清零。
8. **实扣超限价 / 成交数 > 请求数 / key 数 < 付款数** 三种账实不符现在都会写进决策流水,
   超限价还会当场熔断该家。

### Notes & Caveats

- ⚠️ **kiroapp 没有任何服务端限价与数量校验**。`max_total_cny` 完全没实现;`count` 超过
  `max` 时它 **clamp 后成交而不是拒绝**(2026-08-05 实测发 `count:99` 被扣走 10 个号的钱,
  150 积分 ≈¥154)。所以这家的敞口上限只有三个:**钱包余额**、引擎的 `max_per_purchase`、
  以及日预算闸。询价与下单之间涨价的那一笔**拦不住**,只能事后熔断止损。
  想要硬上限,唯一可靠的办法是**把钱包余额本身控制在可接受的敞口内**。
- ⚠️ **回滚约束**:一旦存在 `supplier='kiroapp'` 的 pending 订单,**不能裸回滚** ——
  旧镜像的 `reconcile_pending` 会把所有 pending 一律交给 drop 重放,用同一个 order id
  向 drop 下一张全新订单。回滚前必须先把在途订单清空或人工收尾。
- 历史订单的 `supplier` 是空串,不归任何一家(那时还没有多供应商概念,归给谁都是编的),
  但仍进全局日花费;对账时按事实归给 drop。
- 未做:同轮 fallback、余额守恒告警(`days_left` 预警)、ksk_ 占比按小时物化。

## [restock-money-safety] - 2026-08-05

补货的**资金安全**修复。起因是多供应商方案的对抗审查(gpt-5.6-terra,
Skeptic / Architect / Minimalist 三视角)——三个视角独立收敛到同一批问题,
而且它们**都不是新设计引入的,是现在就在生产上跑着的**。多供应商会放大每一条,
所以先修地基。详见 `docs/MULTI_SUPPLIER_PLAN.md` §3。

### 起因(五条,全部经代码核实)

1. `POST /restock/buy-now` **完全不受互斥保护** —— 它不抢任何锁,直接 `run_once(true)`。
   手动点两次、或手动与后台轮询撞上,两个执行体会读到**同一个** `spent`,各自下单扣款。
2. leader 租约 TTL 最短 **30s**(`poll_interval` 下限 10s × 3),而单轮外层超时是 **120s**;
   花钱前不复验持有。第 31 秒另一个 router 可以接管并下单。
3. **网络超时一律标 `failed`**,而对账只扫 `pending` —— 于是「对方已扣款、响应丢了」
   会**永久失去对账机会**,钱和 key 双双成孤儿。
4. 日预算只统计 `purchased`/`imported`,**不统计 `pending`**。120s 超时把购买掐断后,
   下一轮把那单当成没花过钱,换个 `client_order_id` 再买一次 ——
   幂等键防得住「同一个 id 重放」,防不住「换个 id 再来」。
5. `reconciled` 一置 `true` **永不再对账**。只覆盖得到重启前的遗留单,
   运行期产生的在途单只要进程不重启就一直悬着。

### Features

- **新增「花钱锁」`restock_purchase_lock`,与 leader 租约分开**。两把锁回答不同问题:
  租约 = 「谁来跑轮询」(长期持有),花钱锁 = 「此刻谁在花钱」(只在临界区内持有)。
  `SqliteStore::{try_acquire_lock, release_lock, holds_lock}` 是通用实现,
  `try_acquire_restock_lease` 改为委托给它(那段棘手的条件 UPDATE 现在只有一份)。
- 临界区从**读预算**开始(不是从下单开始),由 RAII 守卫 `PurchaseGuard` 归还。
  **`buy-now` 与后台循环都必须过这道门。**
- `DropError::is_indeterminate()`:`status == 0`(网络失败)或 `5xx` = **结果未知**。
  这类订单**停在 `pending`** 等对账重放,不再判死、也不计熔断。
- `restock_spent_since` 把 `pending` 按 `max_total_cny`(限价 = 可能花掉的上限)计入花费。
- `restock_pending_orders(min_age_secs)` 加年龄门槛;对账改为**每轮都跑**。
- 花钱前复验 leader 租约(`holds_lease`),手动购买跳过。
- 面板新增 `in_flight_orders`(在途单),与 `orphan_orders`(孤儿单)分开数。

### Design Rationale

- **为什么花钱锁不能复用 leader 租约**:后台循环长期持有租约,让 buy-now 去抢会永远抢不到,
  变成一个点了没反应的按钮。反过来只有租约、没有花钱锁,buy-now 就完全裸奔。
- **为什么 `pending` 按限价而不是按实扣计入**:在途单的实扣**还不知道**。方向必须
  fail-closed —— 宁可高估当日花费少买一个号,也不能低估到超预算。对账落定后高估自动消失。
- **为什么对账要加年龄门槛(300s)**:改成每轮跑之后,「刚落库、请求正在飞」的订单也是
  `pending`。没有门槛的话,另一个 router 会把**别人正在途中的那一单**拿去重放 ——
  对方若还没记录这个 id,那就是第二次真实扣款。门槛必须大于单轮外层超时(120s)。
- **4xx 与 5xx 的分界**:4xx 是对方在处理前就拒绝了(确定没扣款),可以安全判死;
  5xx 和网络失败则可能发生在扣款之后。实测该站往返 3.4–4.2s 而超时是 20s ——
  真触发超时时,对方多半已经处理完了。

### Notes & Caveats

- ⚠️ **TTL 锁给不了 exactly-once**(没有 fencing token):持有者卡死超过 90s 时锁会过期,
  放第二个执行体进来。这里拿到的是两层保障 —— 常见情况被串行化,而**最坏情况的总花费**
  由日预算兜住(现在把在途单算进去了)。要做到严格一次需要 fencing,不在本期范围。
- 全量测试 **962 通过 0 失败**。三条新用例都做过回退验证:撤掉对应修复即变红
  (回退「花钱锁校验 holder」时连原有的租约用例也一起红,说明重构被现有用例覆盖住了)。

## [restock-region] - 2026-08-05

自动补货上号时,**把号的服务区(region)一路带到建号**。这是接第二家供应商
(`kiroapp.io`,欧洲区)的前置改动;对现有的 drop 货源**零行为差异**。

### 起因

Kiro 的号**绑死服务区**。2026-08-05 实测一个 `eu-central-1` 的 `ksk_` key:

```
management.eu-central-1.kiro.dev/Get-Usage-Limits  → 200  KIRO POWER
management.us-east-1.kiro.dev/Get-Usage-Limits     → 403  {"message":"Invalid token"}
```

而 `gw_kiro::usage_limits::DEFAULT_REGION` 就是 `us-east-1` —— 账号 `extra` 里读不到
`region` 就用它。

`Engine::onboard` 原先把 key 序列化成**裸串数组** `["ksk_a"]`,走
`import::map_api_key(s, None)` 这条**不带 region** 的路径。drop 的号全是 us-east-1,
默认值恰好蒙对,所以这个洞一直没暴露 —— 换一家欧洲区的货源就是**每个请求都 403**。

### Features

- 新增 `restock::drop::BoughtKey { api_key, region, subscription_title }`,
  `PurchaseResult.keys` 从 `Vec<String>` 升级为 `Vec<BoughtKey>`。
- `BoughtKey::parse` 同时收裸串与 `{"key": "ksk_…"}` 对象,对象形态顺带取
  `region` / `subscription_title`;仍然只放行 `ksk_` 前缀。
- 新增纯函数 `engine::import_payload()`:上号载荷改为**对象数组**
  `[{"api_key":"…","region":"…"}]`,这样 import 走 `map_flat` → `map_api_key(key, Some(acc))`,
  region 才会落进 `extra`。
- 新增纯函数 `engine::key_strings()`:订单 `keys_json` 仍只记 key 串本身。

### Design Rationale

- **region 不进 `restock_orders.keys_json`**。一张订单的所有 key 来自同一个货架、
  共享同一个 region,所以它是**订单级**属性,该由订单加一列来记(多供应商方案 §3.5),
  而不是在每个 key 上重复一遍。本期因此**不动 store 层、不需要迁移**。
- **空 region / 空档位不写进导入对象**。import 侧对这两个字段用的是
  `filter(|s| !s.is_empty())`,写空串与不写等效;不写能让排障时一眼看出
  「这家没给区域」而不是「区域是空的」。
- **对象形态与裸串形态的 `account_id` 派生完全相同**(无 email 时都是
  `kiro-apikey-{sha256(key)[..12]}`),所以老号不会因为换了载荷形态而变成新号。
  测试 `无区域时与改动前的裸串路径逐字节等价` 把这条钉死。

### Notes & Caveats

- **drop 目前不发 region**,解析到就带上、解析不到留空,行为与改动前一致。
- 已用真实的 EU 号手工验证过整条链路:对象形态导入后 `extra.region=eu-central-1` 落库,
  worker 据此成功查到配额,30 分钟内承接 90 个 `claude-opus-5` 请求全部 200,
  按每输出 token 比同期 us-east-1 的号还快约 18%。
- 全量测试 959 通过 0 失败;新增的 region 测试做过回退验证(撤掉写入即变红)。

## [tier-hold] - 2026-08-04

调度器新增「**降层前先等**」:高优先层只是被 429 节流那几百毫秒时,等它回来,
而不是把请求送给低优先级兜底池。默认关闭,两个热调参数开启。

### 起因(生产实测)

30 分钟窗口:唯一活着的自购速刷号(`G0@0`,并发 100)吃 1082 个请求,同期三个 PRO+
兜底号(`G0@100`)吃 169 个。这 169 个**全部**由 429 造成,分钟级对照近乎 1:1 ——
零 429 的那 13 分钟里兜底号精确为 0。不是容量问题(在途并发 4–13,上限 100),
纯粹是「不肯等 250ms」。

漏出去的两条路径都不经过排队:

1. 吃了 429 的请求当场换号 —— `RateLimited` 不在 `worth_switching_account()` 的排除
   名单里,重新选号时该号刚被 pace,合格集把它剔了,最高层空了就落到 100 层;
2. pace 窗口内新到的请求直接降层 —— `eligible_ids` 本来就排除 paced 号。

排队(`queue_wait_ms`)管不到这两条:它的进入条件是「一个能选的号都没有」。
`acquire_in_group` 里确实有一段「等 pace 窗口过去」,但同样在那个分支里,
只要兜底号还活着就永远够不到。

### Features

- `SchedulerConfig` 新增 `tier_hold_ms`(单次取号的等待预算)与
  `tier_hold_window_ms`(请求开始后多久内其取号还允许等待)。**两者默认 0 = 关闭**,
  与开关引入前逐字节等价;经 settings 覆盖层热生效,不需要重启。
- `AccountScheduler::tier_hold_wait()`:一次持锁扫出「此刻能选中的最高层」与
  「仅因节流暂不可选的最高层」,只有后者严格更优先时才等,返回最早到期时间。
- `acquire_in_group` 增加 `allow_tier_hold` 参数;worker 按请求级墙上时钟
  (`retry_started.elapsed()` vs 窗口)传入,实现「窗口外照常降层」。
- `QueueStats` 新增 `tier_held_total`(每次内部睡眠 +1)。它只回答「开关有没有在
  工作、强度如何」;**不能**与 `paced_total` 相减推算漏量 —— 一次 429 可以让几十个
  并发请求各自等一轮,两者口径不同。量化漏出请看 `request_logs` 的各优先层占比。

### Design Rationale

- **等待判定放在 `select_id` 之前**。后者会把会话亲和的 primary 当场改钉到低层号上
  (「primary 不可用 → 立即改选并当场转正」),先选后等就晚了,还白烧一次跨层迁移的
  去抖额度。放前面还有个附带好处:`select_id` 一行没动,它现有的调度测试零改动通过。
- **不消耗 `attempts`**。那是给 busy 换号用的预算(`total * 5`),小组会被等待循环
  几轮耗光,把成功变成 `AllBusy`(503)。循环终止只由预算的单调递增保证。
- **只等节流,不等冷却**。禁用/冷却态归 `queue_wait_ms` 管,职责不重叠,否则两套
  预算叠加。
- **不等 permit 满的号**。窗口过去照样租不到,等只是把延迟加给客户 —— 与
  `best_available_higher` 用 `available_permits()` 判「高层饱和就别迁」同源。
- **必须封顶,且用墙上时钟而不是重试轮数**。`switch_cap(RateLimited)` 是全组,配 180s
  请求级总时限,不封的话一个持续撞 429 的请求能在同一个号上来回弹到超时。用时间窗口
  而非 `attempts`:后者对所有失败类别共用(凭证刷新失败、`ModelNotAvailable` 都在消耗
  它),拿它当等待额度会出现「前两轮被无关错误吃掉、真撞上 429 时反而不许等」——正好是
  本开关要治的病。窗口远小于 180s,顺带保证等待不会把响应推到那条硬线附近。
- **预算裁剪是最后一步**。睡眠下限(10ms)同时兼任「是否还值得等」的门槛:剩余预算不足
  10ms 就直接降层。若反过来「先裁到剩余预算、再取 max(下限)」,预算尾部会系统性超发,
  实际等待越过配置声明的上限。
- **没改 `worth_switching_account()`**。重新取号时等 pace 窗口再选中同一个号,效果
  等同于同号重试,还顺带保住 prompt 缓存(换号意味着 cache_read 全部重算)。动错误
  分类表会波及所有 provider 的换号语义,收益重复、风险更大。

### Notes & Caveats

- **预算用尽一律降层兜底,绝不因此把错误抛给客户** —— 有专门的用例锁死这条。
- 等待发生在**响应开始之前**,客户端只是变慢,不会看到半截流。
- 惊群:同一个 paced 号上的多个等待者会同时醒来再撞一片 429。用 `tier_held_total`
  的低位派生 0–48ms 确定性抖动散开(仓库无 `rand` 依赖,不为此引一个)。
- **回滚不需要重新部署**:把 `tier_hold_ms` 设回 0,30s 内热生效。
- ⚠️ 本次改的是 **worker 侧**,上线必须重启 worker 容器。账号运行态
  (`disabled_reason` / `disabled_until` / 亲和表 / LRU)只在内存,重启即清 ——
  已冷却/封禁的号会以「可用」状态回到轮转,被选中后再逐个失败下线。建议先把确认死透的
  号在 DB 里 `disabled=true` 再重启。

## [restock-2] - 2026-08-04

补货上线一天后的复盘与重做。**判活从「问 caio」改成「看请求日志」,触发从「钟点表」
改成「算这单划不划算」。**

### 起因(生产实测,不是推断)

过去 11 小时里 ksk_ 号池有 **8.1 小时零成功请求**,而同期全站流量没停 ——
02:00–09:00 那 7 小时的 3599 个成功请求全由贵号池承担。按小时看 ksk_ 占比:
有活号时 79–95%,断供时 0%。三个原因叠加:

| 原因 | 损失 |
|---|---|
| 高峰窗口闸排在算 health **之前**直接 return,没有「池子空了」的逃生口 | 7.0 小时/晚 |
| 余额保留金默认 ¥40 > 单价 ¥21,¥58 的余额买不了 ¥21 的号 | 25 轮 |
| 判活失真:全池零成功时仍报 `healthy=1` 拒绝下单 | 结构性 |

### Features

- **判活三态**:有成功=活;有尝试零成功=**僵尸**(不计入水位);窗口内无任何尝试=
  不下结论(维持 caio 的判断)。新号有宽限期。面板与横幅直接显示僵尸数。
- **单位成本闸**取代硬时间窗:`预期单位成本 = 单价 ÷ (min(需求, 单号吞吐上限) × 预期寿命)`,
  高于 `max_unit_cost_cny_per_credit`(默认 ¥0.04/积分)就不买。掏钱前用真实报价复核一次。
- 高峰窗口保留但**降级为可选硬禁买时段**,判断移到 health 之后(流水从此始终带水位)。
- 回收补上真正的死法:`temporarily_suspended` + 近 6 小时有尝试零成功。
- 账号清单新增**服务时长**、**¥/积分**、**死法**三列;快照新增需求速率、预期单位成本、
  实测寿命中位数。
- 轮询间隔默认 60 → 30 秒;余额保留金默认 40 → 0。

### Design Rationale

- **判活为什么不能只信 `reason`**:`status_snapshot()` 第一件事是 `heal_cooldowns()`,
  而 `TemporarilySuspended`(实测 0/35 永不恢复)被归为「可冷却自愈」,3600s 一到就被
  复活成 `reason == ""`。**补货每轮拉 `/health` 这个动作本身就会触发那次复活**,
  然后把复活的尸体数进健康号 —— 观测行为改变了被观测量。实测 22 个成功率 0% 的号
  各自每 51 分钟被轮到一次(正是 3600s 周期),11 小时白烧 258 个客户请求。
  这套判据 Python 原型推导正确过(`fails_since_last_success`),搬进 Rust 时整块丢了。
- **判活为什么留「不下结论」这一档**:窗口内零成功有两种成因 —— 打不通,和压根没被
  选中。夜里没流量时所有号都零成功,一律判死会触发连环购买。只有「有尝试」才构成
  反面证据。
- **买号策略的依据:号按墙上时钟死,不按用量死。** 22 个自购号全生命周期实测,
  烧速差 3 倍(676 vs 1990 积分/时)而存活一律 0.7–0.9 小时,烧得最猛的反而活得最久。
  推论:①并发拉满是对的,「省着烧」的部分到点直接蒸发;②**闲置一分钟就是烧钱一分钟**,
  需求不够时买号 = 花 ¥20 买 45 分钟只用掉三成,单位成本反而高过它要替代的贵号池。
  所以策略不是「买什么号」而是「什么时候买」—— 这与补货及时性是同一件事。
- **旋钮用「¥/积分」而不是「积分/时的阈值」**:前者随价格自适应。drop 从 $2.95 降到
  $2.20 时可接受的需求门槛自动跟着降,不用人改配置。
- **不做提前量买号**(参数留着,默认 0):检测 + 上号的空档约占 45 分钟周期的 2%,
  而提前 N 秒下单会等长折掉新号自己的寿命(同样是墙上时钟)—— 拿 6.7% 的产出换 2%
  的连续性,净亏。改用把轮询降到 30 秒,零成本减半空档。
- **不动 `worker/scheduler.rs`**:`heal_cooldowns` 复活冷却号对数据面是对的,
  错的是补货拿它当水位。在补货侧修,不去动正服务着付费客户的调度器。
- **实测寿命中位数只展示不自动生效**:这个值估短了就是每轮提前下单、花费翻倍,
  这种旋钮不该自己转。

### Notes & Caveats

- `Health` 原注释断言「掉出正常态 = 真死,是精确判据」——**这个前提不成立**,
  已改写。另一个未爆的雷:`queue_enabled` + `rate_limit_pace_max_strikes = 0`
  (生产实际值)意味着开了排队的号撞 429 **永远**不下线、永远停在 `reason == ""`。
  当前窗口内 0 个 429 所以没触发;判活改成证据驱动后它也不再能骗到水位。
- 生产需同步调三个参数:`peak_start`/`peak_end` 设为相同值(全天)、
  `daily_cap_cny` 设 800、确认 `min_balance_reserve_cny` 为 0。
- 覆盖时长翻倍意味着花费也涨:预计从 ~¥320/天到 **~¥500–560/天**(单位成本不变,
  买的是覆盖率)。¥800 上限是刹车不是目标。
- 池子里还有 6 个**人工上的**零成功号(power-4..7 / ad11f612dea7 / db58169df946),
  自动回收按设计不碰人工号,需人工 `disabled=true`。

## [restock] - 2026-08-03

把原本独立的 Python 自动补货服务(`/root/kiro-restock`，面板 :38995)**整体重写进 caio**。
单进程、单套配置、单个后台界面。

### Features

- **系统设置 → 自动补货**:总开关、DRY-RUN、补号时间段(高峰窗口)、水位、日上限、
  单价上限、熔断阈值、新号并发/排队模式、预测时长、闲时抑制阈值 —— 全部即时生效无需重启。
- **自动补货页**:实况看板、每小时积分消耗曲线(实测柱 + 斜纹预测柱同轴)、
  按钟点/按星期几两种画像、决策流水、ksk_ 账号清单(带成本与单次成本)。
- **按周预测**:用「星期几 × 钟点」画像预测未来 N 小时的积分消耗,
  样本不足时逐级退化(周画像 → 日画像 → 近期均值)并**如实标注依据**。
- **闲时抑制**(默认关):预测消耗低于历史峰值的设定比例时不补货。
- 决策、订单、自购号回收、启动对账、熔断,全部从 Python 版等价搬迁。

### Design Rationale

- **积分数据源换成 `usage_records`,不是 `request_logs`**。后者是
  `REQUEST_LOG_CAP = 10_000` 的硬环形缓冲(实测只覆盖约 1.5 小时);前者同样带
  `metering_credit` 且**永不裁剪**,线上已有 51 天历史。所以**周画像上线即成熟**,
  不存在 Python 版那个「要攒两周」的冷启动。聚合走 `stats_conn` 只读连接,
  避免百万行 GROUP BY 占住写锁让计费落库排队。
- **用 DB 租约做 leader election,不靠部署纪律**。生产上有**两个以上**
  `--mode router` 进程(kiro 一个、dario 一个,开了 exp 栈还有第三个),共用同一个
  control.db。把 60s 循环直接挂在 router 角色上就是各买各的、**重复扣款**。
  `README.md` 里「Router 单实例」是单通道时代的过时描述。
- **补货参数存自己的 KV,绝不进 `SystemSettings`**。后者有回滚地板
  (2026-07-31 前的镜像仍带 `deny_unknown_fields`,加字段会让回滚变成全量 503,
  正是 `375e1e3` 刚修过的坑)。密钥同理只留在 `SystemConfig`,不经 `/settings` 回显。
- **配置分两半**:启动期参数 + 密钥在 `system.yaml`(`:ro` 挂载且 dockerignore,
  改了 restart 即可);运行时可调项在 DB。
- **时区不引 chrono-tz**,用可配的 UTC 偏移分钟(默认 480)。Asia/Shanghai 自 1991 年
  无夏令时,固定偏移即精确,省一个依赖及其编译时间。
- **图用 CSS 柱不用 SVG**:SVG 文字随 viewBox 缩放,375px 窄屏刻度会缩到 6px 不可读;
  也与本仓既有可视化语言一致(用量表的占比条)。时间轴超 56 根自动并桶;
  周画像 168 格给自己的横滚容器(167 个 2px 间隙就 334px,不滚会被挤成 0 宽)。
- **闸门分两类**:自动化闸门(开关/窗口/水位/闲时)手动可越过;
  **花钱闸门(熔断/日上限/余额/单价/DRY-RUN)任何情况都不许越过**。

### Notes & Caveats

- **三层默认全关**:`system.yaml` 的 `restock.enabled`、DB 里的业务开关、`dry_run`。
  部署完不会有任何扣款,必须逐层手动打开。
- 上线过程中被测试抓到三个真 bug,其中两个会在生产直接出事:
  ① `#[serde(default)]` 与 `Default` 实现互相递归 → **启动即栈溢出崩溃循环**;
  ② `strftime('%s','now')` 返回 TEXT 不是 INTEGER → **租约完全失效 → 重复扣款**。
- 号的配额是 **10,000 积分**(`quota.limit`),不是调用次数;但实测号平均只用掉 **13.6%**
  就被上游 403 `TEMPORARILY_SUSPENDED` 封死,且 **0/35 个号在冷却后恢复过**。
  所以补货的真实成本由「号被扫死的速度」决定,不由配额决定。

## [account-sort] - 2026-08-02

### Features
- 账号列表新增**排序**选择器:「最新在前」(默认) / 「最早在前」/「按组+名称」。
  纯前端排序,不改后端接口。

### Design Rationale
- **默认改成「最新在前」**:后端 `list_accounts` 固定按 `group_name ASC, account_id ASC`
  返回,新上的号会散落在字母序各处。而 Kiro 短命号(`ksk_` API Key 实测寿命只有 14–22 分钟、
  额度约 320 次成功调用,且**成批死亡**)只能按上号时间管理,旧顺序完全帮不上忙。
- **同批号按 `account_id` 兜底排序**:成批导入的号 `created_at` 是同一秒,没有第二排序键的话,
  账号页 15s 运行态轮询每刷新一次同批号就互相跳位,根本点不中。
- **`sortAccounts` 是纯函数、不改入参**:调用方是 `useMemo`,原地 `sort()` 会污染
  react-query 缓存里的数组,让持有同一引用的其它 memo 读到被打乱的顺序。
- **排在筛选之后、分页之前**:保证「第 1 页」永远是当前筛选条件下最新的号。

### Notes & Caveats
- 服务器 Node 是 v12、本机 v14,都跑不了 Vite 6 / React 19。构建走 `oven/bun:1-slim`
  容器(该镜像本地已有,dario-sidecar 在用),**无需在宿主装任何东西**:
  `docker run --rm -v $PWD/admin-ui:/app -w /app oven/bun:1-slim sh -c 'bun install && bun run build'`
- `embed-ui` 是**编译期 feature**,前端改动必须重建整个 Rust 镜像才能生效,没有热更新路径。
- 上线用**新标签** `acctsort-20260802` 构建,保留 `poison-fix` 作回滚锚点;compose 已备份为
  `docker-compose.yml.bak-acctsort-20260802-151425`。只重建了 router(worker 不服务 `/admin`,
  不动它们就不断流量)。**回滚**:把 compose 里的 image 改回 `claude-all-in-one:poison-fix`
  再 `docker compose up -d router`。
- 上线前已核对:`crates/` 下 **0 个**源文件比 `poison-fix` 镜像新,故本次镜像不夹带任何
  未经上线验证的 Rust 改动;两个镜像二进制 md5 不同属预期(前端被 embed 进二进制)。
- 验证:`tsc --noEmit` 0 错误、`vitest` 15/15 通过(其中 6 个新增排序用例)、线上 bundle
  中英文案均命中、SPA 挂载无 console 报错、`/admin/api/{ping,accounts}` 与 `/health` 均 200。


## [queue-wait] - 2026-07-31

### Features
- 新增**逐账号**「排队等冷却」开关 `extra.queue_enabled`(默认关)。开启后 `acquire` 在
  「组内全禁用」时不立刻返回 503,而是等冷却中的号自愈再选,把上游限速在网关内部消化掉。
- 新增 `scheduler.queue_wait_ms`(默认 `0` = 关闭)作为**最长等待时间**,热调
  (`PUT /admin/api/settings`,worker 30s 轮询生效)。
- `PATCH /admin/api/accounts/{id}` 新增 `queue_enabled` 字段(走 `merge_account_extra`,
  绝不碰凭据);`GET /accounts` 回显该字段。

### Design Rationale
- **为什么逐账号而不是全局**:企业号(`ksk_`/IdC)的上游并发是**跨租户共享**的,429 是跟
  别的买家抢同一个池子 —— 等一下真的就有。而社交号的 429 常伴随额度见底,等待只是把
  客户多挂几秒后照样报错。一刀切会把后者的失败延迟化,体验更差。
- **为什么等待发生在响应开始之前**:此时还没发 HTTP 状态码,客户端只是变慢,不会看到
  半截 SSE。若改成"先发 message_start 再抢号",就再也不能返回 503,只能在流里发 error
  事件,客户端处理方式完全不同 —— 那是协议改动,不在本次范围。
- **队列容量按并发之和动态定**:上限 = 本组内已开排队、**且预算内真能服务**的号的
  `max_concurrency` 之和(1×)。额度跑干/config 禁用/1h 封禁的号**不计入** —— 否则一堆
  跑干的号会把容量撑大,等待者远超真实吞吐、全部排到超时(正是本开关要防的堆积)。
  等待者再多也吃不下更多并发,超出的只是排在后面陪跑到超时 —— 客户等更久、结果一样。
  取 1× 让最坏等待≈一次请求的周转时间,而不是好几轮。
- **额度跑干的号不产生等待**:`QuotaExhausted` 的 `disabled_until` 是 `None`,不构成
  "等得到"的理由。池子真干时仍然快速失败,不会把容量问题放大成全站卡死。同理,到期
  时刻在预算之外的(如 1h 的 `TemporarilySuspended`)也不等。
- **排队位用 RAII 守卫**:客户端中途断开会让整个 acquire future 被 drop,手写 decrement
  在那条路径上不会执行,计数只涨不落,几分钟后队列就永久"满"了。

### UI(同版追加)
- 账号表新增**排队**列(开/关),鼠标悬停说明该号冷却时的行为差异。
- 编辑弹窗新增**排队等冷却**开关(Segment 两档),提交时与原值比较,没动就不带该字段
  —— 否则对着不认这个字段的旧后端,只改并发也会让**整个保存**被判 400。
- 账号页顶部新增**排队实况**横幅:`等待数 / 容量`,以及已开排队的号数。容量口径与
  准入一致(只算开了排队**且当前可服务**的号),所以这个比值是真实拥挤度,不会因为库里
  躺着一堆额度跑干的号而虚高。等待数触到容量时标红。
- 排队实况新增两个**累计**计数:`queued_total`(进过排队的请求数)、`paced_total`(被节流吸收的
  429 次数)。理由:`waiting` 是瞬时值且几乎恒为 0(排队只在全组不可用时触发),准确但看不出
  机制有没有在工作;而节流日志是 `debug!` 级、线上 `RUST_LOG=info` 根本看不到 —— 累计值是
  这两个机制唯一的可观测面。上线首 4.5 分钟实测 `paced_total=627`(≈140 次/分钟的 429 被吸收)、
  `queued_total=0`。
- worker `/health` 与 admin `GET /accounts/runtime` 新增 `queue{waiting,capacity,enabled_accounts}`;
  `accounts_status[]` 新增 `queue_enabled`。旧 worker 不返回该字段时 UI **整块不渲染**,
  而不是显示 `0/0` 误导运维以为队列是空的。

### 新增:账号探针 `POST /admin/api/accounts/{id}/probe`
- **钉住指定账号**真发一次最小 chat(`max_tokens=16`、单轮 "hi"、收到首个文本 delta 即断流),
  返回 `{replied, model, elapsed_ms, text, error_kind, error}`。
  `?model=` 缺省 `claude-haiku-4.5`。
- **为什么必须有**:`/quota` 的 `verified:true` 只证明**控制面**凭据活着,`/models` 只证明
  目录里有这个模型 —— 两者都不代表数据面能出词。实测存在「有额度、目录有 opus,一发 chat
  恒 `ModelNotAvailable`」的号。判定停用号能否复活,只有真收到 delta 才算数。
  在此之前 caio **没有**任何"指定账号发一次请求"的能力,调度器自己选号。
- **不上报账号健康**:人工探测失败不计入与真实流量共用的失败池/冷却,否则批量探测会把好号
  探成 `too_many_failures`(与 `/quota` 的取舍一致)。
- ⚠️ **调用方必须串行 + 限速**。短时高频 chat 验号历史上直接导致 `TEMPORARILY_SUSPENDED`
  (22 分钟送走 5 个号)。服务端只做单次最小化,**批量节奏不替调用方兜底**。

### 新增:429 节流(替代二值冷却),只对开了排队的号
- `scheduler.rate_limit_pace_ms`(热调,默认 `0`=关)。开启后,开了 `queue_enabled` 的号
  命中 429 **不再下线**:只在 `pace` 毫秒内不被选中,到点继续抢 —— 即"保持一个频率访问"。
  面板上这类号仍显示**正常**,不再是"限流冷却"。
- `scheduler.rate_limit_pace_max_strikes`(默认 10)= **熔断**:连续这么多次 429 仍未成功,
  放弃节流退回二值冷却。上游真把号限死时不能无限定频硬撞(22 分钟送走 5 个号的教训)。
  `report_success` 一次即清零连击并解除节流闸。
- 请求级新增 **180s 重试总时限**:限流类错误的 `switch_cap` 本就是 `total`(全组),再叠加
  每轮取号各一份 `queue_wait` 预算,一个一直撞 429 的请求理论上能循环好几分钟 —— 而下游
  yapi 是 300s 无 event 即中止,超了客户只会看到更难查的中断。

### Fixes
- **节流窗口的取号空洞**(自查用例抓到):被节流的号 `disabled=false`,于是 `select_id`
  跳过它、错误分类却把它算进 `avail_total`,两边都不认领 → 掉进 `AllDisabled` 分支把 503
  抛给客户。现在单独统计节流中的号,像 busy 一样短睡重试,**与队列开关无关** ——
  节流窗口是我方自己设的几百毫秒,不该变成客户的错误。
- 探针扇出**不能复用 `AdminState.http`**:它是 2s 超时(为的是 worker 离线时快速跳过),
  而探针要真发一次 chat。超时在扇出循环里等价于"该 worker 离线",最终误报成
  「没有 worker 持有该账号」。已改用独立 120s 客户端。

### Notes & Caveats
- 等待预算**必须远小于**下游客户端的空闲判定。yapi 是 300s 无 event 即中止,建议
  `queue_wait_ms` 不超过 20000。
- 等待分支刻意**不消耗** `attempts` 预算(那是给 busy 换号用的),循环边界只由
  `queue_wait_ms` 决定。
- 队列满时返回 `AllBusy`(而非 `AllDisabled`),两者对外都是 503,仅日志可区分。
- **尚未做**:admin 面板 UI 里的账号级开关(当前只能走 API);按账号类型自动开启。


## [thinking-effort-default] - 2026-07-30

默认思考档位从 `max` 降到 `high`(用户反馈"太慢了"),并把它从编译期常量变成**设置面板
可热改**的运行期参数。

### Features

- **`DEFAULT_EFFORT`: `max` → `high`**。唯一事实源移到
  `gw_core::config::DEFAULT_THINKING_EFFORT`,`gw_kiro::anthropic_types::DEFAULT_EFFORT`
  改为它的别名 —— 该常量同时是配置 schema 的默认值,两处各写一份字面量必然漂移。
- **新增 `thinking.default_effort` 配置段 + `default_thinking_effort` overlay 字段**,
  走既有热更新链路:面板改 → DB overlay → worker 30s 轮询 → `apply_hot_settings` →
  `anthropic_types::set_default_effort`。**改档位无需重启 worker**。
- **设置页新增「思维链」区块**:five 档 segmented 选择器,每档给出实测的深度/延迟取舍,
  选中档位的说明实时显示在下方。
- **`PUT /settings` 校验档位合法性**:非法值 400 并列出可选档位,归一成小写后才落库。

### Design Rationale

**为什么降档**:2026-07-28 的剂量反应实测确认 `effort` 是真正在起作用的旋钮
(`low` 122 帧 → `xhigh` 644 → `max` 1100,单调无重叠),当时据此把默认提到顶格。
但深度不是免费的 —— `max` 约 1.7 倍于 `xhigh` 的思考量,就是约 1.7 倍的等待**和输出计费**。
`high` 的签名加密体是 `xhigh` 的 73%、耗时 95s vs 124s,是折中点,且恰好是上游多数模型
schema 自己的 `default`。**唯一例外是 opus-4.7**(schema `default` 是 `xhigh`):对它我们
现在显式发一个比上游默认更低的档,这是本次降档的本意,专门加了一条用例钉住
(`opus_4_7_default_request_lands_on_policy_default_not_upstream_xhigh`)。

**为什么用进程级全局而非依赖注入**:转换层(`thinking_policy` / `converter`)是一组自由函数,
`chat_stream` 与 `render_kiro_payload` 都不持有 provider 句柄,把参数一路穿下去要改到
gw-app 的请求日志路径。这与 `converter/cache_point.rs` 里 `thinking_signature` 等热控开关
同款,不新造机制。

**为什么归一逻辑要拆出纯函数版**:`normalize_effort` 现在读可热改的全局,拿它做断言等于让
用例依赖全局当下的值,并发跑的其它用例一改就互相污染。逻辑全部下沉到
`normalize_effort_with(fallback, raw)`,单测走这个入口;`set_default_effort` 自己的用例
只验校验语义(成功用例故意设成**当前值**,可观察状态不变),**本 crate 的测试从不把运行期
默认档改成别的值**。

**校验为什么做两层**:admin `PUT /settings` 挡住接口路径,`set_default_effort` 挡住手改 DB
绕过接口的路径。这个值会原样进 wire(`additionalModelRequestFields.effort`),脏值换来的是
上游 400,而且要等下一次真实请求才暴露。

### Fixes(对抗审查,3 个 lens 各自独立提出)

- **档位改成枚举 `ThinkingEffort`,非法值在配置装载边界不可表示**(3/3 lens 一致,Skeptic 判 high)。
  原先是无校验的 `String`:`system.yaml` 写 `default_effort: hihg` 能解析成功、`GET /settings`
  照实返回 `hihg`,而数据面消费点拒收并继续用旧值 —— **控制面显示值与实际生效值永久分叉**,
  面板上五个档位一个都不高亮,运维每 30 秒收一次告警却看不出该改哪。改枚举后 serde 在
  配置装载与 `PUT /settings` 两处边界直接拒绝,这个状态不可达。新增用例钉住
  `VALID_EFFORTS` 与 `ThinkingEffort::ALL` 逐项相等(两份档位表只在一处加档 = 面板能选、
  wire 拒收)。
- **`system.yaml` 存在却解析失败时拒绝启动**(Skeptic#1,既有缺陷,本次新增的 `thinking` 段
  给它添了新触发点)。router 与 worker 原先都是 `unwrap_or_default()` / 双 `.ok()`,而各配置段
  都带 `deny_unknown_fields` —— 一个拼错的字段名会让**整个** `SystemConfig` 静默换成默认值,
  上游超时、调度参数、缓存计费、实验开关一起被重置,线上只表现为"行为莫名其妙变了"。
  现在:文件**缺失**仍用默认值(合法形态),**存在却解析不了**当场报错退出。
  上线前已用生产 `config/system.yaml` 验过能解析(`upstream_timeout_secs=720` 等真实值),
  四个容器共用这一份、无人覆盖 `--system`。
- **补齐"热改真的生效"的端到端测试**(3/3 lens 一致)。原先所有新增用例都刻意不改进程级全局
  (避免污染同进程并发跑的 865 个用例),后果是**把 `set_default_effort` 里的赋值删掉、
  或让 `normalize_effort` 回头读编译期常量,单测依然全绿而线上"面板改了不生效"**。
  新增 `crates/gw-kiro/tests/thinking_effort_hot_reload.rs`:集成测试是独立进程,可以放心改
  全局(文件内用 `SERIAL` 互斥锁串行)。覆盖 `apply_hot_settings → 全局 → wire payload` 全链路、
  热改后仍走逐模型夹取(4.6 收到 xhigh 回落 high)、无 schema 的模型一个字段都不发、
  以及客户端显式档位不被默认值压掉。
- **`normalize_effort_with` 收回私有**(Minimalist#1)。它的 `fallback` 不校验合法性,公开出去
  等于给外部一条绕过档位白名单、把任意串送上 wire 的路;crate 内唯一调用方喂的是
  `default_effort()`,恒合法。
- **前端 patch 不再无条件携带该字段**(Architect#1)。`buildPatch` 原先拿补过默认值的表单值与
  **原始**响应比,旧后端不返回该字段时 `'high' !== undefined` 恒成立 —— 用户只改代理也会捎带
  这个字段,被旧后端的 `deny_unknown_fields` 判 400,整个保存失败。现在比较对象与
  `settingsToForm` 同口径补默认值。
- **`high` 的说明从「当前默认」改为「出厂默认」**(Skeptic#4)。它现在是可热改参数,
  管理员存了 `max` 后再看这句会自相矛盾。

不采纳两条:Architect#3 称"多个 worker 各自轮询、旧快照覆盖新快照",但一个进程只跑一个
worker(`--mode worker --instance N`),进程内只有一个轮询者写这个全局,前提不成立;
Minimalist#4 嫌本条目的设计辩护冗长,但项目约定要求 changelog 记 Design Rationale。

### Notes & Caveats

- 热应用时**字段缺失 = 不动当前值**,不回落编译期兜底 —— 轮询响应偶发缺字段不该把面板上
  设的档位悄悄打回出厂值。非法值只告警不生效。
- 4.5 系与 haiku 上游没有 `additionalModelRequestFieldsSchema`,这个设置对它们**不起作用**
  (一个字段都不发),UI 文案里已注明。
- 按 `budget_tokens` 翻译档位的老客户端(opencode 全量、部分 Claude Code)走
  `budget_to_effort`,**最高只到 `xhigh`**,到不了 `max`,也不受本设置影响。
- `OutputConfig::effective_effort()` 仍是仅测试可达的死代码(生产无调用点),本次只让它
  跟随运行期默认值,没有删。

## [admin-ui-memberships] - 2026-07-29

07-28 上线的「账号-分组多对多」重构后端全套改完了,admin-ui 一直停在旧模型上。
这次把运维界面对齐到新调度模型,并收敛上号入口。

### Features

- **`GET /accounts` 返回成员边**:每行新增 `groups: [{name, priority}]`(组名升序)。
  新增 `SqliteStore::list_all_memberships()` 一次查全表聚合,不按 owner 过滤 —— 与
  worker 选号用的 `load_group_memberships(owner)` 是两个口径,后者只回本 owner 名下的边。
- **账号编辑弹窗可改成员边**:每个分组一行「勾选框 + 高/低两档」,一个号可同时属多组、
  每组独立优先级。提交时按差集只发变化的请求(`upsertGroupMember` / `removeGroupMember`),
  单条失败点名是哪个组、不关弹窗。
- **删掉编辑弹窗里那个写 `extra.priority` 的优先级控件** —— 重构后调度根本不读它,
  改了没有任何效果,留着只会继续误导。
- **账号表格改显示成员边**:分组列渲染多个 chip;成员边为空时显示红色「无分组」。
  优先级列改为按组汇总(`高×1 · 低×2`),逐组明细在 title 里。
- **导入弹窗的方式选择改成两栏卡片**,标题由「导入 KiroManager 账号」中性化为「导入账号」。
- **移除「添加账号」入口**,claude-dario 粘贴 `.credentials.json` 的路径挪进 OAuth 弹窗的
  折叠区;后端 `POST /accounts` 端点保留(粘贴路径仍在用)。

### Design Rationale

**为什么必须做**:2026-07-29 踩了一次。3 个新导入的社交 POWER 号只有 `G0@100` 一条成员边,
而 G0 挂的两把 key 24 小时零请求 —— 号在 runtime 里显示「可用、0.0%」,`request_logs` 里
却一行都没有,白放了一天才被发现。旧 UI 既看不出一个号属于哪几个组,也改不了成员边,
这几天所有边都是手工 `curl` 敲的。

**`groups` 只加在列表响应上**,单条增删改的响应仍不带 —— 前端那几个 mutation 只做
invalidate、不回写缓存,加了也没人读。

**组名升序只为展示稳定**(表格里的 chip 不因刷新而跳动);差集比较是按组名做 Map 查找的,
不依赖顺序。测试钉住顺序是为了防表格抖动,不是差集正确性的前提。

**新勾选的组默认「低」档**,与后端 `default_member_priority` 一致。这也是更安全的一档:
新号直接进高优先层会一上来吃掉全部新会话 —— 07-29 五个 ksk_ 号就是 `pri=0 / conc=10`
配置下 22 分钟被上游风控封掉的。

### Fixes(对抗审查发现,三个 lens 跑在 codex 上)

- **[三人共识 high] 改归属可以绕过"一组一 owner"不变量**。`upsert_membership` 守着建边侧,
  但 `update_account` 直接写 `accounts.group_name`、不校验已有成员边 —— 边一条没动,
  却把边另一端的 owner 换了,同一条不变量从另一头被破坏,而且是运维在 UI 上点一下就能走通。
  现在 `update_account` 返回 `UpdateAccountOutcome`,在**同一事务**内先查"这个号参与的每个组里,
  别的成员归属谁",有冲突整单拒绝并回 400 点名是哪个组。**这是重构就带进来的既有缺陷,不是本次引入。**
- **[medium] 两档 UI 会静默改写 0/100 之外的历史优先级**。草稿原本存"高/低"档位,提交时映射回
  0/100,于是一条实际优先级 50 的边会在运维只改并发时被顺手改成 0。改为草稿存**原始数值**,
  只有用户真的点了档位按钮才写 0/100。
- **[medium] 部分失败会让账号掉容量**。成员边原先是加删混在一起并发发出;若新增因 CrossOwner
  失败而删除成功,账号会当场比原来的组还少。改为 `applyMembershipDiff` **先加后删,加失败就完全不删**
  —— 最坏只是多挂几个组,可见可恢复,不掉容量。
- **[medium] 部分失败后重试永远报错**。已成功的删除在重试时会再 DELETE 一次,后端返回 404 →
  又被记成失败。删除现在对 404 宽容(边本来就不在 = 目标状态已达成)。
- **[medium] N 条边 = N×2 次缓存失效**。改为单个 `useSaveMemberships`,全部 settle 后只失效一次
  (用 `onSettled` 而非 `onSuccess`,部分失败时同样要拿到权威新基线)。
- **[medium] `GET /accounts` 不是同一数据库快照**。原先分两次调用、中间释放了锁,导入正好插在
  中间就会返回"账号已建、边还没有"这种从未真实存在过的组合(`create_account` 是原子建号+建边)。
  合并为 `list_accounts_with_memberships()`,一把锁读完两张表。
- **[low] `groups` 缺失被当成"没有分组"**。旧缓存响应缺该字段时表格会误报红色"无分组"。
  现在区分 `undefined`(未知,显示 —)与 `[]`(确实不在任何组,报红)。
- **[low] UI 文案里的 `**markdown**` 原样显示星号**(放进原生 `title` 和普通 `<p>`),已去掉。

**未采纳**:审查建议加一个事务型的批量 replace 端点,让"保存账号"成为原子操作。
先加后删 + 单次失效已经堵掉了会真实伤到客户的那条路径(掉容量),剩下的中间态是
"多挂了几个组"这种可见可恢复的情形。等真有跨 owner 迁移需求时再上端点。

### Notes & Caveats

- `upsert_membership` 的 **CrossOwner 拒绝**(一个组的成员必须同 owner)会以 400 返回,
  前端原样透出错误文案。当前池子里除 DARIO 的 2 个号外全是 owner=G0,实际不会触发。
- 改归属现在要求**先把该号从与别人共享的组里摘出去**。这不是新限制,是原本就该有、
  只是一直没人拦的约束;实践中改归属本来就极少发生。
- admin-ui 经 `rust-embed` 内嵌进二进制(`router/mod.rs`),**改前端必须重建整个镜像**,
  不能只传 dist。
- 顶层 `priority`(=`extra.priority`)字段保留,仍是导入时的默认种子,语义未变。
- 一并清掉了随 `CreateAccountDialog` 删除而孤立的 30 个 i18n 键(zh/en 各 30 条)。

## [effort-max-default] - 2026-07-28

隔离栈交叉实验推翻了「旧 thinking 标签让思考更深」的结论。改用真正有效的那条通道。

### Features

- **默认思考档位 `xhigh` → `max`**(`DEFAULT_EFFORT`)。
- **停发旧 thinking 文本标签**:生产去掉 `KIRO_LEGACY_THINKING_TAGS`,
  `history[0]` 不再含 `<thinking_mode>/<thinking_effort>`,与 1.0.212 客户端一致。

### Design Rationale

先前基于一轮 n=4 的对照得出「关掉旧标签思考深度减半、省 43.5% 积分」。**交叉实验推翻了它**:
把处理在两个账号间对调后差异消失(ON 687 vs OFF 644,范围完全重叠),而第一轮的
2.7 倍差距主要来自**账号本身**(phamdragon 均 900 vs phambac 均 559)。原实验处理与账号共线。

同一批实验给出了真正有用的结论 —— **新字段才是起作用的旋钮**,且剂量反应干净:

| effort(仅新字段,无旧标签) | thinking 帧 |
|---|---|
| `low`   | 123, 121      → 122  |
| `xhigh` | 894, 509, 529 → 644  |
| `max`   | 1345, 855     → 1100 |

单调、无重叠。且带着旧标签时新字段照样说了算(`low` 70 帧 vs `xhigh` 687 帧,9.8 倍),
说明两条通道不互斥、新字段主导;`ON+low`(70)甚至低于 `OFF+low`(122),
旧标签在低档位不但没加深度还略微反向。

所以:**要深度就提 `max`,而不是靠在正文里塞提示词。** `max` 在上游 enum 里、真客户端
也发得出,是合规通道,不增加指纹;旧标签则效应存疑还带着真客户端不会有的文本特征。

`max` 在**所有**带 schema 的模型上都存在(含没有 `xhigh` 的 4.6 系),所以这个默认值
对全系模型都能原样落地,不会触发按模型回落。

### Notes & Caveats

- **积分会涨**。生产实测计费近似 `每请求 ≈ 0.84 + 输出token/1000 × 0.48`
  (输入 17-27 万 token 因 96% 命中缓存只值 0.84,**输出才是驱动项**)。
  thinking token 计入 output,所以档位提高必然涨钱。但真实流量 82.5% 的请求输出 < 600 token,
  底价占大头,涨幅远小于重推理场景的测试值。
- 本次同时移除旧标签 = 改动 `history[0]` = **一次性冲掉全部在途会话的缓存前缀**。
  与档位改动一起部署,这笔代价只付一次。
- 交叉实验里有 1 条请求失败(4s,429),已从统计中剔除;其余 13 条全部 200。


## [kiro-1.0.212-align] - 2026-07-28

同步到 Kiro 1.0.212 客户端的线缆形态。长期不同步 = 可被规则化识别 = 封号,这是唯一动机。
依据是拆包 `extensions/kiro.kiro-agent/dist/extension.js`(22 MB / 536,408 行)+ 真号只读实测。

### Features

- **`effort: "max"` 是合法最高档,不再被降级成 `xhigh`。** 真实 enum 为
  `["low","medium","high","xhigh","max"]`。此前把 `max` 当同义词映射,等于把客户端顶格的
  请求(生产每 300 条约 33 条)静默降一级。
- **thinking 档位改为逐模型夹取**(`clamp_effort_for_model`)。档位表随模型不同:
  `opus-4.6`/`sonnet-4.6` **没有 `xhigh`**;`opus-4.7` 的 schema default 是 `xhigh`
  (全表唯一);`opus-4.5`/`sonnet-4.5`/`haiku-4.5` 压根没有 schema → **一个字段都不发**。
- **`ListAvailableProfiles` 也迁到 `management.{region}.kiro.dev`**(POST 路径不变)。
  它属于 `AmazonCodeWhispererService`(UA 仍是 `api/codewhispererruntime`),但 1.0.212
  构造该 client 时的 endpoint 来自 `cpsConfigs`(`:389260`),值就是 management 域。
  **`q.{region}.amazonaws.com` 在 1.0.212 全树一次都没出现。**
- **`getUsageLimits` 迁到 control-plane**:
  `GET https://management.{region}.kiro.dev/Get-Usage-Limits?origin=AI_EDITOR[&profileArn]`,
  UA 换成 `api/kirocontrolplanebearer#1.0.0`,**不再发 `resourceType`**。
  `KIRO_LEGACY_QUOTA_ENDPOINT=1` 可整套切回旧形态。
- **新增 `ListAvailableModels` 接入**(`models_api.rs`)+ 落库 + 两个 admin 端点:
  `POST /accounts/{id}/models`(用该号拉一次并落库)、`GET /models/catalog`(读快照)。
  取回 `rateMultiplier` 供定价、逐模型档位表供上一条夹取。
- 顺带上线前一批已就绪改动:body 里必发 `agentMode`、无工具时不发空
  `userInputMessageContext`、UA 版本 → 1.0.212、旧 thinking 文本标签默认关。

### Design Rationale

- **为什么不补 `thinking` / `max_tokens`**:上游 schema 确实声明了这两个属性,但客户端的生成
  函数 `qe8`(`extension.js:222579`)整个函数体只有两个 `case`,产出恒为
  `{output_config:{effort}}` —— 补了就是比真客户端**多发**字段,与做这件事的初衷相反。
  (原计划里有这一条,拆包后撤销。)
- **为什么不补 `systemPrompt`**:`extractSystemPrompt` 受 A/B 开关
  `AB_SYSTEM_FIELD_INJECTION` 控制,默认 **false**(`:227082`)。关着时 system 留在
  `history[0]` —— 正是 caio 现有做法。当初为省积分这么设计是对的。
- **为什么档位不可用时回落 `default` 而不是升到 `max`**:对齐客户端 `A7`
  (`:140071-140076`)与设置面板(`:340057`)的行为。宁可少一档,也不擅自升档制造真客户端
  不会出现的形态。
- **为什么老配额端点必须换**(结论不变,但理由要说准):我一度断言
  `AmazonCodeWhispererService.GetUsageLimits`"全树零调用点" —— **那是错的**,`:493283` 的
  `fetchUsageLimitsData` 就在调它、还带 `resourceType`(只在"列 profile 顺带查用量"时走)。
  漏判是因为只搜了控制面的 `new yh(`。真正为零的是**域名**:`q.*.amazonaws.com` 全树不出现,
  连那个"runtime client"也被 `cpsConfigs` 指到了 management 域。换的是域名,不是"操作没人用了"。
  配额是后台**每 20 分钟一轮**的常驻轮询,比偶发请求的信号稳定得多,更该对齐。
- **模型目录复用 `settings` 表**(键 `model_catalog`)而不是新建表:零 schema 变更、零迁移。
  测试钉死了它与 `system` 键互不影响。

### Notes & Caveats

- **端点路径大小写和连字符是有意义的**:Smithy `@http` trait 逐字为 `/Get-Usage-Limits` /
  `/List-Available-Models`,不是 `/getUsageLimits`。
- **UA 虚惊一场**:客户端源码里 `getCustomUserAgent()` 返回空格分隔的
  `KiroIDE {ver} {machineId}`,但 SDK 的 `escapeUserAgent` 会把空格转成 `-`
  (`:356844-356846`),最终线缆上仍是 `KiroIDE-{ver}-{machineId}`。现有拼法是对的,未改。
- **上线前真号只读实测**(不发 chat):新旧配额端点返回 **937 字节、6 个解析字段逐字相同**;
  带不带 `resourceType` 返回**完全一致**(breakdown 恒为 `CREDIT`,上游根本不读该参数)。
  `ListAvailableProfiles` 用一个 **IdC 号**(最依赖这条路径的类型)两端各打一次:
  均 200、468 字节、**profileArn 逐字段相同**。
- **隔离栈端到端实测**(独占 2 个号、同出口、12 个请求全 200 零错误):
  `max` 档位原样上 wire;新配额端点与 `ListAvailableModels` 均真机打通(19 个模型,
  档位表与静态表逐条一致);前缀击穿护栏按设计只在未设 `KIRO_LEGACY_THINKING_TAGS` 的一侧告警。
- **`m/N,E` 这一段是沿用**,未从 bundle 中重新推导。它由运行时 feature 检测生成,
  控制面与 runtime 两个 client 的 bearer 认证路径相同,推测一致但未逐字验证。
- **本批全部前缀安全**,不动 prompt 缓存前缀。移除 `history[0]` 里的 thinking 标签
  **不在本批** —— 那条会让所有在途会话下一轮全 miss,须低峰单独切,并对着基准
  (opus-5:0.9584 积分/请求、83.4% 命中)盯两个数。

## [thinking-budget-floor] - 2026-07-28

客户端可以把按 opus 计费的请求降级成浅思考。给它设个下限。

### Features

- **`enabled` 模式的思考预算下限**:低于 `KIRO_MIN_THINKING_BUDGET`(默认 8192)时抬到下限。
  抬升被 `max_tokens - 1024` 夹住,夹不下就保持客户端原值(绝不挤掉答案的空间)。
- **只动 `enabled`**:`adaptive` 的深度由 effort 决定、budget 字段不参与;`disabled` 一律不碰。
- **`effort: "max"` 作为 `xhigh` 的同义词翻译**,不再当非法值回退。

### Design Rationale

- **为什么要有下限**:生产实测 **opencode 18/18 条请求都发 `budget_tokens=1024`**,签名加密体
  (真 CoT 的载体)只有 5800 字节 = `xhigh` 21984 的 26%,用户体感"秒回,不像在思考"。
  我方按 opus 计费,不该让客户端把它降级。
- **为什么不碰 `disabled`**:抽样 48 条 `disabled` **全是 Claude Code 的内部杂务** ——
  16 条 `max_tokens=64` 的会话标题生成 + 30 条 haiku 的技能路由。给它们开思考是纯烧钱
  + 拖慢 UI,对"用户用上聪明的 Claude"零贡献。
- **这是抬"上限"不是设"目标"**:`max_thinking_length` 是天花板,模型按需取用。上线前后实测
  同一道题:签名 5800 → **10260(+77%)**、耗时 45s → 68s,但输出只从 1621 → 1722 token
  (**+6%**)。成本不是按比例涨的。
- **为什么 `max` 是翻译不是回退**:Kiro 档位到 `xhigh` 为止,`max` 语义上就是顶格。之前当
  非法值处理,30 分钟刷 103 条告警,还掩盖了真正的脏 effort。

### Notes & Caveats

- **可见 thinking 文本长度不能用来判断思考深度**。实测 `high` 的可见摘要只有 1579 字符
  (全场最短),而它的签名加密体 15940 比 `low` 的 10744 大 48%。要看深度就看
  **signature 长度和耗时**,两者随 effort 严格单调。旧注释「high 仅产桩推理」据此推翻。
- **签名是单向的,这是 Kiro 协议决定的,不是缺陷**:线缆结构 `AssistantMessage{content, tool_uses}`
  没有 thinking 字段。客户端回传的 thinking 块会被静默丢弃 —— 实测两轮往返 HTTP 200、
  答案正确、缓存命中 4450 token,**不破坏会话**。副作用反而是正向的:上游收不到历史推理,
  模型每轮都从头重新思考,不存在"上轮没传回去所以这轮变笨"。
- 下限调高需评估成本:8192 实测约等于 `adaptive` + `low` 的深度(签名 10260 vs 10744)。

## [client-error-sanitization] - 2026-07-28

对外错误只说「客户能做什么」,不说「这条渠道背后是谁」。

### Features

- **`UpstreamError` 拆成两份文案**:`message`(内部诊断,含上游原始报文/接口名,只进
  `tracing` 与运维面)与 `client_message()`(对外)。后者是唯一对外出口。
- **`client_detail: Option<String>` 且 fail-closed**:默认 `None` → 按 `UpstreamErrorKind`
  给中性文案。只有我方本地生成、逐条确认过的文案才用 `bad_request_visible` /
  `with_client_detail` 登记(体积超限、报文解析失败、模型不支持、毒报文拦截)。
  新增错误点忘了登记 = 客户少看到一点细节,而不是 = 泄露渠道来源。
- **三条对外出口统一口径**:`upstream_error_response`(非流式/首包前)、
  `sse_error_event`(流内 error 事件)、`AcquireError::client_message()`(选号失败)。
- **运维面单独留一条 `admin_error_response`**:worker 的 `/oauth/exchange`、
  `/accounts/{id}/refresh`、`/accounts/{id}/quota` 仍回全量原文 —— 它们 listen 在
  127.0.0.1,由 admin 面板扇出调用,客户到不了。
- **router 侧同步脱敏**:`分组 'GECO' 无可用 worker` / `worker 不可达` → 统一
  `CLIENT_UNAVAILABLE`。分组名就是价格档,`worker` 就是账号池形态。
- **补日志**:非流式抽干与流中硬错误此前**没有**任何一处记录上游原文(唯一的去处就是
  发给客户端)。脱敏前先把这两处的 `tracing::warn` 加上,否则排查会变瞎。
- **公网 `GET /health` 收成纯存活探针**(对抗评审 high,两个镜头一致):旧实现无鉴权
  返回 65 KB —— `provider: kiro`、分组名(= 价格档)、出口 IP,以及 **220 个账号 ID
  (真实邮箱)** 连同配额/优先级/禁用态。报错文案脱敏得再干净,一条 `curl /health`
  就把渠道来源和整个账号池抖干净。明细在 `GET /admin/api/accounts/runtime`(admin 鉴权)
  里一条不少,面板走的本就是那条。顺带去掉「每次公网探针扇出 N 个内网请求」的放大面。
- **上游自发的 SSE `error` 事件也过闸**(对抗评审 high):provider 把错误当**普通 SSE
  事件**产出时(dario 直透 Anthropic 流即如此)绕开 `UpstreamError`,流式会原样转发、
  非流式经 `fold_sse_to_message` 原样回传。新增 `sanitize_upstream_error_payload`:
  **保留 `error.type`**(客户端按它判重试,动它就是改重试语义)**只换 `message`**,原文落日志。
- **`client_detail` 收成私有 + 删掉公开的 `with_client_detail`**(对抗评审三镜头一致):
  只留 `bad_request_visible` 一个入口。留着「把任意 String 登记为对外文案」的公开 API,
  fail-closed 早晚退化成靠自觉。

### Design Rationale

- **为什么不是逐个改字符串**:上游响应体是厂商控制的文本,今天是
  `USER_REQUEST_RATE_EXCEEDED`,明天可能是别的指纹。关键词黑名单迟早漏。所以对外**默认
  不透传上游任何文字**,要透传的必须显式登记 —— 白名单方向,不是黑名单方向。
- **为什么状态码一个不动**:429/502/529 的重试语义是客户端(Claude Code / SDK / NewAPI)
  的行为契约,改了会连带改变重试与「渠道判死」逻辑。本次只动 message。
- **为什么运维面不脱敏**:导号时「这个号是 invalid_grant 还是网络抖」全靠上游原文,
  一起脱敏等于把运维的眼睛蒙上。按「客户能不能打到这个端点」分界,不按错误类型分界。

### Notes & Caveats

- 客户端可见文案改为中性后,**客服口径要跟着变**:客户报「服务暂时不可用」时,凭
  `request_logs.error_kind` + worker 日志定位,不能再让客户把报错原文贴过来。
- `bad_request_visible` 登记的四处是有意为之(客户可自助):请求体超限 ×2、Anthropic
  报文解析失败、模型不支持 / 消息为空。新增登记前请自问文案里有没有厂商名、接口名、
  上游报文、账号标识。
- **`/health` 的返回体变了**:任何依赖它拿 worker/账号明细的外部脚本会失效(面板不依赖)。
  监控只看 `status == "ok"` 的不受影响。**worker 自己的 `/health` 未动**(内网 127.0.0.1,
  admin 面板与 `accounts/runtime` 都靠它)。
- **对抗评审驳回的两条**:①「按端点是否公网可达来划脱敏边界不够结构化,应拆成两个
  Router / 统一 PublicError renderer」—— 方向对,但那是一次协议层重构,不该搭在本次
  上线路径上;当前边界有注释 + 测试钉住。②「客户看到中性文案后无法与内部日志关联,
  应引入 request_id」—— 真缺口,但属新增特性,单列。

### 待办(评审提出、本次未做)

- 对外错误带一个不泄密的 `request_id`,与 router/worker/SSE 日志贯通,客服据此定位。
- `gw-claude-subprocess` 的本地请求校验文案(缺 messages / 无用户文本)目前退化成中性
  文案;该 provider 未部署,待启用前再逐条登记 `bad_request_visible`。

## [account-group-membership] - 2026-07-27

把 `group` 这一个词承担的**三件事**拆开。这是对同日上线的影子分组(见下一节)的
**替换**,不是叠加 —— 影子组那套整体删除。

### Features

- **新表 `account_groups(account_id, group_name, priority)`**:账号↔分组的 **N:M 成员边**。
  `priority` 挂在**边**上,不再挂在账号上 —— 同一个号因此可以在 A 组当主力(0)、
  在 B 组当兜底(100)。
- **`accounts.group_name` 语义收窄为「归属」**:哪个 worker 进程独占管理该号的运行态。
  列名与基数(N:1)都不变,只是不再兼任权限与排序。
- **调度器改按请求的成员视图**:`TierGuard` → `GroupView{rank}`。`eligible_ids` /
  `tiered_lru` / `best_available_higher` / `select_id` 里读 `e.priority` 的三处全部改读
  `view.rank_of(id)`;分层 LRU、会话亲和、向上迁移去抖(`MIGRATE_UP_DEBOUNCE`)语义不变。
  `acquire_where` 保留为「无视图」的薄封装。
- **自愈按视图收窄**:`heal_too_many_failures` 只复活**本组成员**。低价组一次全灭不再把
  正常组刚合法禁用的号一起复活并清零失败计数。
- **内网头简化**:`x-gw-tier`(档位区间 JSON)→ `x-gw-group`(组名)。worker 用组名从
  本地 membership 快照取视图;快照与账号集在同一把 `sync_lock` 内一起换。
- **router 按成员归属选 worker**:`pick_worker` 由「组名 == account_group」改成
  「owner 覆盖本组成员」(`group_owners()` + 15s TTL 快照)。一个组的成员因此**可以跨多个
  owner**,router 在其中做亲和/负载 —— 旧模型做不到。
- **admin 成员边端点**:`GET/POST /groups/{name}/members`、
  `DELETE /groups/{name}/members/{account_id}`、`POST /groups/{name}/members/bulk`
  (按 owner / subscription_title 批量加,220 个号手工点不现实)。
- **删除影子组全套**:`shadow_of`、`tier_min_priority`、`tier_max_priority`、`TierPolicy`、
  `AcquireError::TierExhausted`(→ 语义更准的 `GroupEmpty`)。

### Design Rationale

- **为什么非拆不可**:归属是物理约束(两个 worker 持有同一个号 → 并发翻倍 + rolling
  refresh_token 互相覆盖 → `invalid_grant` 报废),而权限与排序是策略。把策略焊死在物理
  约束上,导致「一个号进两个组」只能造影子组、「同一个号在两组里排序不同」只能拿全局
  priority 切区间 —— 而单侧区间在数学上表达不了「只要低优先的号」,才有了同日的
  `tier_min_priority` 补丁。**根因是建模,不是缺一个字段。**
- **2026-07-27 事故就是这个建模缺陷的代价**:低价档配成「只用小号」后,13 个小号全部
  上游过载、成功率 71%,却**无法溢出**到主力号(溢出顺序是全局的,改不了),只能硬报错。
  新模型下这就是一条成员边的事:低价组配 `小号@0 + 主力@100`,小号压满自然溢出。
- **回填保证逐条等价**:`INSERT OR IGNORE ... SELECT account_id, group_name,
  COALESCE(extra.priority, 100) FROM accounts` —— 每个号在原组、优先级沿用原值。
  `OR IGNORE` + 复合主键使其**幂等且只补不覆盖**:运维手工调过的组内优先级不会在
  下次重启被账号上的旧值冲掉。
- **查不到组名 → 空视图(503),绝不回落全量池**:回落等于让一个成员边还没同步过来的
  受限分组瞬间拿到全部账号,是静默提权。头**缺席**才回落全量池(未分组 key / 滚动升级
  窗口内的旧 router),这两种情况下的全量池本就是升级前的行为。
- **`delete_group` 护栏收紧**:原先只挡影子组,现在**任何**仍有 key 绑定的组都不许删。
  删组会把 `api_keys.group_name` 清空,而 router 把空组名回落到主组 —— 这些客户当场
  拿到全部账号。下线一个组的正确姿势是清空它的成员边(该组随即 503),或先迁走 key。

### Notes & Caveats

- **验收基线**:`cargo test --workspace` 802 绿,其中 **49 条既有 scheduler 测试零改动
  通过** = G0 行为不变的机械证明。两处变异(非成员回落默认排序、分层比较取反)均被
  多条测试抓红。
- **生产数据副本验证**(隔离栈 `/root/caio-next`,222 个号的 `refresh_token` 全部清空):
  迁移 222 条边、与旧模型**不一致 0 条 / 漏建 0 条**;G0 键 33 请求 100% 落 POWER,
  低价键 30 请求 100% 落 PRO MAX(两组账号集完全不相交);低价组第一层全灭后
  12/12 **溢出**到主力号而非硬报错。
- **`extra.priority` 仍保留**,但只作导入时的默认种子,调度不再读它。改它不影响任何
  已存在的成员边 —— 这是刻意的,避免"改了账号却不知道改到了哪些组"。
- **发布顺序不可颠倒:先 worker 后 router**。旧 worker 不认 `x-gw-group`,会忽略它并用
  全量池。更强的保险是**先上代码、后改成员边**:回填后的成员边与升级前等价,切换那一刻
  行为零变化。
- **未做(需要时再说)**:按组的并发预算 / 额度闸门。组内优先级只能保证"先用小号",
  不能保证"低价最多占主力号 N 个并发槽"——共享账号的并发仍是先到先得。

### 对抗评审(codex,三镜头)修掉的问题

首轮判 **REJECT**,4 条高危有共识。都是设计成立、实现有洞的那类:

- **worker 启动后 30 秒全组 503**:周期同步的首跳被跳过(注释"启动时刚加载过"只对
  账号成立),`group_views` 留空 → 头 30 秒每个带组名的请求都 GroupEmpty。**每次发版
  都会打出一个 30 秒不可用窗口。** 改成启动时同步装载成员边。
- **删组会毁掉同名 owner**:`delete_group` 只看绑在该组的 key,却把归属该组的账号
  `group_name` 清空 → 那些号成为孤儿,而**借用它们的别的组当场全量 503**,删的人还
  看不出因果。新增 `IsOwner` 拒绝;删组不再改动账号归属。
- **跨 owner 组静默降级**:成员跨 owner 时 router 只按会话数选 worker,被选中的 worker
  只看得见自己那部分成员,可能直接用兜底层而另一 owner 的主力号闲着。与其假装支持,
  改为写入侧拒绝(单条与批量都拒,批量**整批**拒不做部分写入)。
- **成员边与账号集撕裂发布**:两次独立查询分别发布,membership 成功而账号失败 → 新视图
  配旧账号;反过来 → **已撤销的成员边继续授权**(提权方向)。改成都读成功才发布、
  任一失败整轮跳过,且**先视图后账号**(撤销立即生效,新号至多短暂不可选)。
- **自愈触发条件被放宽**:原语义是"全池告罄"才自愈;按视图收窄后,一个只含单个坏号的
  小组每个请求都能触发一次自愈 → 该号反复复活,连续失败保护对它失效。加
  `whole_pool_exhausted` 前置闸门(复活范围仍按视图收窄)。
- **两个轮询器错配**:router owner 缓存 15s、worker 成员快照 30s,合法变更会有最长约
  30 秒的错误路由窗口。成员边变更后主动捅 worker `/sync`;owner 缓存查询挪出全局
  mutex(否则控制面抖动变成数据面队头阻塞);冷启动读库失败回落"组名即 owner"而不是
  空映射(空映射会让每个组都 503)。
- **缺少一步下线动作**:错误信息让运维"清空本组成员",却只有单条 DELETE(220 个成员
  要发 220 次,中途失败留半下线态)。新增 `DELETE /groups/{name}/members`。

**已知取舍(刻意保留)**:`last_selected_at` 挂在账号上、全 worker 共享,两个组共享同一
层里的号时会互相扰动**层内**轮转顺序。分层(哪层可见、先用哪层)才是隔离语义,已由成员边
严格保证;层内先用谁不影响可见性、不影响溢出顺序。要让层内 LRU 也按组独立,得给每个组
维护一份 `last_selected_at`,那会让"同一个号被多个组用"的负载彼此看不见,更容易把号打爆。
已用一条测试把该耦合显式钉住,避免以后被当成 bug 误修。

## [shadow-group] - 2026-07-27

### Features

- **影子分组(低价档)**:新增一类分组,它**不持有账号、不绑 worker**,而是复用源组的
  worker,只是**可见的账号更少**。典型用法:`GLOW.shadow_of = G0` + `tier_max_priority = 0`
  → 低价档只看得见 G0 里 priority=0 的主力号,看不见 priority=100 的兜底层。
  - `groups` 表加 `shadow_of` / `tier_max_priority` 两列(`ensure_column` 增量迁移)。
  - `authenticate` 的 SQL 加一次 `LEFT JOIN groups`,零额外查询把策略带到 `AuthenticatedKey.tier`;
    **策略每请求现读** → admin 改完下一个请求即生效(回退无需重启、无需发版)。
  - router 新增 `resolve_route`:影子组映射到源组再 `pick_worker`,并经内网头 `x-gw-tier`
    把守卫下发给 worker。`forward` / `forward_models` / `count_tokens` 三个入口全部走它。
  - scheduler 新增 `TierGuard` + `acquire_tiered`;`acquire_where` 保留为委托 `None` 的薄封装。
- **`AcquireError::TierExhausted`**(→ 503):档位内账号全冷却/占满时的专用错误。
- **删组保护**:`delete_group` 返回 `DeleteGroupOutcome`,对「被影子组引用的源组」和
  「仍有 key 绑定的影子组」返回 409。
- **转组保护**(对抗审查追加):把一个**仍有客户 key 在用**的普通组就地转成影子组会让
  那些客户无声降级(丢掉低优兜底层 + 改路由),`validate_shadow` 一并拒绝。反方向
  (`shadow_of=""`,低价档下线)不受影响 —— 那是解除限制,是标准回退姿势。
- **写入时兜底**(对抗审查追加):`create_group` / `update_group` 把「源组必须存在且本身
  非影子」合并进同一条 SQL 语句。admin 层是"先读快照再写"两次独立加锁,并发的
  `delete_group` 能在窗口里删掉源组 → 写出 `shadow_of` 悬空的行(整组静默 503)。
- **会话亲和按档位分命名空间**:worker 侧亲和键由请求内容派生、不含档位,两档共用一张表。
  正常客户的会话若钉在 priority=100 兜底号上,一个键名碰撞的低价请求会因守卫判定该号不合格,
  触发 `select_id` 的「primary 不可用 → 改选并当场转正、永不迁回」,**永久改写正常客户的
  钉扎**(上游前缀缓存冷启动)。档位头带上组名,worker 据此给亲和表分区。

### Design Rationale

- **为什么影子组不能有自己的 worker**:`instances.validate` 与 router 启动校验都禁止
  一个 account_group 绑多个 worker。两个 worker 加载同一批账号 → 并发上限翻倍 +
  各自刷新 rolling refresh_token 互相覆盖 → 账号 `invalid_grant` 报废。所以"一个号同时
  属于两个组"只能做成**同一 worker 内的可见性视图**:一份 entry、一个信号量、一个刷新写者。
- **守卫为何独立于 `supports` 谓词**:塞进 `supports` 的话,过滤后的空集会掉进
  `!supported_any` 分支 → `NoModelSupport` → **400**「订阅等级不足」。而档位耗尽是运行时
  状态、稍后可恢复,必须是 **503**;客户端(SDK/NewAPI)对 400 不重试,会误判成请求非法。
  错误码映射同时从二分 `if` 改成穷尽 `match`,逼后续新增变体做显式决策。
- **带守卫的请求不触发全灭自愈**:`heal_too_many_failures` 是全局的,会复活所有
  `TooManyFailures` 账号并清零失败计数。若低价请求也走这条路,正常组刚合法禁用的号会被
  反复复活,连续失败保护对所有档位一起失效。
- **守卫判据只收单调量**:`max_priority` 只在 admin 改配置时变,所以会话亲和不会因它反复
  重钉。`available_permits()` 这类抖动量**不能**进守卫——primary 一失格就会"改选并当场
  转正、永不迁回",会让会话反复换号、上游缓存冷启动,反而放大额度消耗。
- **删组为何要拦**:`delete_group` 把成员的 `group_name` 清成 `''`,而 router 的
  `resolve_group` 把 `''` 回落到 `default_group`(主组)。所以裸删影子组 = 低价客户**静默
  提权**成主组不受限访问,无任何告警。正确的下线姿势是 `PATCH {"shadow_of":""}`。

### Notes & Caveats

- **默认完全 inert**:DB 里没有 `shadow_of != ''` 的行时,`tier` 恒 `None` → 恒不发头 →
  `acquire_tiered(.., None)` 与旧 `acquire_where` 逐字节等价(有测试
  `guard_none_matches_acquire_where_exactly` 机械保证)。开关是一行 DB 数据,不是代码分支。
- **发布顺序不可颠倒:先 worker,后 router**。新 router + 旧 worker = 发了头没人认 =
  低价流量无守卫打主力号。更强的保险是代码先上线、影子组后建。
- **schema 双向兼容**:所有 SELECT 都是显式列名,旧二进制读新库正常 → 回滚二进制不需要
  回滚 schema。
- **共享账号 = 共享故障域(本次范围外,需知情)**:低价流量消耗的是主力号同一份月额度;
  它触发的 `QuotaExhausted`(永久禁用)/ `TemporarilyBlocked`(1h 冷却)会同时把号从正常组
  踢掉;**封号风险无法隔离**(账号/machineId/profileArn/出口 IP 都共享)。本次只做分组与
  优先级可见性,未做额度预算、故障归因分层、并发预留。

## [upstream-overload] - 2026-07-25

### Features

- **新增 `UpstreamErrorKind::Overloaded`——上游模型级过载不再当成账号故障。** 三条策略同时成立:
  - **不惩罚账号**(`spares_account_health()`):不计 `failure_count`、不触发 `TooManyFailures` 禁用。
  - **不换号**(`worth_switching_account() = false`):容量是模型级的,换号打的还是同一个模型端点。
  - **同号退避重试**(`chat_with_overload_backoff`):250 / 750 / 2000ms,各叠 0~40% 抖动。
- **模型级过载窗口**:收到显式 `MODEL_TEMPORARILY_UNAVAILABLE` 后 60s 内,该模型的通用 5xx
  (`reason:null`)也按过载处理。窗口按模型隔离,不跨模型泄漏。
- **对外映射 529 `overloaded_error`**(Anthropic 官方过载语义)。状态码收敛到 `upstream_status()`
  单点,响应体与请求日志 `status_code` 同源。
- 测试:gw-core 49 / gw-kiro 447 / gw-app 184 全绿,新增 10 条。

### Design Rationale

- **事故实况(2026-07-25)**:Kiro 的 opus-5 容量抖动被记进账号连续失败计数,**35 秒内禁光 7 个
  健康号**并触发全灭自愈。禁用对**所有模型**生效,于是 opus-4-6 / sonnet-5 一起被连带打挂。
  上游报文两种:`{"reason":"MODEL_TEMPORARILY_UNAVAILABLE"}` 与 `{"reason":null}`。
- **为什么不换号**:实测 177 次上游 500 靠换号只救回 19 次(19%)。换号还有两笔实打实的成本——
  白烧另一个号的配额,以及丢掉会话 cache 亲和(实测一次 opus-5 请求 `cache_read` 达 10.7 万 token,
  换号全部重算)。
- **为什么这不违反 2026-06 防雪崩约束**:同号退避的**爆炸半径 = 1 个号**,比原换号重试(默认 2 个号)
  更小。`max_switch_attempts` 一字未改。
- **为什么通用 5xx 用"窗口"而不是直接全归过载**:重分类必须有上游显式信号背书。实测 176 条通用 5xx
  中 **84.7% 与显式过载落在同一分钟**(35 个有 5xx 的分钟里 19 个两种并存),故以显式信号为真相源;
  窗口外的通用 5xx 一律仍是 `ServerError`(仍换号、仍记账号失败),不靠猜。
- **窗口取 60s**:事故是分钟级成簇的,60s 刚好覆盖一簇。更长会把上游真内部错误也长期误判成过载,
  更短则簇内空隙漏掉、退回逐个禁号。
- **抖动是必需的**:并发请求会在同一波容量抖动里齐刷刷失败,无抖动会同步重撞,把重试变成新尖峰。
  熵取 `uuid` v4 的一个字节(workspace 已依赖 uuid+fast-rng,不为此引入 `rand`)。

### Notes & Caveats

- **`report_failure` 刻意穷举 kind 而非用 `spares_account_health()` 做守卫**:守卫会让新增 kind 悄悄
  落进某个分支,穷举则编译不过、强迫做决策。两者一致性由测试
  `spares_account_health_matches_no_penalty_arms` 锁住。
- **校正只作用于首包前的错误**。事故中观测到的 276 次 5xx 全部 `ttfb=NULL`(都在首包前);流中途
  冒出的 5xx 仍走 `finish_response` 原路径,不做窗口校正。
- **过载窗口仅内存**,重启即清、重新学习(与 `model_unavailable` 同策略)。
- **失败请求的上游报错仍未落库**:`response_payload` 只在 `Outcome::Ok` 时写,失败一律空串,原因只在
  `tracing::warn!` → docker 日志会轮转。所以 admin UI 的"失败"依旧不显示原因,排查仍需
  `docker logs caio-worker0`。这是本次**未**修的独立改进项。
- 事故期间曾把 `max_failures` 从 5 热调到 30 止血(admin settings,30s 生效);本次上线后**已调回 5**。

## [opus-5] - 2026-07-25

### Features

- **接入 Claude Opus 5**(上游 Kiro 已支持,官方 2026-07-24 发布):对外 `claude-opus-5`,上游 `claude-opus-5`。
  - `KIRO_MODELS` 权威表新增一行(1M 窗口、`supports_thinking=true`、`identity_short="Opus 5"`、无 `dated_alias`)。
    `/v1/models` 目录、`map_model` 路由、`get_context_window_size` 窗口、`requested_model_identity` 身份短名
    **全部从该行自动派生**,无需改动其它位置。
  - `map_model_substring` 的 opus 分支新增 `opus-5`/`opus5` 邻接匹配(置于最前),覆盖未列名异名写法
    (如 `claude-opus-5-0`、`openrouter/claude-opus-5-preview`);窗口兜底同步给 1M。
  - 前端 `RequestLogsPage.contextWindow()` 补 `opus-5`。**顺带修复 sonnet-5 的遗漏**——上次接 sonnet-5
    只改了后端,前端这份硬编码窗口表没跟上,导致 admin UI 里 sonnet-5 的上下文% 一直按 200k 计(实际 1M)。
- 端到端实测通过:`claude-opus-5`(非流式)、`claude-opus-5-thinking`(返回 thinking+text 块)、
  异名 `claude-opus-5-0`(子串兜底),对照 `claude-opus-4-8` 无回归。

### Design Rationale

- **5 系命名与 4.x 不同,不能套用短横转点号规则**:4.x 是对外 `claude-opus-4-8` → 上游 `claude-opus-4.8`,
  而 5 系上游 modelId 就是主版本裸名 `claude-opus-5`(无 `x.y` 点号)——与 2026-07-02 接入的 `claude-sonnet-5`
  同规律。历史上 sonnet-4.8 因猜错上游 id 吃过 `INVALID_MODEL_ID`,故本次上线前以真实账号实测确认。
- **子串兜底必须锚定 `opus-5` 邻接串**,不能用裸 `contains("5")`:否则 `claude-opus-4-5`/`4.5` 会被误吞。
  该分支置于 opus 各版本判断最前,因 opus-5 的写法不含 `4-x`,与下方分支天然互斥(测试已钉住这组边界)。
- **不把 `claude-opus-5-0` 加进公告表**:官方 Opus 5 是无日期快照,id 仅 `claude-opus-5`;公告一个不存在的
  id 会误导客户端(且违反"公告面 ⊆ 可服务面"的既有不变量)。该写法经子串兜底仍可正常路由,鱼与熊掌兼得。
- 计价无需改动:`pricing.rs` 按 `contains("opus")` 归入 OPUS 档(5/25/0.5/6.25),与 Opus 5 官方价恰好一致。

### Notes & Caveats

- **前后端各有一份窗口表**,是既有的结构性重复(后端 `model_map.rs` / 前端 `RequestLogsPage.tsx`)。本次两处都改了,
  但下次加模型仍会踩——sonnet-5 就是漏在前端。值得后续让前端改从 `/v1/models` 的 `context_length` 取值,消除重复。
- **线上代码与本地 git 已严重漂移**:线上为严格超集——独有 `crates/gw-dario` 整个 crate(dario/Claude-OAuth 通道)、
  `websearch.rs`、`tool_repair.rs`、`document_name.rs` 等 14 个文件,另有 53 个文件内容领先。
  ⚠️ **任何从本地 rsync/部署到线上的操作都会抹掉 dario 通道**,回流前必须先把线上漂移入库。
- 回滚:`docker tag claude-all-in-one:rollback-20260725 claude-all-in-one:local && docker tag ... :exp && docker compose up -d --no-build`。


## [kiro-api-key-credential] - 2026-07-20

### Feature —— 支持官方 Kiro API Key(ksk_)作为上游账号凭据

**背景**:此前只支持 social/IdC(均需 refresh_token 运行时换 access_token)。Kiro CLI 2.0 headless
模式引入官方 API Key(`ksk_`,app.kiro.dev 生成),长期有效、无刷新。本版把它接成一类新的上游凭据。

- **鉴权(实测确定)**:apikey 号发包带 `Authorization: Bearer ksk_...` + **`TokenType: API_KEY`** 头。
  该头**服务端强制要求**——缺它则 ksk_ 被当普通 OAuth token,报 400「profileArn is required」;带它则
  免 profileArn,服务端按 key 自身账号解析。实测三个端点(runtime.kiro.dev / q / codewhisperer)均放行,
  **caio 现用的 `runtime.{region}.kiro.dev/generateAssistantResponse` 直接可用**,主链路无需改端点。
- **凭据判定**:统一以「`kiro_api_key` 非空」为准(`machine_id::is_api_key_credential`,提为 pub 复用),
  不只看 `auth_method` 标签(防误配)。bearer 由 `headers::bearer_token` 按类型取真值:apikey→`kiro_api_key`,
  social/IdC→`access_token`(故 apikey 号仅需一个字段,不必镜像 access_token)。
- **旁路 OAuth 专属步骤**:apikey 号 `has_fresh_token` 恒真(不触发刷新)、`refresh_auth` 空操作、
  `discover_profile_arn`/`ensure_profile_arn` 短路(不发 ListAvailableProfiles)、`resolve_profile_arn`
  永不注入 profileArn。订阅档位由后台 getUsageLimits 回填(实测订阅 KIRO POWER,不含 FREE → 放行 opus)。
- **导入 / 配置**:accounts.yaml 支持 `auth_method: api_key + kiro_api_key`;JSON 导入识别 `kiro_api_key`/
  `apiKey` 字段及裸 `ksk_` 字符串;新增 admin `POST /accounts/import-apikeys` + 前端「导入 → 官方 API Key」
  粘贴列表批量建号(复用既有建号/去重/逐号只读验活链路)。

### Design Rationale
- **bearer 按类型取真值,而非镜像 access_token**:apikey 号可能只从 YAML/admin 建号、只带 `kiro_api_key`;
  强制镜像一份密钥既冗余又易漏(YAML seed / admin create 各是一条路径)。单一事实来源 = `kiro_api_key`。
- **`TokenType: API_KEY` 是官方 CLI headless 的真实客户端形态**(非 IDE 形态),属合法官方指纹,且服务端强制。

### Notes & Caveats
- apikey 号收 403 会走一次同号刷新兜底(refresh_auth 对 apikey 空操作),retry 同 key 一次后由调度层换号/上报,
  行为有界、无网络放大。
- 端到端已验证:非流式 / 流式 / opus / 配额(KIRO POWER)/ 批量导入端点均实测通过。


## [ingress-body-limit] - 2026-06-17

### Features

- **放开入站请求体上限(修大图片/PDF 在入口被 413 闷死)**:客户端含大量 base64 图片/PDF 的请求体常 >2MB,
  axum 0.8 的 `Bytes`/`Json` 提取器默认上限仅 **2MB**,超了在 handler 执行前就被框架直接 **413**,
  请求根本到不了业务逻辑——**不入库、后台不可见**(线上实测 `status_code=413, bad response status code 413`)。
  - `SystemConfig` 新增 `max_request_body_bytes`(默认 **16MB**,`0`/缺省回落默认),`effective_max_request_body_bytes()` 取有效值。
  - router(`/v1/messages` 等 + admin 全路由)与 worker(`/v1/messages`)**两处入站咽喉**各挂 `DefaultBodyLimit::max(..)`——
    缺一处仍会在另一侧 413。router 的 layer 挂在 `nest("/admin/api")` **之后**,使大 JSON 导入也一并放开。
  - 两进程启动时各打印生效上限,便于核对配置一致(只重启一侧导致漂移时,日志即现形)。

### Design Rationale

- 取 **16MB** = 出站 6.3MB 护栏(`DEFAULT_MAX_BODY_BYTES`,对齐 Kiro 上游 ~7.3MB 硬限)的 ~2.5×:给当前轮 +
  可被 `shed` 裁掉的历史媒体留余量;放开入口只是让大请求**抵达**内容感知护栏(裁剪/压缩/清晰报错)而非被框架裸 413。
- 用有界 `DefaultBodyLimit::max()` 而非 `disable()`:网关 `:38991` 对外、入口提取在鉴权前完成,无界缓冲是 DoS 面;
  16MB 较旧 2MB 已 8× 决定性解除闷死,同时把缓冲面控制在合理范围,需更大可在 `system.yaml` 显式上调。
- 该值是**启动期参数**(axum `DefaultBodyLimit` app 构建期固定),故意**不进** `SystemSettings` 热调 overlay,
  避免「前端改了不重启不生效」的误导;改动需同时重启 router + worker。

### Notes & Caveats

- **PDF 结构性上限**:文档无法像图像那样压缩(base64 原样透传),且 `shed` 只裁历史不裁当前轮 → 单个原始 >~4.5MB 的
  PDF(base64 ~6.16MB + 脚手架顶破 6.3MB)在当前轮仍会被 `gw-kiro` 本地 `BadRequest`,无法自动补救——需产品侧引导拆分/缩小。
- **待硬化(本次未做)**:router 入口缓冲在鉴权前完成,理想应加 鉴权前置 / Content-Length 预检 / 并发护栏;
  当前以「有界默认 + 可配」缓解。`tool_result` 内嵌图(browser 截图)目前不走压缩,满体量撞 6.3MB 墙,可后续纳入压缩。
- 入站放开**不绕过**出站 6.3MB 护栏:二者是上下游两道独立闸门(入站 Anthropic body vs 出站 Kiro body)。

## [anti-ban-retry-cascade + egress-gateways] - 2026-06-17

### Features

- **重试雪崩止血(防大面积封号)**:`messages()` 重试循环改用 `UpstreamErrorKind::worth_switching_account()`
  + 新增硬上限 `max_switch_attempts`(默认 **2**,热调)。一个失败请求最多波及 2 个号、不再走遍全组。
  - `worth_switching_account()`:`BadRequest / EmptyResponse / TemporarilyBlocked` 一律**不换号**。
  - `error_map`:403 拆分——含封禁标记(`is_account_suspended`,"suspend")→ `TemporarilyBlocked`;否则 → `TokenInvalid`。
  - 新增 `DisabledReason::TemporarilySuspended` + `suspended_cooldown_secs`(默认 **3600=1h**,热调):封禁号较长冷却、面板标 `temporarily_suspended`,不每 5min 重戳。
  - `token.rs` 刷新错误也认 403+suspend → `TemporarilyBlocked`,封禁号不再被永久禁死。
- **出口网关(美国多 IP)+ 上号可选**:
  - `SystemSettings.egress_pool`(每行一个代理 URL = 一个网关)+ 设置页可配;新增 `max_switch_attempts`/`suspended_cooldown_secs` 热调项。
  - 导入/新建账号对话框新增「出口网关」下拉:**直连 / 自动均衡(最少使用) / 指定网关**(`EgressPicker` + body `egress` 字段,按索引解析,密码不经前端)。
  - `POST /admin/api/accounts/rebalance-egress`:把现有号按最少使用回填到网关池。

### Design Rationale

- 根因:旧循环上限=账号总数且从不调 `worth_switching_account()`,封号 403 被错分 `TokenInvalid` → 刷新失败 → 换号把同一(被封内容/高频)请求扩散到健康号 → 雪崩封全池。硬上限 + 内容/封禁类不换号双管齐下,把单请求爆破半径从「全池」降到 2。
- `TemporarilyBlocked` 走冷却自愈而非永久禁用:封禁多为临时,1h 后自愈再试、仍封则再冷却,既不扩散也不每 5min 重戳产生异常指纹。
- 出口选择按**索引**回传而非明文 URL:后端响应已掩码代理密码,前端拿不到真值,选索引由后端解析,杜绝密码经接口往返。

### Notes & Caveats

- `is_account_suspended` 仅匹配 "suspend"(覆盖 `TEMPORARILY_SUSPENDED`);上线后需用真实封禁 403 body 复核标记串。即便标记不中,`max_switch_attempts=2` 仍是兜底。
- Vultr 附加 IP(66/140 段)经实测进出均不通(元数据挂着但 Vultr 未路由),3-IP 需面板 re-attach;当前单主 IP 美国出口可用。

## [tool-repair] - 2026-06-16

### Fix —— 工具参数双重编码防御性修复（tool_repair）

**线上取证**：部分上游模型（Kiro 上的 Opus）偶发把本应是 JSON array 的工具参数序列化成 JSON
**字符串**——1027 次 `AskUserQuestion` 调用中 223 次把 `questions` 编码成字符串，客户端按
`input_schema` 校验拒收：`The parameter questions type is expected as array but provided as string`。
反代逐字透传模型产出的 tool input，本身无 bug，但这是唯一可控点。

- **新增 `crates/gw-kiro/src/tool_repair.rs`**：转换请求时从每个工具 `input_schema` 提取「顶层
  type 为 array/object 的字段名集合」建表（`tool_repair_fields: 工具短名 → 字段集合`，键与上游回显的
  wire 短名同源）；收尾组装 tool input 时，若字段当前为字符串且能解析成 array/object 就解包替换。
- **流式（chat.rs）**：命中字段的工具走「缓冲不发 → `close_open_tool` 在 `content_block_stop` 前
  只发一条 `repair_str` 修复后完整 `input_json_delta`」（无双发）；`finish()` 兜未显式 stop 的截断。
  非修复工具逐帧透传。非流式经同一 BlockTracker 后 `fold_sse_to_message` 折叠，天然覆盖。

### Design Rationale

- 安全边界（绝不破坏正常输入）：仅 schema 声明 array/object 的顶层字段、当前值为字符串、且可解析成
  对应类型时才替换；标量字符串、非 JSON 串、已是数组、半截 JSON 一律原样保留（11 项单测锁定）。
- `array_object_fields` 读**原始** `input_schema`（在 normalize/多模态降级之前），故 schema 降级只改
  发往上游的副本、不影响修复表——按客户端真实期望解包。

### Notes & Caveats

- 缓冲对所有含顶层 array/object 字段的工具生效（input 不再增量流式、整段缓冲到 stop；协议正确性无损）。
- 仅解顶层一层；nullable/$ref/anyOf 包裹的 array、嵌套双编码不识别（漏修不误伤）。
- 既有缺陷（与本修复无关、不受影响）：响应侧未消费 tool_name_map，名长 >63B 的工具向客户端泄露短哈希名。

## [agent-continuation-cache-ab] - 2026-06-15

### Fix(实验开关) —— 恢复稳定 agentContinuationId,修复真实缓存全 miss

**背景/根因(线上取证)**:caio 真实 Kiro 前缀缓存命中 **0%**(1985/1985 success),credit **+49% vs
kiro.rs**(opus-4-8 1.37 vs 0.92/req);而 kiro.rs 同上游同期 **~43%** 命中。逐层对比两套源码:caio
发的 wire 看似完美可缓存(稳定 conversationId、history 前缀逐字节相同、reminder 已剥),唯一差异是
2026-06-13 caio **删掉了** `agentContinuationId` + `agentTaskType="vibe"`,理由"kiro.rs/static_flow
都不发"——经核对**证伪**:kiro.rs 生产一直在发稳定值(其代码实测注释:稳定 → metering 降 ~36%),
caio 自身 2026-05-24 命中 A/B 基线也含此字段(6-13 才删)。删除疑为当时 reminder-leak bug 混淆的误判。

- **恢复** `derive_agent_continuation_id`(= `SHA256("agent-continuation:"+conversationId)` 前 16 字节
  排 UUID,**逐字节对齐 kiro.rs**;golden-vector `621628ff-…` 锁死)。
- **新实验开关 `agent_continuation`**(默认**关**):走与 tools_in_prefix/cache_point 同款 RwLock 热控
  (config.rs→cache_point.rs→apply_hot_settings),可经**设置面板/API 热翻**——生产做**可逆 A/B**:
  默认关 = 部署零 wire 变化(`Option` 字段 `skip_serializing_if` 完全省略),开启 = 复刻 kiro.rs proven
  配置(稳定 conversationId + 稳定 agentContinuationId + vibe)。
- 附挂逻辑抽成纯函数 `with_agent_continuation_metadata(state, enabled)`,便于直接测两分支。
- 前端设置面板加该开关 + 中英文案(指向请求日志「真」命中列)。
- **设计取舍**:做成默认关的可逆开关(而非直接改默认)——历史测量多次被混淆,用真实流量 A/B 定论;
  必须配稳定 conversationId(本 crate 已保证),否则每轮新值反而 miss(kiro.rs 实测)。
- **对抗审查(Codex×2)**:修 4 项 medium——补 enabled-path 测试(纯函数双分支 + 序列化边界双向
  断言:关→JSON 省略两 key、开→含两 key 且 = golden vector)、golden-vector 锁死、OnceLock→RwLock
  热控(消除"重启才能翻、不可热回滚")。1 项 high 为误报(glob import,编译已过)。
- **测试**:gw-kiro 357 + workspace 560 全绿;admin-ui tsc+build 通过;clippy 净。

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
