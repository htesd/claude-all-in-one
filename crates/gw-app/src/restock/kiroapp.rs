//! kiroapp.io 客户端(第二家货源)。
//!
//! 与 drop 的**四处硬差别**,抄 drop 的实现必然踩:
//!
//! | | drop.kiro.ss | kiroapp.io |
//! |---|---|---|
//! | 购买路由 | `/api/my/purchase`(**my**) | `/api/me/purchase`(**me**,`/api/my/` 返 405) |
//! | 计价单位 | USD | **credits**,`credits_per_usd` 在 `/api/me/recharge` |
//! | 错误体 | `{"error":{"code","message"}}` | `{"error":"中文串"}` **扁平** |
//! | 竞争失败 | 409 | **403**(余额不足)/ **404**(该区域无货) |
//!
//! ## 这家没有任何服务端限价
//!
//! `max_total_cny` **完全没有实现**,发什么都照扣。更狠的是 `count`:
//! 2026-08-05 实测发 `count: 99`(对方 `max` 明写 10),它**不报错,clamp 到 10 并成交**,
//! 一次扣走 150 积分。响应里 `requested: 99, purchased: 10` 是唯一的痕迹。
//!
//! 所以这家的敞口上限只有两个:**钱包余额**,和**引擎侧的 `max_per_purchase` + 日预算**。
//! 适配器这一层能做的只有一件事:[`assert_not_oversold`] —— 成交数超过请求数时**尖叫**,
//! 让这类事故至少在流水里留下证据,而不是变成一条「购买成功」。

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;

use super::drop::BoughtKey;
use super::supplier::{BuyOutcome, Receipt, Shelf, Supplier, Survey};

/// 与 drop 同口径:对方往返数秒,给足余量但不能无限等。
const TIMEOUT_SECS: u64 = 20;

pub const DEFAULT_BASE_URL: &str = "https://kiroapp.io";

/// 货架 → Kiro 服务区。**两个命名空间,不能互相当对方用**(见 [`Shelf::account_region`])。
///
/// 写死而不是信对方下发:区域串要一路走到 `management.<region>.kiro.dev`,
/// 拼错的后果是这个号的每个请求都 403,而那时钱已经花了。购买响应里逐 key 带的
/// `region` 仍会被采纳(见 [`BoughtKey::parse`]),这张表只是询价阶段的先验。
fn region_of(shelf_id: &str) -> &'static str {
    match shelf_id {
        "eu" => "eu-central-1",
        _ => "us-east-1",
    }
}

