//! 多供应商契约:一家货源要能被补货引擎调度,必须实现的最小面。
//!
//! ## 为什么值得抽这一层
//!
//! 2026-08-05 前引擎里只有一个 `drop: DropClient` 字段,库存/单价/余额三个数直接从它
//! 读、直接进决策。接第二家时如果照着复制一份,决策链会分叉成两条 ——
//! 「买哪家」这个判断就没有单一落点,而它恰恰是**唯一花钱的判断**。
//!
//! 所以这里定的是**归一化**而不是继承:各家把自己的库存表达成一组 [`Shelf`],
//! 把自己的钱表达成 CNY,引擎只看货架、只比 `unit_price_cny`。
//!
//! ## 金额口径(全局唯一,别在别处再折算一次)
//!
//! **一进引擎就必须是 CNY。** 折算只许发生在适配器内部,用调用方传进来的
//! `usd_to_cny`(即 `Params::rate_cap`,一个**上限汇率**)。因此:
//!
//! - 所有 `*_cny` 都是**上界**,不是精确值。方向是 fail-closed:价格估高 → 门槛更严;
//!   预算估高 → 少买一个号。两个方向都安全。
//! - 两家用**同一个**汇率折算,所以**比价是精确的**(倍率对消),尽管绝对值偏高。
//! - drop 的报价本身是 USD、扣款走 CNY;kiroapp 的报价是 **credits**,要走
//!   `credits → USD → CNY` **三段**。少折一段会低估 7 倍 —— 那会让「余额 ¥700」
//!   被判成「¥97,钱不够」。这条规矩存在的意义就是让这类错误不可表达。
//!
//! ## 契约里为什么没有 `Result`
//!
//! 购买的失败必须分成三类而不是一类,见 [`BuyOutcome`]。用 `Result<Receipt, E>` 表达
//! 不了「结果未知」——而那是唯一会丢钱的分支。

use async_trait::async_trait;

use super::drop::BoughtKey;

/// 一家供应商的一个「货架」:同一批号、同一个价、同一个服务区。
///
/// drop 没有区域概念 → 恰好一个货架(`shelf_id` 空);
/// kiroapp 分 `us` / `eu` 两个货架,价格与库存**互相独立**(实测 US 常年 0 且贵 47%)。
#[derive(Debug, Clone, PartialEq)]
pub struct Shelf {
    pub supplier_id: String,
    /// **供应商自己的货架标识**(kiroapp 是 `us` / `eu`;drop 没有 = 空串)。
    /// 下单时原样回传给对方。
    pub shelf_id: String,
    /// 该货架发出的号要落进 `extra.region` 的 **Kiro 服务区**(如 `eu-central-1`)。
    /// 空串 = 用上游默认(us-east-1)。
    ///
    /// ⚠️ 与 [`Self::shelf_id`] 是**两个命名空间**,绝不能混用:kiroapp 的货架叫 `eu`,
    /// 但号要打的是 `management.eu-central-1.kiro.dev`。写错的后果是每个请求 403。
    pub account_region: String,
    pub stock: i64,
    /// 单价,**已归一到 CNY**(见模块注释的口径说明)。
    pub unit_price_cny: f64,
    /// 对方允许的单笔上限。
    ///
    /// ⚠️ 这是**对方的**上限,不是我们的意愿量。**绝不能**把它当成「传大了会被拒」——
    /// 2026-08-05 实测 kiroapp 对超限的 `count` 是 **clamp 而不是 reject**:
    /// 发 `count: 99` 它照买 10 个并扣款。真正的量闸只有引擎侧的 `max_per_purchase`。
    pub max_per_order: i64,
    /// 档位,**数值越小越优先**(与账号优先级同向)。同档内才比价。
    ///
    /// ⚠️ **由引擎按名册回填**([`super::registry::shelf_priority_of`]),
    /// 供应商适配器一律留 `0` —— 一家货源不该自己声明自己有多重要。
    ///
    /// 留 `0` 的后果是「全部同档」,退化成纯按价格排序,即本字段引入前的行为。
    /// 这个缺省方向是安全的:忘了回填只会少一层偏好,不会把便宜货架排到贵的后面。
    pub priority: i64,
}

