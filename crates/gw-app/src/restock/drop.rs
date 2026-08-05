//! drop.kiro.ss 客户端(自动补货的上游卖家)。
//!
//! 鉴权只用 `X-API-Key`。该站另有一套只认 HttpOnly session cookie 的 `/api/v1/*`
//! (reservation / wallet / dashboard),API Key 对其一律 401 `AUTH_REQUIRED` ——
//! **不要**试图用它们,会话会过期,不适合无人值守。
//!
//! 注意路由前缀不一致:库存是 `/api/me/stock`(**me**),其余全是 `/api/my/*`(my)。
//! `/api/my/stock` 与 `/api/me/profile` 均返回 404,抄错必然失败。这不是笔误,是对方如此。

use std::time::Duration;

use serde::Deserialize;

/// 单次请求超时。实测该站 API 往返 3.4–4.2s,给足余量但不能无限等 ——
/// 补货循环卡死等于补货停摆。
const TIMEOUT_SECS: u64 = 20;

#[derive(Debug)]
pub struct DropError {
    pub message: String,
    pub status: u16,
    pub code: String,
}

impl std::fmt::Display for DropError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for DropError {}

impl DropError {
    fn net(msg: impl Into<String>) -> Self {
        Self { message: msg.into(), status: 0, code: String::new() }
    }

    /// 409:余额不足 / 库存不足 / 订单号冲突 / 价格超过 `max_total_cny`。
    ///
    /// 这类**不是故障**,是正常的竞争失败(别人抢先买走了、价格刚涨),
    /// 因此**不应计入熔断** —— 否则一个抢购高峰就能把补货自己关掉。
    pub fn is_price_or_stock_conflict(&self) -> bool {
        self.status == 409
    }

    /// **结果未知**:这一单到底扣没扣款,从我方看不出来。
    ///
    /// 两种情况:
    /// - `status == 0` —— 网络层失败(超时 / 连接断 / 解码失败)。请求可能已经打到对方
    ///   并成交,只是响应没回来。**实测该站往返 3.4–4.2s,而超时是 20s** ——
    ///   真触发超时时,对方那边多半已经处理完了。
    /// - `5xx` —— 对方内部错误。可能发生在扣款之前,也可能在之后。
    ///
    /// 反过来,`4xx`(409 竞争、400 参数错、401/403 鉴权)都是**对方在处理前就拒绝了**,
    /// 确定没扣款,可以安全地把订单判死。
    ///
    /// ## 为什么必须单独分出这一类
    ///
    /// 原先所有 `purchase` 错误一律把订单标成 `failed`,而对账只扫 `pending`
    /// —— 于是「对方已扣款、响应丢了」这条路径会**永久失去对账机会**,钱和 key 都成孤儿。
    /// 结果未知的订单必须**停在 `pending`** 等重放确认,这是唯一能把钱找回来的状态。
    pub fn is_indeterminate(&self) -> bool {
        self.status == 0 || self.status >= 500
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Stock {
    pub stock: i64,
    /// **USD**。对方报价用美元、扣款走人民币,别把它当人民币用。
    pub price_usd: f64,
    pub balance_cny: f64,
}

/// 买到的一个号。
///
/// **不能退化回 `String`** —— `region` 必须跟着 key 一路走到上号。Kiro 的号**绑死服务区**:
/// 2026-08-05 实测一个 `eu-central-1` 的 key,打 `management.eu-central-1.kiro.dev` 返 200,
/// 打 `management.us-east-1.kiro.dev` 直接 403 `Invalid token`;而 caio 读不到
/// `extra.region` 时默认就是 `us-east-1`(`gw_kiro::usage_limits::DEFAULT_REGION`)。
///
/// 原先 [`super::engine::Engine::onboard`] 把 key 序列化成**裸串数组**,走
/// `import::map_api_key(s, None)` 这条不带 region 的路径。drop 的号全是 us-east-1,
/// 默认值恰好蒙对,所以这个洞一直没暴露 —— 换一家欧洲区的货源就是**每个请求都 403**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoughtKey {
    /// 官方 API Key(`ksk_` 前缀,解析侧已过滤)。
    pub api_key: String,
    /// 该号所属服务区。**空串 = 用上游默认**(us-east-1),与本次改动前逐字节等价。
    pub region: String,
    /// 档位标签(如 `KIRO POWER`),仅作展示。空串 = 不写入 extra。
    pub subscription_title: String,
}

impl BoughtKey {
    /// 从响应里的一个 key 条目解析。裸串与 `{"key": "ksk_…"}` 对象都收,只放行 `ksk_` 前缀。
    ///
    /// 对象形态顺带取 `region` / `subscription_title`:**drop 目前两个都不发**
    /// (它的号全是 us-east-1),取不到就留空;但同一个响应形状会被别家复用,
    /// 取到就带上,免得下一家接进来时又丢一次 region。
    pub fn parse(v: &serde_json::Value) -> Option<Self> {
        let (api_key, region, subscription_title) = match v {
            serde_json::Value::String(s) => (s.trim().to_string(), String::new(), String::new()),
            serde_json::Value::Object(o) => {
                let field = |name: &str| {
                    o.get(name).and_then(|x| x.as_str()).unwrap_or("").trim().to_string()
                };
                (field("key"), field("region"), field("subscription_title"))
            }
            _ => return None,
        };
        if !api_key.starts_with("ksk_") {
            return None;
        }
        Some(Self { api_key, region, subscription_title })
    }
}

#[derive(Debug, Clone)]
pub struct PurchaseResult {
    pub client_order_id: String,
    pub purchased: i64,
    pub remaining_cny: f64,
    pub status: String,
    pub keys: Vec<BoughtKey>,
}

/// 限价保护值:`count × 单价(USD) × 上限汇率`,向上取整到分。
///
/// 取**上限汇率**而非实时汇率:宁可容忍几毛钱的宽松,也不能因为汇率小幅波动让合法订单
/// 被对方判 409;同时又能挡住"价格翻倍"这类真正该拦的情况。
///
/// **先 round 再 ceil**:`3 × 2.95 × 7.2` 的精确值是 63.72,但二进制浮点算出
/// 63.720000000000006,直接 ceil 会变成 63.73 —— 不影响安全(限价偏高更保守),
/// 但会让账面出现无法解释的一分钱。
pub fn max_total_cny_for(count: i64, price_usd: f64, rate_cap: f64) -> f64 {
    let cents = (count as f64) * price_usd * rate_cap * 100.0;
    ((cents * 1e6).round() / 1e6).ceil() / 100.0
}

/// 32 位十六进制幂等键(对方的格式要求)。
pub fn new_order_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    // 不引 rand 依赖:纳秒时钟 + 进程 id + 一个进程内自增计数,三者拼起来足够唯一。
    // 幂等键的作用是「崩溃后能问出同一张订单」,不是密码学随机。
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    format!("{nanos:016x}{:08x}{:08x}", pid & 0xffff_ffff, seq & 0xffff_ffff)
}