/// 数字字段可能以字符串下发(drop 实测有此形态)。这里只用于**非金额**字段。
fn as_f64(v: Option<&serde_json::Value>) -> Option<f64> {
    match v? {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_i64(v: Option<&serde_json::Value>) -> Option<i64> {
    as_f64(v).map(|f| f as i64)
}

#[derive(Deserialize)]
struct StockBody {
    #[serde(default)]
    balance: Option<serde_json::Value>,
    #[serde(default)]
    max: Option<serde_json::Value>,
    #[serde(default)]
    stock_us: Option<serde_json::Value>,
    #[serde(default)]
    stock_eu: Option<serde_json::Value>,
    #[serde(default)]
    price_us: Option<serde_json::Value>,
    #[serde(default)]
    price_eu: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct PurchaseBody {
    #[serde(default)]
    client_order_id: String,
    #[serde(default)]
    purchased: Option<serde_json::Value>,
    #[serde(default)]
    requested: Option<serde_json::Value>,
    #[serde(default)]
    remaining: Option<serde_json::Value>,
    #[serde(default)]
    total_debit: Option<serde_json::Value>,
    #[serde(default)]
    replayed: bool,
    #[serde(default)]
    keys: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
struct OrdersBody {
    #[serde(default)]
    items: Vec<OrderRow>,
    /// 分页元信息。**对账要靠它判断「没找到」到底算不算数** —— 见 `reconcile`。
    #[serde(default)]
    total: Option<serde_json::Value>,
    #[serde(default)]
    page_size: Option<serde_json::Value>,
}

#[derive(Deserialize, Clone)]
struct OrderRow {
    #[serde(default)]
    client_order_id: String,
    #[serde(default)]
    purchased_count: Option<serde_json::Value>,
    #[serde(default)]
    total_debit: Option<serde_json::Value>,
    #[serde(default)]
    remaining: Option<serde_json::Value>,
}

/// HTTP 层的失败,尚未翻译成 [`BuyOutcome`]。
struct Call {
    status: u16,
    /// 扁平错误体里的中文串;非错误响应为空。
    error: String,
    body: String,
}

/// `credits_per_usd` 的缓存寿命。
///
/// 为什么要缓存:询价与下单各要读一次计价口径,而池子空着时决策每 30 秒跑一轮 ——
/// 不缓存就是把请求量翻倍地打在一个**不怎么变**的常量上。更要紧的是**下单路径**:
/// 那里读失败会返回 `Fault` 并计入该家熔断,于是对方一次抖动就能把整家停掉。
///
/// 600 秒:比轮询间隔(30s)长得多以真正省掉请求,又远短于任何合理的调价周期。
const RATE_TTL: Duration = Duration::from_secs(600);

pub struct KiroappClient {
    id: String,
    base: String,
    api_key: String,
    http: reqwest::Client,
    /// `(credits_per_usd, 取到的时刻)`。见 [`RATE_TTL`]。
    rate_cache: parking_lot::Mutex<Option<(f64, std::time::Instant)>>,
}

impl KiroappClient {
    pub fn new(id: &str, base_url: &str, api_key: &str) -> anyhow::Result<Self> {
        let base = base_url.trim().trim_end_matches('/');
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .build()?;
        Ok(Self {
            id: id.to_string(),
            base: if base.is_empty() { DEFAULT_BASE_URL.into() } else { base.into() },
            api_key: api_key.to_string(),
            http,
            rate_cache: parking_lot::Mutex::new(None),
        })
    }

    /// 发一次请求。网络层失败用 `status = 0` 表示(与 drop 同约定)。
    ///
    /// 错误文案里**不带 url** —— reqwest 的 Display 会把 host 带出来,进对外响应就是
    /// 渠道来源泄漏。
    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Call {
        let mut rb = self
            .http
            .request(method, format!("{}{}", self.base, path))
            .header("X-API-Key", &self.api_key)
            .header("Content-Type", "application/json");
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        let resp = match rb.send().await {
            Ok(r) => r,
            Err(e) => {
                let why = if e.is_timeout() {
                    "请求超时"
                } else if e.is_connect() {
                    "连接失败"
                } else {
                    "请求失败"
                };
                return Call { status: 0, error: format!("{path} {why}"), body: String::new() };
            }
        };
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        // 扁平错误体:`{"error":"中文串"}`。drop 那套嵌套结构在这里解不出来 ——
        // 原先直接复用会让所有错误变成空字符串,面板上就只剩一个 HTTP 码。
        let error = if status >= 400 {
            serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|x| x.as_str()).map(str::to_string))
                .unwrap_or_else(|| format!("HTTP {status}"))
        } else {
            String::new()
        };
        Call { status, error, body }
    }

    /// `credits → CNY` 的折算系数。
    ///
    /// **不许写死**:`credits_per_usd` 是对方的定价旋钮,它变了而我们没变,
    /// 会让整条比价链失真 —— 而比价正是选家的唯一依据。查不到**且缓存也过期**时
    /// 直接失败,这家本轮不参与竞标(fail-closed:算不出价就别买)。
    async fn credit_to_cny(&self, usd_to_cny: f64) -> Result<f64, String> {
        if let Some((per_usd, at)) = *self.rate_cache.lock() {
            if at.elapsed() < RATE_TTL {
                return Ok(usd_to_cny / per_usd);
            }
        }
        let c = self.call(reqwest::Method::GET, "/api/me/recharge", None).await;
        if c.status != 200 {
            return Err(format!("读取计价口径失败: {}", non_empty(&c.error, c.status)));
        }
        let v: serde_json::Value = serde_json::from_str(&c.body)
            .map_err(|e| format!("计价口径响应解析失败: {e}"))?;
        let per_usd = as_f64(v.get("credits_per_usd"))
            .filter(|x| *x > 0.0)
            .ok_or_else(|| "计价口径缺 credits_per_usd".to_string())?;
        *self.rate_cache.lock() = Some((per_usd, std::time::Instant::now()));
        Ok(usd_to_cny / per_usd)
    }

    /// 把一次购买响应翻译成回执。`rate` = credits→CNY。
    ///
    /// `order_id` 只用来核对回显 —— 对方把回执贴到别的订单上时,我们会按自己的
    /// `client_order_id` 记账,那笔钱就对不上了。这类错配只在日志里能被发现。
    fn receipt_of(
        &self,
        b: PurchaseBody,
        order_id: &str,
        requested: i64,
        rate: f64,
        cap_cny: f64,
    ) -> Receipt {
        if !b.client_order_id.is_empty() && b.client_order_id != order_id {
            tracing::error!(
                "补货:{} 回执的订单号 {} 与我方 {order_id} 不符,记账可能张冠李戴",
                self.id,
                b.client_order_id
            );
        }
        let purchased = as_i64(b.purchased.as_ref()).unwrap_or(0);
        assert_not_oversold(&self.id, requested, as_i64(b.requested.as_ref()), purchased);
        // 扣款取 `total_debit`(对方权威值),**不做宽松兜底**:
        // 缺这个字段时回落 0 会让日预算把这一单当成没花钱,下一轮接着买。
        // 方向必须 fail-closed —— 算不出就按限价记满。
        let debited_cny = match as_f64(b.total_debit.as_ref()).filter(|x| *x > 0.0) {
            Some(d) => d * rate,
            None => {
                tracing::error!(
                    "补货:{} 购买响应缺 total_debit,按限价 ¥{cap_cny:.2} 记账(宁可高估)",
                    self.id
                );
                cap_cny
            }
        };
        Receipt {
            keys: b.keys.iter().filter_map(BoughtKey::parse).collect(),
            debited_cny,
            balance_after_cny: as_f64(b.remaining.as_ref()).unwrap_or(0.0) * rate,
            purchased,
            replayed: b.replayed,
            // 由 `reconcile` 在确认幂等未命中时置位;正常购买路径永远是 false。
            double_charged: false,
        }
    }
}

/// 空错误串时回落成 HTTP 码,免得面板上出现一条没有内容的报错。
fn non_empty(err: &str, status: u16) -> String {
    if err.is_empty() {
        format!("HTTP {status}")
    } else {
        err.to_string()
    }
}

/// 成交数超过请求数 = 对方 clamp/放大了我们的订单。**只报警不阻断**:
/// 号已经买到手,丢掉它才是真损失;但这件事必须在日志里留下证据。
///
/// 这条断言的由来是一次真实事故(见模块注释):`count: 99` 被 clamp 成 10 并成交。
/// 当时如果有这行,流水里会立刻出现一条 error 而不是一条平静的「购买成功」。
fn assert_not_oversold(id: &str, requested: i64, echoed: Option<i64>, purchased: i64) {
    if purchased > requested {
        tracing::error!(
            "补货:{id} **超卖** —— 请求 {requested} 个,对方成交 {purchased} 个。\
             钱已扣,号会照常上架,但请立刻核对钱包余额与日预算"
        );
    }
    if let Some(e) = echoed {
        if e != requested {
            tracing::warn!("补货:{id} 回显请求数 {e} 与我方 {requested} 不符");
        }
    }
}

/// 按错误文案把 4xx 归类。**白名单,未命中一律 `Fault`**(fail-closed)——
/// `Fault` 会计入该家熔断并最终停手,`Conflict` 会被忽略并立刻重试,
/// 把未知错误当成 `Conflict` 就是拿一个不认识的失败去无限重试。
///
/// 不按 HTTP 码分类的原因:这家的 403 既可能是「余额不足」(竞争失败)也可能是
/// 「API Key 失效」(真故障),码相同、善后完全相反。
fn classify_4xx(err: &str) -> BuyOutcome {
    const CONFLICT: [&str; 5] = ["余额不足", "无可用", "暂无", "库存", "售罄"];
    if CONFLICT.iter().any(|k| err.contains(k)) {
        BuyOutcome::Conflict(err.to_string())
    } else {
        BuyOutcome::Fault(err.to_string())
    }
}

#[async_trait]
impl Supplier for KiroappClient {
    fn id(&self) -> &str {
        &self.id
    }

    async fn survey(&self, usd_to_cny: f64) -> Result<Survey, String> {
        let rate = self.credit_to_cny(usd_to_cny).await?;
        let c = self.call(reqwest::Method::GET, "/api/me/stock", None).await;
        if c.status != 200 {
            return Err(format!("读取库存失败: {}", non_empty(&c.error, c.status)));
        }
        let b: StockBody =
            serde_json::from_str(&c.body).map_err(|e| format!("库存响应解析失败: {e}"))?;
        let balance_credits = as_f64(b.balance.as_ref()).unwrap_or(0.0);
        let max_per_order = as_i64(b.max.as_ref()).filter(|x| *x > 0).unwrap_or(1);

        let mut shelves = Vec::new();
        for (shelf_id, stock, price) in [
            ("us", b.stock_us.as_ref(), b.price_us.as_ref()),
            ("eu", b.stock_eu.as_ref(), b.price_eu.as_ref()),
        ] {
            let (Some(stock), Some(price)) = (as_i64(stock), as_f64(price)) else {
                continue;
            };
            shelves.push(Shelf {
                supplier_id: self.id.clone(),
                shelf_id: shelf_id.to_string(),
                account_region: region_of(shelf_id).to_string(),
                stock,
                unit_price_cny: price * rate,
                max_per_order,
                priority: 0, // 引擎按名册回填,适配器不表达自己的档位
            });
        }
        Ok(Survey {
            supplier_id: self.id.clone(),
            balance_cny: balance_credits * rate,
            balance_native: format!("{balance_credits:.0} 积分"),
            shelves,
        })
    }

    async fn buy(
        &self,
        shelf: &Shelf,
        count: i64,
        order_id: &str,
        max_total_cny: f64,
        usd_to_cny: f64,
    ) -> BuyOutcome {
        let rate = match self.credit_to_cny(usd_to_cny).await {
            Ok(r) => r,
            // 折算不出来就**不下单**:买回来记不了账,日预算会失真。
            //
            // 判 `Conflict` 而不是 `Fault`:这是**下单前**的自我否决(请求根本没发出去),
            // 既确定没扣款,也不该算作「这家有故障」去累计熔断 —— 否则对方计价接口
            // 抖两下就能把一整家停掉,而它的购买接口其实好好的。
            // 缓存(见 [`RATE_TTL`])已让这条路径极少走到。
            Err(e) => return BuyOutcome::Conflict(format!("下单前折算失败,本轮不从这家买: {e}")),
        };
        let c = self
            .call(
                reqwest::Method::POST,
                "/api/me/purchase",
                Some(serde_json::json!({
                    "count": count,
                    "client_order_id": order_id,
                    "region": shelf.shelf_id,
                    // 对方没实现,仍然发 —— 零成本,且它哪天实现了我们立刻受保护。
                    "max_total_cny": max_total_cny,
                })),
            )
            .await;
        match c.status {
            200 => match serde_json::from_str::<PurchaseBody>(&c.body) {
                Ok(b) => BuyOutcome::Ok(self.receipt_of(b, order_id, count, rate, max_total_cny)),
                // 200 但解析不出来 = 钱可能已经扣了而我们读不懂回执。**必须判未知**。
                Err(e) => BuyOutcome::Unknown(format!("购买响应解析失败(可能已扣款): {e}")),
            },
            s if (400..500).contains(&s) => classify_4xx(&c.error),
            s => BuyOutcome::Unknown(format!("HTTP {s} {}", c.error)),
        }
    }

    async fn reconcile(
        &self,
        order_id: &str,
        shelf: &str,
        count: i64,
        max_total_cny: f64,
        usd_to_cny: f64,
    ) -> BuyOutcome {
        let rate = match self.credit_to_cny(usd_to_cny).await {
            Ok(r) => r,
            Err(e) => return BuyOutcome::Unknown(format!("对账时折算失败: {e}")),
        };
        // ① **先查单**。这一步是只读的,查一万次也不会多扣一分钱,而它能回答对账里
        //    最关键的那个问题:那次请求到底有没有落到对方那边。
        let c = self.call(reqwest::Method::GET, "/api/me/orders", None).await;
        if c.status != 200 {
            return BuyOutcome::Unknown(format!("查单失败: {}", non_empty(&c.error, c.status)));
        }
        // 解析失败必须判 **Unknown** 而不是「没找到」。
        // 200 但响应截断/字段升级/`items:null` 时,`.ok()` 会静默变成空列表,
        // 于是一笔**已经扣了款**的订单被当成「对方无此单」判死,限价占用被释放,
        // 钱和 key 从此不再重试。这是整条对账链上最贵的一次误判。
        let body: OrdersBody = match serde_json::from_str(&c.body) {
            Ok(b) => b,
            Err(e) => return BuyOutcome::Unknown(format!("查单响应解析失败: {e}")),
        };
        let found = body.items.iter().find(|o| o.client_order_id == order_id).cloned();
        let Some(row) = found else {
            // 「这一页没有」≠「不存在」。订单列表是分页的(实测 `page_size: 50`),
            // 只有在**确认看全了**的前提下,「没找到」才等于「确定没扣款」。
            // 看不全就必须留在 pending 等下一轮 —— 宁可多问几次,不可错判一次。
            let total = as_i64(body.total.as_ref()).unwrap_or(i64::MAX);
            let page_size = as_i64(body.page_size.as_ref()).unwrap_or(0);
            if page_size > 0 && total <= page_size {
                return BuyOutcome::Conflict("对方订单列表已看全且无此单,确认未成交".into());
            }
            return BuyOutcome::Unknown(format!(
                "订单列表分页未看全(共 {total} 条 / 每页 {page_size}),无法确认,保持在途"
            ));
        };

        // ② 单确实存在 = 钱已经花了。此时才重放,目的**只是把 key 取回来**。
        //    对方响应里有 `replayed` 字段,说明幂等是它有意实现的能力。
        //
        //    ⚠️ 必须带上**下单时那个货架** —— 少了 `region` 就不是原请求了,
        //    对方会回落 US,要么判成另一张订单(二次扣款),要么给回一批错区域的 key。
        let c = self
            .call(
                reqwest::Method::POST,
                "/api/me/purchase",
                Some(serde_json::json!({
                    "count": count,
                    "client_order_id": order_id,
                    "region": shelf,
                    "max_total_cny": max_total_cny,
                })),
            )
            .await;
        if c.status == 200 {
            if let Ok(b) = serde_json::from_str::<PurchaseBody>(&c.body) {
                // `replayed=false` = 对方没认出幂等键,这**是第二次真实扣款**。
                // key 照收(钱已经花了,丢掉才是纯损失),但必须让补货停手 ——
                // 判 `Fault` 会让引擎计入该家熔断,人来看过之前不再从它买。
                if !b.replayed {
                    tracing::error!(
                        "补货:{} 对账重放 order={order_id} 返回 replayed=false —— \
                         **这是第二次真实扣款**,已把该货源熔断,请人工核对钱包流水",
                        self.id
                    );
                    let mut r = self.receipt_of(b, order_id, count, rate, max_total_cny);
                    r.double_charged = true;
                    return BuyOutcome::Ok(r);
                }
                return BuyOutcome::Ok(self.receipt_of(b, order_id, count, rate, max_total_cny));
            }
        }
        // ③ 重放拿不回 key,但订单确实存在 —— 钱花了、号没到手。
        //    把已知的扣款金额如实记进回执(否则日预算会漏算),keys 为空,
        //    引擎据此把它落成**孤儿订单**,由面板的醒目横幅交给人处理。
        let debited = as_f64(row.total_debit.as_ref()).map(|d| d * rate).unwrap_or(max_total_cny);
        tracing::error!(
            "补货:{} order={order_id} 已成交但取不回 key(扣款 ¥{debited:.2}),转人工",
            self.id
        );
        BuyOutcome::Ok(Receipt {
            keys: Vec::new(),
            debited_cny: debited,
            balance_after_cny: as_f64(row.remaining.as_ref()).unwrap_or(0.0) * rate,
            purchased: as_i64(row.purchased_count.as_ref()).unwrap_or(0),
            replayed: true,
            double_charged: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 货架标识与服务区是两个命名空间() {
        assert_eq!(region_of("eu"), "eu-central-1");
        assert_eq!(region_of("us"), "us-east-1");
        // 未知货架回落 us-east-1 = 上游默认,与不写 region 等价(不会更糟)。
        assert_eq!(region_of("mars"), "us-east-1");
    }

    #[test]
    fn 四xx按文案分类未命中一律判故障() {
        // 竞争失败:确定没扣款,不计熔断,可以换一家立刻重试。
        for s in [
            "余额不足，请充值",
            "该区域（us-east-1）暂无可用 Key，但 eu-central-1 有货，请带上 region 参数重试",
            "库存不足",
        ] {
            assert!(matches!(classify_4xx(s), BuyOutcome::Conflict(_)), "应判竞争失败: {s}");
        }
        // 真故障:重试一万次也一样,必须计熔断。
        for s in ["region 无效（可选: us / eu）", "API Key 无效", "HTTP 401"] {
            assert!(matches!(classify_4xx(s), BuyOutcome::Fault(_)), "应判故障: {s}");
        }
        // 不认识的错误 → Fault(fail-closed)。判成 Conflict 会变成无限重试。
        assert!(matches!(classify_4xx("某种全新的错误"), BuyOutcome::Fault(_)));
    }

    #[test]
    fn 库存响应拆成两个货架且价格归一到cny() {
        // 2026-08-05 实测原文。
        let b: StockBody = serde_json::from_str(
            r#"{"stock":18,"price":30,"price_us":30,"price_eu":15,"balance":680,
                "max":10,"stock_us":0,"stock_eu":18,"warranty_minutes":10}"#,
        )
        .unwrap();
        assert_eq!(as_i64(b.stock_eu.as_ref()), Some(18));
        assert_eq!(as_i64(b.stock_us.as_ref()), Some(0));
        // credits_per_usd = 7,rate_cap = 7.2 → 每积分 ¥1.0286
        let rate = 7.2 / 7.0;
        let eu = as_f64(b.price_eu.as_ref()).unwrap() * rate;
        let bal = as_f64(b.balance.as_ref()).unwrap() * rate;
        assert!((eu - 15.43).abs() < 0.01, "EU 单价应约 ¥15.43,实际 {eu}");
        // 680 积分是 **¥699**,不是 $97。少折一段会低估 7 倍,让 ¥700 被判成钱不够。
        assert!((bal - 699.4).abs() < 0.5, "余额应约 ¥699,实际 {bal}");
    }

    #[test]
    fn 缺total_debit时按限价记满而不是记零() {
        let c = KiroappClient::new("kiroapp", DEFAULT_BASE_URL, "k").unwrap();
        let b: PurchaseBody =
            serde_json::from_str(r#"{"purchased":1,"remaining":600,"keys":["ksk_a"]}"#).unwrap();
        let r = c.receipt_of(b, "oid", 1, 1.0286, 20.0);
        assert_eq!(r.debited_cny, 20.0, "缺扣款字段必须按限价记满,记 0 会让日预算漏算");
        assert_eq!(r.keys.len(), 1);
    }

    #[test]
    fn 超卖会被识别但不丢弃已买到的号() {
        // 真实事故形状:请求 99、对方 clamp 到 10 并成交。
        let c = KiroappClient::new("kiroapp", DEFAULT_BASE_URL, "k").unwrap();
        let b: PurchaseBody = serde_json::from_str(
            r#"{"purchased":10,"requested":99,"remaining":530,"total_debit":150,
                "keys":[{"key":"ksk_a","region":"eu-central-1"},
                        {"key":"ksk_b","region":"eu-central-1"}],"replayed":false}"#,
        )
        .unwrap();
        let r = c.receipt_of(b, "oid", 1, 1.0286, 20.0);
        assert_eq!(r.purchased, 10);
        assert_eq!(r.keys.len(), 2, "已经花了钱的号一个都不能丢");
        assert_eq!(r.keys[0].region, "eu-central-1", "区域必须跟着 key 走");
        assert!((r.debited_cny - 150.0 * 1.0286).abs() < 0.1, "记账按对方权威的 total_debit");
    }

    #[test]
    fn 订单响应能按幂等键匹配() {
        // 对账第一步只读查单的真实响应形状。
        let b: OrdersBody = serde_json::from_str(
            r#"{"items":[{"id":"uuid","client_order_id":"d9764acddba49d08aafe6c1120906836",
                 "requested_count":1,"purchased_count":1,"unit_price":20,"total_debit":20,
                 "remaining":680}],"total":1}"#,
        )
        .unwrap();
        let hit = b.items.iter().find(|o| o.client_order_id == "d9764acddba49d08aafe6c1120906836");
        assert!(hit.is_some(), "查单必须能按 client_order_id 定位,否则对账无从下手");
        assert_eq!(as_i64(hit.unwrap().total_debit.as_ref()), Some(20));
    }
}