impl Shelf {
    /// 排序键:**先档位、再价格**。同档同价时用 `supplier_id`/`shelf_id` 定序,
    /// 保证结果**确定** —— 否则同价两家会随 HashMap 迭代顺序抖动,决策流水里
    /// 看不出为什么这轮换了家。
    ///
    /// 档位在价格之前是有意的:价格是可观测的,**号的质量不是**。实测同为
    /// KIRO POWER 的号,不同货源的封禁率能差出一个数量级(2026-08-05:drop 侧
    /// 29 个号 0 次 `temporarily_suspended`,kiroapp/eu 侧 12 个号 12 次)。
    /// 便宜 48% 但到手即封的号,单价再低也是纯损失,而这件事无法编码进单价。
    pub fn sort_key(&self) -> (i64, f64, &str, &str) {
        (
            self.priority,
            self.unit_price_cny,
            self.supplier_id.as_str(),
            self.shelf_id.as_str(),
        )
    }

    /// 人读的货架名,进决策流水与面板。
    pub fn label(&self) -> String {
        if self.shelf_id.is_empty() {
            self.supplier_id.clone()
        } else {
            format!("{}/{}", self.supplier_id, self.shelf_id)
        }
    }
}

/// 一次询价的结果:这家现在有多少钱、有哪些货架。
#[derive(Debug, Clone, Default)]
pub struct Survey {
    pub supplier_id: String,
    /// 余额,**已归一到 CNY**。
    pub balance_cny: f64,
    /// 对方原生单位的余额文本(如 `680 积分`),**仅供展示**。
    ///
    /// 单独留一列是因为面板上只显示折算后的 ¥ 会让人对不上对方网站的数字,
    /// 而对不上账的第一反应永远是「系统算错了」。
    pub balance_native: String,
    pub shelves: Vec<Shelf>,
}

/// 一次成功购买的回执。
#[derive(Debug, Clone, Default)]
pub struct Receipt {
    pub keys: Vec<BoughtKey>,
    /// 实际扣款(CNY)。由适配器按**余额差**算 —— 报价是估算,余额差才是事实。
    pub debited_cny: f64,
    pub balance_after_cny: f64,
    /// 对方声明的成交数量。**可能与请求数不等**(见 [`Shelf::max_per_order`])。
    pub purchased: i64,
    /// 这一单是**重放命中**的(对方认得幂等键,返回了同一张订单,没有二次扣款)。
    /// 对账路径据此判断「钱找回来了」而不是「又买了一次」。
    pub replayed: bool,
    /// **已确认发生了第二次真实扣款**(对账重放时对方没认出幂等键)。
    ///
    /// key 仍然要收下(钱花了,丢掉才是纯损失),但引擎必须据此**熔断这家**:
    /// 幂等失效意味着「重放安全」这个前提没了,而整套对账全建立在那个前提上。
    /// 只打一条日志是不够的 —— 没人盯日志,而下一轮还会接着重放。
    pub double_charged: bool,
}

/// 购买的四种结局。**不是 `Result`** —— 三类失败要走三条完全不同的善后路径。
#[derive(Debug, Clone)]
pub enum BuyOutcome {
    Ok(Receipt),
    /// **竞争失败,确定没扣款**:库存被抢光、余额不足、价格已变。
    /// 订单判 `failed`,**不计熔断**(一个抢购高峰不该把补货自己关掉),
    /// 且引擎可以立刻换下一个货架再试。
    Conflict(String),
    /// **对方拒绝,确定没扣款**:参数错、鉴权失效、账号被禁。
    /// 订单判 `failed` 并**计入该家的熔断** —— 这类错误重试一万次也一样。
    Fault(String),
    /// **结果未知,可能已扣款**:网络失败或对方 5xx。
    ///
    /// 订单**必须停在 `pending`**,由对账用原幂等键确认。这是四个分支里唯一
    /// 会丢钱的那个,也是整个契约不用 `Result` 的原因。
    Unknown(String),
}