/// 数字字段可能以**字符串**形式下发。
///
/// 实测 `/api/me/stock` 返回 `{"balance":"464.380000","price":"2.95","stock":12}` ——
/// 同一个响应里三个数字两种类型。Python 版的 `float()` 天然吞得下,Rust 的 `f64`
/// 反序列化会直接报 `invalid type: string`,这是重写时最容易丢的那类兼容性。
/// 所有数字字段一律走这两个宽松解析,不假设对方的类型稳定。
#[derive(Deserialize)]
#[serde(untagged)]
enum NumOrStr {
    N(f64),
    S(String),
}

fn de_f64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
    Ok(match Option::<NumOrStr>::deserialize(d)? {
        Some(NumOrStr::N(n)) => n,
        Some(NumOrStr::S(s)) => s.trim().parse().unwrap_or(0.0),
        None => 0.0,
    })
}

fn de_i64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
    Ok(match Option::<NumOrStr>::deserialize(d)? {
        Some(NumOrStr::N(n)) => n as i64,
        Some(NumOrStr::S(s)) => s.trim().parse::<f64>().map(|f| f as i64).unwrap_or(0),
        None => 0,
    })
}

#[derive(Deserialize)]
struct StockBody {
    #[serde(default, deserialize_with = "de_i64")]
    stock: i64,
    #[serde(default, deserialize_with = "de_f64")]
    price: f64,
    #[serde(default, deserialize_with = "de_f64")]
    balance: f64,
}

#[derive(Deserialize)]
struct ErrBody {
    #[serde(default)]
    error: Option<ErrDetail>,
}

