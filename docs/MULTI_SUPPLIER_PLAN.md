# 自动补货:多供应商架构

> 2026-08-05。目标(用户原话):**「两边互补,保证我总是有低价车可用」**。
> 已实测第二家 `kiroapp.io` 并实买 1 个号验完全部关键项。
>
> **本版是对抗审查(gpt-5.6-terra,Skeptic/Architect/Minimalist 三视角)后的重写。**
> 首版被判 REJECT,主要发现见 §3 —— **多数高危项不是本设计引入的,是它脚下的地基
> 本来就不安全**,而多供应商会放大每一条。因此新增 P0 资金安全期。

---

## 一、实测结论(全部有据)

### 1.1 价格:EU 档只便宜 1.8%,不是「低很多」

报价单位是 **credits 不是钱**,折算口径在 `GET /api/me/recharge` →
`{"credits_per_usd":7,"currency":"usd"}`。用 drop 的实际有效汇率
(¥21.01 实扣 ÷ $2.91 报价 ≈ 7.22)统一:

| 货源 | 报价 | 折 USD | 折 CNY | 对比 |
|---|---|---|---|---|
| drop.kiro.ss | $2.91 | $2.91 | **¥21.01**(36 单实扣均值) | 基准 |
| kiroapp `eu-central-1` | 20 credits | $2.86 | **¥20.63** | 便宜 **1.8%** |
| kiroapp `us-east-1` | 30 credits | $4.29 | **¥30.94** | 贵 **47%**,且 `stock_us` 长期 0 |

**这家实际只供 EU 档。** 按 15 个/天,EU 档一天省 ¥5.7。价差不是接入理由。

> ⚠️ **换算是三段而不是两段**:`credits → USD → CNY`。首版文档在这里栽过一次 ——
> 把 680 credits 写成 `balance_cny: 97.1`(那是**美元**,人民币是 ¥701),低估 7 倍。
> 这个值若进了余额闸,会在还有 ¥700 时判定「钱不够,不买」。**所有金额一进引擎就必须
> 是 CNY,折算只许发生在适配器里** —— 这条规矩正是为了让这类错误不可表达。

### 1.2 号的质量:与 drop 等价,额度上限不 binding

买到的号实测(只读 `Get-Usage-Limits`,未发任何推理包):

```
subscriptionTitle: "KIRO POWER"   type: Q_DEVELOPER_STANDALONE_POWER
usageLimit: 10000 credits         currentUsage: 0
List-Available-Models: 17 个(完整目录,不是 IdC 号那 8 个)
```

**但 10000 这个上限根本用不到。** 15 个自动买入号的实测产出:

| 分位 | p10 | p25 | p50 | p75 | p90 | max |
|---|---|---|---|---|---|---|
| 产出积分 | 324 | 560 | **1122** | 1411 | 1869 | 3048 |

**零个号超过 9000**,最大只到 3048。绑定约束是**墙上时钟死亡**(服务时长中位 55 分钟)。

> **推论:两家的号价值等价,唯一变量是价格和可得性。**
> 所以选家不需要给不同供应商的号做质量加权 —— 但这**不等于**可以纯按价格排序,
> 见 §4.3。

### 1.3 真正的接入理由:缺货

近 7 天 `drop 无库存` **854 轮**,全部是「水位已破、闸门全过、就差有货」:

| 日期 | 缺货轮数 | 折合(轮询 30s) |
|---|---|---|
| 2026-08-03 | 157 | ~1.3 小时 |
| 2026-08-04 | 564 | **~4.7 小时** |
| 2026-08-05(至查询时) | 133 | ~1.1 小时 |

号的寿命中位数 52 分钟,缺货 4.7 小时 = 那天近 5 小时靠贵号池兜底
(¥0.068/积分,是 ksk_ 号中位 ¥0.0211 的 **3.2 倍**)。

### 1.4 EU 区延迟:已实测,结论有利

手工导入 `kiro-apikey-cbc42a84424f`(`G0@0`/`GECO@0`/`GLOW@0`,并发 100,排队开),
30 分钟内承接 **90 个 `claude-opus-5` 请求全部 200**:

| 号 | 区 | n | 平均 duration | 平均输出 tok | **ms/输出 tok** |
|---|---|---|---|---|---|
| `cbc42a84424f` | eu-central-1 | 90 | 5432ms | 230 | **23.6** |
| `f0b9c982b065` | us-east-1 | 532 | 13891ms | 482 | **28.8** |

同模型同窗口,EU **不比 US 慢,按每输出 token 还快约 18%**。
prompt cache 正常(`cache_read_tokens` 稳定 4500–5600)。