/// 一家货源。
///
/// 实现方**只负责翻译**:把对方的协议翻译成货架与回执,把对方的错误翻译成
/// [`BuyOutcome`] 的四态。**任何花钱的判断都不在这里** —— 水位、预算、单价闸、
/// 余额闸全在引擎里,这样「什么情况下会掏钱」只有一个地方需要读。
#[async_trait]
pub trait Supplier: Send + Sync {
    fn id(&self) -> &str;

    /// 询价:余额 + 货架。失败返回人读的原因(会进面板)。
    ///
    /// 这是**只读**的,引擎每轮都可能调,实现方不要在这里产生任何副作用。
    async fn survey(&self, usd_to_cny: f64) -> Result<Survey, String>;

    /// 下单。
    ///
    /// - `order_id` 已由调用方**落库**,实现方必须原样传给对方作幂等键。
    /// - `count` 已过引擎所有闸门,实现方**不得自行放大**。
    /// - `max_total_cny` 是限价保护。**不要假设对方会执行它** —— kiroapp 实测完全忽略。
    async fn buy(
        &self,
        shelf: &Shelf,
        count: i64,
        order_id: &str,
        max_total_cny: f64,
        usd_to_cny: f64,
    ) -> BuyOutcome;

    /// 用原幂等键确认一张**结果未知**的订单到底成没成。
    ///
    /// 两种实现风格,都合法:
    /// - **重放**(drop):用同一个 `client_order_id` 再发一次购买请求,对方认得就返回
    ///   同一张订单。前提是对方真的实现了幂等。
    /// - **查单**(kiroapp):查订单列表按 `client_order_id` 匹配。**更安全** ——
    ///   它不依赖对方的幂等实现,只读,重放一万次也不会多扣一分钱。
    ///
    /// 能查单就别重放。
    ///
    /// ⚠️ `shelf` 必须传**下单时那个货架**。重放的语义是「用原参数再问一次」,
    /// 少一个参数就不是原请求了 —— kiroapp 不带 `region` 会回落 US,
    /// 于是要么被判成另一张订单(二次扣款),要么取回一批错区域的 key。
    async fn reconcile(
        &self,
        order_id: &str,
        shelf: &str,
        count: i64,
        max_total_cny: f64,
        usd_to_cny: f64,
    ) -> BuyOutcome;
}