#[derive(Deserialize, Default)]
struct ErrDetail {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

#[derive(Deserialize)]
struct PurchaseBody {
    #[serde(default)]
    client_order_id: String,
    #[serde(default, deserialize_with = "de_i64")]
    purchased: i64,
    #[serde(default, deserialize_with = "de_f64")]
    remaining: f64,
    #[serde(default)]
    status: String,
    #[serde(default)]
    keys: Vec<serde_json::Value>,
}

pub struct DropClient {
    /// 供应商标识,进决策流水、订单表与每家独立的熔断键。
    id: String,
    base: String,
    api_key: String,
    http: reqwest::Client,
}

impl DropClient {
    /// **不复用 `AdminState.http`** —— 那是 2s 超时的管理面客户端(为的是 worker 离线时
    /// 快速跳过),而 drop 往返实测要 3.4–4.2s。仓库里已有两处同样规避它的先例
    /// (账号探针 120s、OAuth 换码 30s)。
    /// `id` 是名册里的标识,会进订单表与该家独立的熔断键 —— 所以由调用方给,
    /// 不在这里写死 `"drop"`(同一份适配器将来可能同时接两个 drop 站点)。
    pub fn with_id(id: &str, base_url: &str, api_key: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .build()?;
        Ok(Self {
            id: id.to_string(),
            base: base_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            http,
        })
    }

    async fn call(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<String, DropError> {
        let url = format!("{}{}", self.base, path);
        let mut rb = self
            .http
            .request(method, &url)
            .header("X-API-Key", &self.api_key)
            .header("Content-Type", "application/json");
        if let Some(b) = body {
            rb = rb.json(&b);
        }
        // 错误里**不带 url** —— reqwest::Error 的 Display 会把它带出来,而 url 进日志无妨、
        // 进对外响应就是渠道来源泄漏。这里统一只写 path。
        let resp = rb
            .send()
            .await
            .map_err(|e| DropError::net(format!("drop {path} 网络失败: {}", strip_url(&e))))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if status >= 400 {
            let d = serde_json::from_str::<ErrBody>(&text)
                .ok()
                .and_then(|b| b.error)
                .unwrap_or_default();
            return Err(DropError {
                message: format!("drop {path} → HTTP {status} {} {}", d.code, d.message)
                    .trim_end()
                    .to_string(),
                status,
                code: d.code,
            });
        }
        Ok(text)
    }

    /// 库存 / 单价 / 余额。**路由是 `/api/me/stock`**,全站唯一用 `me` 的端点。
    pub async fn stock(&self) -> Result<Stock, DropError> {
        let text = self.call(reqwest::Method::GET, "/api/me/stock", None).await?;
        let b: StockBody = serde_json::from_str(&text)
            .map_err(|e| DropError::net(format!("drop 库存响应解析失败: {e}")))?;
        Ok(Stock { stock: b.stock, price_usd: b.price, balance_cny: b.balance })
    }

    /// 扣款购买。
    ///
    /// **调用前 `client_order_id` 必须已落库** —— 同 id + 同 count 可安全重试,
    /// 但前提是崩溃后还知道那个 id 是什么。
    pub async fn purchase(
        &self,
        count: i64,
        client_order_id: &str,
        max_total_cny: f64,
    ) -> Result<PurchaseResult, DropError> {
        let text = self
            .call(
                reqwest::Method::POST,
                "/api/my/purchase",
                Some(serde_json::json!({
                    "count": count,
                    "client_order_id": client_order_id,
                    "max_total_cny": max_total_cny,
                })),
            )
            .await?;
        let b: PurchaseBody = serde_json::from_str(&text)
            .map_err(|e| DropError::net(format!("drop 购买响应解析失败: {e}")))?;
        // key 可能是裸串,也可能是 {"key": "ksk_..."} 对象;只收 ksk_ 前缀的。
        // 解析细节(含为什么要顺带取 region)见 [`BoughtKey::parse`]。
        let keys = b.keys.iter().filter_map(BoughtKey::parse).collect();
        Ok(PurchaseResult {
            client_order_id: if b.client_order_id.is_empty() {
                client_order_id.to_string()
            } else {
                b.client_order_id
            },
            purchased: b.purchased,
            remaining_cny: b.remaining,
            status: b.status,
            keys,
        })
    }
}

/// drop 作为「一家供应商」的样子。
///
/// **这是纯平移,不改任何行为**:一个货架、无区域概念(`shelf_id` 与 `account_region`
/// 都是空串,落到 `import_payload` 时不写 `region`,与本抽象引入前逐字节等价)。
///
/// 报价折算:drop 的 `price` 是 USD、余额与扣款本来就是 CNY,所以只有单价要乘汇率,
/// 且沿用 [`max_total_cny_for`] 的取整口径 —— 让「面板上显示的单价」与「下单时的限价」
/// 是同一个数,免得两处差一分钱时没人说得清哪个对。
#[async_trait::async_trait]
impl super::supplier::Supplier for DropClient {
    fn id(&self) -> &str {
        &self.id
    }