### 1.5 质保盯的是早夭尾部,但兑现机制未验证

p10 只产出 324 积分 → ¥0.065/积分,和贵号池一样贵。**约 10% 的购买接近全损。**
`warranty_minutes: 10` 正好盯这条尾巴,期望价值 ≈ ¥2/号,是价差(¥0.38)的五倍。

⚠️ 但探测里**没有任何 refund/warranty 端点**(全 404)。**不要在拿到证据前把它写进
任何数据结构** —— 首版把 `warranty_secs` 加进了 `Shelf`,却没有任何消费方,
只会制造「已经有质保保护」的错觉。本版已删。

---

## 二、kiroapp 的两个硬差异

### 🔴 `max_total_cny` 被完全忽略 —— 限价没有服务端兜底

实测发 `max_total_cny: 0.01` 买 20 credits 的号,**照常成交、照扣 20**:

```json
{"purchased":1,"remaining":680,"unit_price":20,"total_debit":20,"replayed":false,
 "keys":[{"key":"ksk_…","region":"eu-central-1","price":20,…}]}
```

drop 的价格保护是**服务端**做的(超限价返 409),kiroapp 没有。

**客户端的两道闸拦不住这一单。** quote 与 purchase 之间对方可以涨价,前置检查基于旧价、
事后复核发生在**不可逆扣款之后**,熔断只能阻止后续单。这不是实现细节能补的洞。

**唯一真正有效的对策是把敞口锁死在钱包余额里**:kiroapp 钱包**不要多充**。
单次事故的最大损失 = 当时的钱包余额。当前 680 credits(≈¥701)可以接受;
**不要为了「够用一个月」把它充到几千**。这条比任何代码都管用,写进运维纪律。

### 🔴 号绑死 region

```
management.eu-central-1.kiro.dev/Get-Usage-Limits  → 200 KIRO POWER
management.us-east-1.kiro.dev/Get-Usage-Limits     → 403 {"message":"Invalid token"}
```

而 caio 默认 region 就是 `us-east-1`。**P1 已修**(见 CHANGELOG `[restock-region]`)。

### 🟡 两处静默差异

- **错误体形状**:kiroapp 给 `{"error":"中文串"}`,drop 给 `{"error":{"code","message"}}`。
  现有 `ErrBody` 解析失败后被 `.ok()` 吞掉 → 运维看到没有原因的 HTTP 码。
- **竞争失败状态码**:drop 一律 409;kiroapp 是 403 / 404。
  ⚠️ **但不能靠裸状态码归类**:403 既可能是「余额不足」(常态,该 Conflict),
  也可能是「key 失效」(真故障,该 Fault)。必须按 `error` 字符串的**白名单**判定,
  **未知形状一律 fail-closed 归 Unknown**,宁可停下来等人看,不要猜。

---

## 三、审查发现:地基本来就不安全(P0 的由来)

以下五条**现在就在生产上跑着**,与多供应商无关,但三货架顺延会把每条的窗口拉长。
全部经代码核实:

| # | 问题 | 证据 | 后果 |
|---|---|---|---|
| 1 | `POST /restock/buy-now` **完全不抢租约**,直接 `run_once(true)` | [admin/restock.rs:379](../crates/gw-app/src/admin/restock.rs#L379) | 手动点两次、或手动与后台并发 → 各读到同一个 `spent`,各自下单扣款 |
| 2 | 租约 TTL 最小 **30s**(`interval.clamp(10,3600) × 3`),单轮超时却是 **120s**;购买前不复验持有,也无 fencing token | [mod.rs:105](../crates/gw-app/src/restock/mod.rs#L105) / [mod.rs:145](../crates/gw-app/src/restock/mod.rs#L145) | 第 31 秒另一个 router 接管并下单,原持有者恢复后继续下单 → 重复扣款 |
| 3 | **网络超时一律标 `failed`**,而 `reconcile_pending` 只扫 `pending` | [engine.rs:585](../crates/gw-app/src/restock/engine.rs#L585) / [gw-store:1031](../crates/gw-store/src/lib.rs#L1031) | 上游已扣款但响应丢失 → 订单变 `failed` → **永久失去对账机会**,钱和 key 都成孤儿 |
| 4 | 日预算 `restock_spent_since` 只统计 `purchased`/`imported`,**不统计 `pending`** | [gw-store:1077](../crates/gw-store/src/lib.rs#L1077) | 120s 超时中止后订单停在 `pending`,下一轮把它当没花过钱,用新单号再买一次 |
| 5 | `reconciled` 一置 `true` **永不再对账** | [mod.rs:119](../crates/gw-app/src/restock/mod.rs#L119) | 进程不重启的话,运行期产生的 `pending` 永远不会被重放确认 |

**这五条必须先修。** 在一个「超时就丢失对账、手动入口不受互斥、预算漏算在途」的底座上
再加一家供应商,等于把重复扣款的概率乘上货架数。

---

## 四、架构:「总是有低价车」拆成三层

目标不是一个功能,是一个**结果指标**:`request_logs` 里 ksk_ 号承接的请求占比。

⚠️ **这个指标目前测不了跨天**:`REQUEST_LOG_CAP = 10_000` ≈ **1.5 小时**
([worker/mod.rs:2500](../crates/gw-app/src/worker/mod.rs#L2500))。
P4 必须把它**按小时物化到一张小表**,否则上线前后没法比较,也就无法证明达标。

| 层 | 失效模式 | 机制 | 状态 |
|---|---|---|---|
| **L0 资金安全** | 重复扣款 / 丢失对账 | 租约收口 + 不确定态可对账 + 预算算在途 | **P0(新增)** |
| **L1 供给** | 一家没货买不到 | 多货架 + 缺货转移 + 灰度爬坡 | P2–P3 |
| **L2 库存** | 有货没及时买 / 早夭 / **钱花完了** | 水位 + 判活 + 余额守恒 | 部分已有 |
| **L3 调度** | 有号但请求漏到贵池 | tier-hold | 已实现,合并上线 |

### 4.1 货架(Shelf)是最小单位

kiroapp 的 eu/us 是**两个独立的价格和库存**,选家时必须能分别参与排序:

```rust
/// 一个可下单的货架 = 供应商 × 区域。
pub struct Shelf {
    pub supplier_id: String,
    /// **供应商自己的货架标识**(kiroapp 是 `us` / `eu`;drop 没有区域概念 = 空串)。
    /// 它进配置、进面板、进 purchase 请求体。
    pub shelf_id: String,
    /// 该货架发出的号要落进 `extra.region` 的 **Kiro 服务区**(如 `eu-central-1`)。
    ///
    /// ⚠️ **与 `shelf_id` 是两个命名空间,绝不能混用。** kiroapp 的请求参数是 `eu`,
    /// 而号真正能通的端点是 `management.eu-central-1.kiro.dev`。把 `eu` 当服务区
    /// 写进 extra,就退回到 P1 修掉的那个全量 403。适配器负责映射并校验。
    pub account_region: String,
    pub stock: i64,
    /// **已归一到 CNY**。折算只许发生在适配器里(见 §1.1 的教训)。
    pub unit_price_cny: f64,
    pub max_per_order: i64,
}
```

### 4.2 购买回执:必须四件套,且返回枚举而不是 `Result`

首版的 `purchase() -> Result<Vec<BoughtKey>, BuyFailure>` **实现不了它自己的要求** ——
事后限价复核要成交价、记账要余额、幂等要 `replayed`;而超限价时返回 `Err` 会把
**已经买到的 key 丢掉**,让「已扣款但没上号」从意外变成必然路径。

```rust
/// 一次购买的完整回执。少任何一个字段,引擎都无法闭合对账。
pub struct Receipt {
    pub keys: Vec<BoughtKey>,
    /// 实际扣款(已归一 CNY)。对方给了 `total_debit` 就用它,没给就用余额差兜底。
    pub debited_cny: f64,
    pub balance_after_cny: f64,
    /// 对方是否认出了同一个 `client_order_id`(kiroapp 有 `replayed` 字段)。
    /// 重放确认时必须为 true,否则说明对方没去重 —— 那就是第二次扣款。
    pub replayed: bool,
    /// 实际成交数量。与请求数不符即拒收转人工(见 §4.5)。
    pub purchased: i64,
}

/// **返回枚举不返回 `Result`** —— 因为「成功」和「失败」不是二分,
/// 中间还有一个必须显式处理的「结果未知」。
pub enum BuyOutcome {
    Ok(Receipt),
    /// 竞争失败:缺货 / 涨价 / 余额不足 / 订单号冲突。**确定没扣款**,可换下一个货架。
    Conflict(String),
    /// 确定失败且**确定没扣款**(400 参数错、401 鉴权)。计该家熔断。
    Fault(String),
    /// **结果未知**:超时、连接中断、响应无法解析、错误体形状不认识。
    /// 可能已扣款 → 订单**停在 `pending` 等对账**,且**本轮立即终止,不许顺延到下一个货架**。
    Unknown(String),
}
```

### 4.3 选家:缺货转移优先,价格其次,新家必须爬坡

首版写「按 `unit_price_cny` 升序」。**这是错的** —— EU ¥20.63 < drop ¥21.01,
于是 kiroapp 会赢**每一单**,一家只买过 1 个号、质保没验证过的供应商直接变成主供应商。
这跟本文自己的接入理由(冗余,不是价差)正面冲突。

正确的顺序:

1. 并发拉所有启用家的报价(一轮延迟从 N×4s 压到 4s)
2. 摊平成货架,过滤:库存 > 0 / **该货架单价过单位成本闸** / 该家未熔断 / 该家余额够
3. 排序键 = `(爬坡档位, unit_price_cny)` —— **爬坡档位优先于价格**
4. 取第一个下单;`Conflict` 顺延下一个(最多 3 个),`Fault` 记该家熔断并停本轮,
   **`Unknown` 立即停本轮**
5. **每轮最多成交 1 单**(由 P0 的租约收口真正保证,不靠自觉)

**爬坡档位**是一个每家的整数配额 `daily_quota`:新家先给一个小额度(如每天 3 个),
观察实测寿命与成功率达标后由人调高。默认 `0 = 只在其它家全部缺货时才用` ——
这才是「互补」而不是「取代」。

单位成本闸必须**逐货架**用该货架自己的价格算,不能再用快照里那个单一的
`price_usd`([engine.rs:494](../crates/gw-app/src/restock/engine.rs#L494))。

### 4.4 闸门作用域

| 闸门 | 作用域 | 理由 |
|---|---|---|
| `enabled` / `dry_run` | 全局 | 业务开关 |
| `daily_cap_cny` | **全局** | 钱是一个总预算 |
| 单位成本闸 / `max_price` | **每货架** | 各货架价格不同 |
| 余额 + `min_balance_reserve` | **每家** | 各家钱包独立 |
| `daily_quota`(爬坡) | **每家** | 新家灰度 |
| **购买异常熔断** | **每家** | 一家挂了不该停另一家 —— 这正是接第二家的目的 |
| **导入失败熔断** | **全局,但仅限供应商无关的失败** | ⚠️ 见下 |
| 水位 / 高峰 / 闲时抑制 | 全局 | 与供应商无关 |

⚠️ **导入熔断不能一刀切全局。** 现在 `maybe_trip` 被购买异常和导入失败共用同一个
`KEY_BREAKER`([engine.rs:754](../crates/gw-app/src/restock/engine.rs#L754))。
「响应没有 key」「key 字段改名」这类是**供应商特有输入**的问题,归全局就会让
kiroapp 的契约变化停掉正常的 drop —— 恰好毁掉冗余目标。
只有**本地 DB / 建号器**这类供应商无关的不变量失败才配全局熔断。

**旧值迁移**:升级时 `restock_breaker` 里可能存着历史的 drop 购买故障串。
P3 启动时必须把它**读走并清空**(迁进 `restock_breaker:drop`),否则会被新语义
当成全局导入熔断,一上来就把两家全停。

### 4.5 一致性断言

- **一单一货架一区域**是不变量,所以 region 是**订单级**属性(加列),不逐 key 存。
  但它现在**没被强制** —— 适配器必须断言「本单所有 key 的 region 一致」,
  发现混区就整单拒收转人工,而不是默默取第一个。
- **数量必须对得上**:现在解析器允许 `purchased=10` 却只返回 1 个合法 `ksk_`,
  引擎只检查非空([engine.rs:621](../crates/gw-app/src/restock/engine.rs#L621))。
  kiroapp 单单上限是 10,这个场景会**扣 10 份钱只导入 1 个号**。
  `Receipt.purchased != keys.len()` 或 `!= 请求数` 一律停下转人工。

### 4.6 数据模型

```sql
ALTER TABLE restock_orders ADD COLUMN supplier TEXT NOT NULL DEFAULT 'drop';
ALTER TABLE restock_orders ADD COLUMN shelf    TEXT NOT NULL DEFAULT '';
```

老行自动归 `drop`,语义正确。但**光加列不够**,首版漏了整条读写链路:

- `setup_schema()` 的增量迁移清单要加这两列([gw-store:327](../crates/gw-store/src/lib.rs#L327))
- `restock_create_order()` 要收 supplier/shelf 参数([gw-store:975](../crates/gw-store/src/lib.rs#L975))
- `RestockOrder` 与查询 SQL 要把它们读出来([store.rs:379](../crates/gw-core/src/store.rs#L379))
- `reconcile_pending()` 要据此选适配器,不能再硬编码 `self.drop`
- **供应商被改名/禁用/删除时**,其 pending 订单不能重放 → 必须**保留该家配置直到订单清空**,
  并在面板上把这类订单单列告警。禁止「找不到适配器就跳过」。
- `restock_decisions` 同样要加 supplier/shelf,否则「哪家、哪个区、以什么价被拦」查不出来。

### 4.7 配置:`deny_unknown_fields` 是回滚地雷

`RestockConfig` 带 `deny_unknown_fields`([config.rs:187](../crates/gw-core/src/config.rs#L187))。
**往 `system.yaml` 加 `suppliers:` 之后,旧镜像会直接启动失败** —— 首版声称
「分期可独立回滚」是错的。

两条出路,**选后者**:

- ~~去掉 `deny_unknown_fields`~~ —— 它挡住的是配置 typo 静默失效,不该为这个牺牲。
- ✅ **把多供应商配置搬进 DB 参数表**(和现有 `restock_params` 同一张表),
  `system.yaml` 一个字都不改。这样:配置热更、不需要重启、旧镜像照常启动、
  密钥也不经 `GET /settings` 回显。现有 `Params` 已经是这个模式,照抄即可。

---

## 五、分期

| 期 | 内容 | 独立价值 | 风险 |
|---|---|---|---|
| ~~P0-验号~~ | | ✅ 已完成,见 §1–2 | |
| ~~P1~~ | region 贯通 onboard | ✅ **已完成**,CHANGELOG `[restock-region]` | |
| **P0'** | **资金安全**:修 §3 那五条 | **独立于多供应商就该修** | 低,纯修 bug + 测试 |
| **P2** | 抽 `Supplier` trait(含 `Receipt`/`BuyOutcome`),drop 平移 | 逐字节等价,可单独上线 | 低 |
| **P3** | kiroapp 适配器 + 货架排序 + 爬坡 + 每家熔断 + 订单列 | 真正的冗余 | **中,真花钱** |
| **P4** | 余额守恒告警 + ksk_ 占比按小时物化 + 面板多家 | 验收指标才测得了 | 低 |

**P0' 排在最前**,因为它修的是现在就在漏的洞,且不依赖任何多供应商工作。

**回滚约束(P3 必须遵守)**:P3 上线后一旦产生过 kiroapp 订单,就**不能裸回滚到 P2** ——
P2 的 `reconcile_pending` 不认 supplier,会拿同一个幂等号去 drop 下单,那是真扣错钱。
P3 的回滚路径只有一条:**先把 kiroapp 关掉、等 pending 清空、再回滚**。写进部署清单。

---

## 六、风险

- **kiroapp 涨价无法在单笔上防住** → 用钱包余额当敞口上限(§2),不多充。
- **kiroapp 契约不稳**(新站,账号 2026-08-05 当天注册) → 全字段宽松解析,
  但 ⚠️ **限价相关字段不许宽松**:`de_f64` 对缺失/非法返回 `0.0`
  ([drop.rs](../crates/gw-app/src/restock/drop.rs))是 fail-**open**,
  用在 `unit_price`/`total_debit` 上等于把「解析不出来」当成「价格是 0」放行。
  这两个字段必须 fail-closed:解析不出来 → `Unknown` → 停下等人。
- **供应商 id 未校验唯一** → 两条 `id: a` 会共享 `restock_breaker:a`,
  一家故障熔断另一家。装载时断言非空且唯一。
- **接了但没省到钱** → §1.1 已说清:理由是缺货冗余不是价差。
  验收看「`drop 无库存` 轮数中被 kiroapp 接住的比例」**以及** ksk_ 占比,
  两个都要看 —— 前者证明 L1 起作用,后者才是用户真正要的结果。

---

## 七、未探明

`GET /api/public/stats` 有 `mother_price: 500`(credits = $71.4),profile 里有
`supply_allowed` / `supply_public` / `supply_keep_count` —— 说明可以买**母号**自己 mint key、
还能把多余的 key 卖回市场。买到的号 `issuer_url: https://d-90667814ff.awsapps.com/start`、
带 `account` + `password`,说明卖的就是母号 mint 出来的 IdC 子号。

若一个母号能持续 mint,单位成本可能远低于 ¥20.63/个。但无对应 API 端点
(`/api/me/mother*` 全 404),`/api/me/accounts` 返回空数组,**信息不足以估算,先记下不做**。