/// 把货架按**档位 → 价格**排序,并滤掉没货的。
///
/// 抽成自由函数(而不是引擎的方法)是为了能直接测:「买哪个货架」是整套多供应商
/// 里唯一真正的调度判断,它必须能在没有网络、没有 DB 的情况下被钉住。
pub fn rank_shelves(mut shelves: Vec<Shelf>) -> Vec<Shelf> {
    shelves.retain(|s| s.stock > 0 && s.unit_price_cny > 0.0);
    shelves.sort_by(|a, b| {
        a.sort_key()
            .partial_cmp(&b.sort_key())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    shelves
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shelf(sup: &str, id: &str, stock: i64, price: f64) -> Shelf {
        Shelf {
            supplier_id: sup.into(),
            shelf_id: id.into(),
            account_region: String::new(),
            stock,
            unit_price_cny: price,
            max_per_order: 10,
            priority: 0,
        }
    }

    fn tiered(sup: &str, id: &str, stock: i64, price: f64, pri: i64) -> Shelf {
        Shelf { priority: pri, ..shelf(sup, id, stock, price) }
    }

    #[test]
    fn 货架按价格升序且滤掉没货的() {
        let out = rank_shelves(vec![
            shelf("drop", "", 5, 20.95),
            shelf("kiroapp", "eu", 18, 15.43),
            shelf("kiroapp", "us", 0, 30.86), // 没货
        ]);
        assert_eq!(out.len(), 2, "库存 0 的货架不该出现在候选里");
        assert_eq!(out[0].label(), "kiroapp/eu", "最便宜的排第一");
        assert_eq!(out[1].label(), "drop");
    }

    #[test]
    fn 价格为零的货架视为报价异常被剔除() {
        // 对方接口抖动时数字字段解析失败会得到 0。0 元排序后必然排第一,
        // 于是「最便宜」永远选中一个报不出价的货架 —— 必须在排序前就剔掉。
        let out = rank_shelves(vec![shelf("x", "", 9, 0.0), shelf("drop", "", 5, 20.95)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].supplier_id, "drop");
    }

    #[test]
    fn 低档位的贵货架排在高档位的便宜货架前面() {
        // 这是引入档位的**全部意义**:drop 贵 71% 但号不被封,kiroapp/eu 便宜但到手即封。
        // 只按价格排,永远选中后者。
        let out = rank_shelves(vec![
            tiered("kiroapp", "eu", 17, 15.43, 2),
            tiered("kiroapp", "us", 10, 30.86, 1),
            tiered("drop", "", 5, 26.36, 0),
        ]);
        let order: Vec<String> = out.iter().map(|s| s.label()).collect();
        assert_eq!(order, ["drop", "kiroapp/us", "kiroapp/eu"]);
    }

    #[test]
    fn 高档位缺货时自动落到下一档而不是停摆() {
        // drop 常年 0 库存(近 7 天 854 轮无货)。档位**不能**变成「drop 没货就不补」——
        // 那等于亲手制造多供应商本来要消除的断供。
        let out = rank_shelves(vec![
            tiered("drop", "", 0, 26.36, 0), // 没货
            tiered("kiroapp", "eu", 17, 15.43, 2),
            tiered("kiroapp", "us", 10, 30.86, 1),
        ]);
        assert_eq!(out[0].label(), "kiroapp/us", "越过空档,落到下一档最优的");
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn 同档内仍然按价格比而不是按档位平局() {
        let out = rank_shelves(vec![
            tiered("b", "", 5, 30.0, 1),
            tiered("a", "", 5, 40.0, 1),
            tiered("c", "", 5, 20.0, 1),
        ]);
        assert_eq!(
            out.iter().map(|s| s.supplier_id.clone()).collect::<Vec<_>>(),
            ["c", "b", "a"],
            "档位相同就退回比价"
        );
    }

    #[test]
    fn 档位缺省全为零时与引入档位前逐字节同序() {
        // 回滚安全:老名册没有 priority 字段 → 全 0 → 必须与纯比价的旧行为一致。
        let out = rank_shelves(vec![
            shelf("drop", "", 5, 20.95),
            shelf("kiroapp", "eu", 18, 15.43),
            shelf("kiroapp", "us", 3, 30.86),
        ]);
        assert_eq!(
            out.iter().map(|s| s.label()).collect::<Vec<_>>(),
            ["kiroapp/eu", "drop", "kiroapp/us"]
        );
    }

    #[test]
    fn 负档位可用于把一家临时顶到最前() {
        // 档位是 i64 而不是 usize:出事时要能**不动别人**就把某一家提到最前面,
        // 「把其他所有家都调大」在生产上是错误率最高的操作。
        let out = rank_shelves(vec![
            tiered("drop", "", 5, 26.36, 0),
            tiered("紧急", "", 5, 99.0, -1),
        ]);
        assert_eq!(out[0].supplier_id, "紧急");
    }

    #[test]
    fn 同价时定序确定不随迭代顺序抖动() {
        let a = rank_shelves(vec![shelf("bbb", "", 1, 20.0), shelf("aaa", "", 1, 20.0)]);
        let b = rank_shelves(vec![shelf("aaa", "", 1, 20.0), shelf("bbb", "", 1, 20.0)]);
        assert_eq!(a[0].supplier_id, "aaa");
        assert_eq!(a.iter().map(|s| s.supplier_id.clone()).collect::<Vec<_>>(),
                   b.iter().map(|s| s.supplier_id.clone()).collect::<Vec<_>>());
    }

    #[test]
    fn 货架名区分有无区域() {
        assert_eq!(shelf("drop", "", 1, 1.0).label(), "drop");
        assert_eq!(shelf("kiroapp", "eu", 1, 1.0).label(), "kiroapp/eu");
    }
}