    async fn survey(&self, usd_to_cny: f64) -> Result<super::supplier::Survey, String> {
        let s = self.stock().await.map_err(|e| e.to_string())?;
        Ok(super::supplier::Survey {
            supplier_id: self.id.clone(),
            balance_cny: s.balance_cny,
            balance_native: String::new(), // drop 的余额本来就是人民币,没有第二种口径
            shelves: vec![super::supplier::Shelf {
                supplier_id: self.id.clone(),
                shelf_id: String::new(),
                account_region: String::new(),
                stock: s.stock,
                unit_price_cny: max_total_cny_for(1, s.price_usd, usd_to_cny),
                // drop 不下发单笔上限;用库存兜底,真正的量闸在引擎的 max_per_purchase。
                max_per_order: s.stock.max(1),
                priority: 0, // 引擎按名册回填,适配器不表达自己的档位
            }],
        })
    }

    async fn buy(
        &self,
        _shelf: &super::supplier::Shelf,
        count: i64,
        order_id: &str,
        max_total_cny: f64,
        _usd_to_cny: f64,
    ) -> super::supplier::BuyOutcome {
        self.purchase_outcome(count, order_id, max_total_cny).await
    }

    /// drop 没有只读的查单接口,只能**重放**:用同一个 `client_order_id` 再发一次,
    /// 对方认得就返回同一张订单。这正是幂等键存在的意义。
    ///
    /// (kiroapp 那边能先只读查单再决定要不要重放,更安全 —— 见其 `reconcile`。)
    async fn reconcile(
        &self,
        order_id: &str,
        _shelf: &str,
        count: i64,
        max_total_cny: f64,
        _usd_to_cny: f64,
    ) -> super::supplier::BuyOutcome {
        self.purchase_outcome(count, order_id, max_total_cny).await
    }
}

impl DropClient {
    /// 把 [`Self::purchase`] 的 `Result` 翻译成四态结局。
    ///
    /// 三条分支的判据与本抽象引入前**逐条相同**,只是从引擎里搬到了这里:
    /// 409 = 竞争失败(不计熔断)、其余 4xx = 故障、网络失败与 5xx = 结果未知。
    async fn purchase_outcome(
        &self,
        count: i64,
        order_id: &str,
        max_total_cny: f64,
    ) -> super::supplier::BuyOutcome {
        use super::supplier::{BuyOutcome, Receipt};
        match self.purchase(count, order_id, max_total_cny).await {
            Ok(r) => BuyOutcome::Ok(Receipt {
                keys: r.keys,
                // drop 不下发单笔扣款额。留 0 表示「我说不出来」,由引擎按余额差算;
                // 引擎在**对账**路径上拿不到买前余额,会回落到订单限价(宁可高估)。
                // 早先这里的 0 会一路落成 `spent_cny = 0`,等于把一笔真实扣款
                // 从日预算里抹掉,同一天可以凭空多买一单。
                debited_cny: 0.0,
                balance_after_cny: r.remaining_cny,
                purchased: r.purchased,
                // drop 的重放会返回同一张订单,但响应里没有可区分的标记 ——
                // 所以也判断不了「幂等有没有失效」,`double_charged` 只能保守留 false。
                replayed: false,
                double_charged: false,
            }),
            Err(e) if e.is_indeterminate() => BuyOutcome::Unknown(e.to_string()),
            Err(e) if e.is_price_or_stock_conflict() => BuyOutcome::Conflict(e.to_string()),
            Err(e) => BuyOutcome::Fault(e.to_string()),
        }
    }
}

/// reqwest 错误的 Display 里带完整 URL(含 host),写进对外文案等于泄漏渠道来源。
/// 这里只保留错误类别。
fn strip_url(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "请求超时".to_string()
    } else if e.is_connect() {
        "连接失败".to_string()
    } else if e.is_decode() {
        "响应解码失败".to_string()
    } else {
        "请求失败".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 限价按上限汇率算并向上取整到分() {
        assert!((max_total_cny_for(1, 2.95, 7.2) - 21.24).abs() < 1e-9);
        // 浮点噪声:精确值 63.72,裸 ceil 会给 63.73。
        assert!((max_total_cny_for(3, 2.95, 7.2) - 63.72).abs() < 1e-9);
        // 但仍必须 ≥ 真实值,不能因为少一厘被对方判 409。
        assert!(max_total_cny_for(1, 2.951, 7.0) >= 2.951 * 7.0);
        assert!(max_total_cny_for(2, 2.20, 6.8) >= 2.0 * 2.20 * 6.8);
    }

    #[test]
    fn 幂等键是32位十六进制且不重复() {
        let a = new_order_id();
        let b = new_order_id();
        assert_eq!(a.len(), 32, "对方要求 32 位 hex,实际 {a}");
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
        assert_ne!(a, b, "同进程内连续两次不能撞");
    }

    #[test]
    fn 数字字段是字符串也能解析() {
        // 生产实测的真实响应形状:同一个响应里 balance/price 是字符串、stock 是整数。
        // 这条上线时炸过一次 —— Rust 的 f64 反序列化不像 Python 的 float() 那样宽松。
        let b: StockBody =
            serde_json::from_str(r#"{"balance":"464.380000","price":"2.95","stock":12}"#).unwrap();
        assert!((b.balance - 464.38).abs() < 1e-9);
        assert!((b.price - 2.95).abs() < 1e-9);
        assert_eq!(b.stock, 12);

        // 反过来(全是数字)也要照收 —— 对方随时可能改回去。
        let b2: StockBody =
            serde_json::from_str(r#"{"balance":100.5,"price":2.2,"stock":"7"}"#).unwrap();
        assert!((b2.balance - 100.5).abs() < 1e-9);
        assert_eq!(b2.stock, 7);

        // 缺字段 / 解析不了 → 0,不炸整个响应。库存 0 只会导致本轮不买,是安全的失败方向。
        let b3: StockBody = serde_json::from_str(r#"{"price":"abc"}"#).unwrap();
        assert_eq!(b3.stock, 0);
        assert_eq!(b3.price, 0.0);

        let p: PurchaseBody =
            serde_json::from_str(r#"{"purchased":"1","remaining":"443.32","keys":["ksk_a"]}"#)
                .unwrap();
        assert_eq!(p.purchased, 1);
        assert!((p.remaining - 443.32).abs() < 1e-9);
    }

    #[test]
    fn key条目裸串与对象都收且只放行ksk前缀() {
        let v = |s: &str| serde_json::from_str::<serde_json::Value>(s).unwrap();

        // 裸串:drop 的常见形态。region/档位留空 = 用上游默认,与改动前等价。
        let k = BoughtKey::parse(&v(r#""ksk_abc""#)).unwrap();
        assert_eq!(k.api_key, "ksk_abc");
        assert_eq!(k.region, "", "drop 不发 region,必须留空而不是猜一个");
        assert_eq!(k.subscription_title, "");

        // 对象形态:顺带取 region 与档位(kiroapp 就是这个形状)。
        let k = BoughtKey::parse(&v(
            r#"{"key":"ksk_xyz","region":"eu-central-1","subscription_title":"KIRO POWER","price":20}"#,
        ))
        .unwrap();
        assert_eq!(k.api_key, "ksk_xyz");
        assert_eq!(k.region, "eu-central-1");
        assert_eq!(k.subscription_title, "KIRO POWER");

        // 对象但缺 region → 留空,不炸。
        assert_eq!(BoughtKey::parse(&v(r#"{"key":"ksk_1"}"#)).unwrap().region, "");

        // 非 ksk_ 前缀一律丢弃 —— 与改动前的 `.filter(|k| k.starts_with("ksk_"))` 同闸。
        // 放进来的后果是建出一个跳过刷新、拿 TokenType: API_KEY 的假账号。
        for bad in [r#""usr-abc""#, r#"{"key":"rt.1.xxx"}"#, r#"{"key":""}"#, "123", "null"] {
            assert!(BoughtKey::parse(&v(bad)).is_none(), "不该收: {bad}");
        }
        // 缺 key 字段的对象。
        assert!(BoughtKey::parse(&v(r#"{"region":"eu-central-1"}"#)).is_none());
    }

    #[test]
    fn 购买响应解析出带区域的key() {
        // kiroapp 实测形状(2026-08-05):keys 是对象数组,逐 key 带 region 与 price。
        let b: PurchaseBody = serde_json::from_str(
            r#"{"purchased":1,"remaining":680,"keys":[
                 {"key":"ksk_a","region":"eu-central-1","price":20},
                 {"key":"not_a_key","region":"eu-central-1"}]}"#,
        )
        .unwrap();
        let keys: Vec<BoughtKey> = b.keys.iter().filter_map(BoughtKey::parse).collect();
        assert_eq!(keys.len(), 1, "非 ksk_ 条目必须被丢掉");
        assert_eq!(keys[0].region, "eu-central-1");
    }

    #[test]
    fn 冲突409不计熔断其余算故障() {
        let conflict = DropError { message: String::new(), status: 409, code: String::new() };
        assert!(conflict.is_price_or_stock_conflict());
        for s in [0u16, 400, 401, 500, 502] {
            let e = DropError { message: String::new(), status: s, code: String::new() };
            assert!(!e.is_price_or_stock_conflict(), "HTTP {s} 不该被当成竞争失败");
        }
    }
}
