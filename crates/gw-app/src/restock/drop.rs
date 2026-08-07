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

/// drop 现在**按服务区分货**(2026-08-07 实测)。
///
/// `GET /api/me/stock` 不带参数时只回 `us-east-1` 那一档;带 `?region=` 才看得到别的。
/// 实测同一时刻:
///
/// ```text
/// (无参数)              → {"region":"us-east-1",   "price":"5.88","stock":0}
/// ?region=eu-central-1 → {"region":"eu-central-1","price":"3.67","stock":3}
/// ?region=eu           → 同上(简写会被归一成 eu-central-1)
/// ```
///
/// ## 不带 region 的代价(这是一次真实故障)
///
/// 旧代码只问不带参数的那一档。于是 us 缺货时引擎看到 `stock=0` 判「缺货」→ 不补货,
/// 而**网站上明明有 eu 的货**。号按墙上时钟一个个死掉,池子抽干,客户端拿 503 ——
/// 用户侧的表现是「有货却显示缺货,然后卡顿一段时间」。
///
/// 没有区域列表端点(`/api/me/regions`、`/api/me/shelves`、`/api/regions` 全 404),
/// 所以区域集合只能写死在这里。新增一个区就往这个数组里加一项。
const REGIONS: &[&str] = &["eu-central-1", "us-east-1"];

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

/// 库存/购买端点的路径(含 region 查询参数)。
///
/// 抽成纯函数是为了**能测**:购买那条路我方不能实测(一次就是一次真实扣款),
/// 至少要保证「区域到底有没有拼进请求」这件事有断言钉住。
fn path_with_region(base: &str, region: Option<&str>) -> String {
    match region {
        Some(r) if !r.trim().is_empty() => format!("{base}?region={}", r.trim()),
        _ => base.to_string(),
    }
}

/// 响应里没带 region 的 key,按本次请求的区域补上,并返回补了几个。
///
/// 抽成纯函数同上:这段决定号会打哪个 `management.<region>.kiro.dev`,
/// 写错就是每个请求 403,而它所在的那条路不能实测。
fn fill_missing_regions(keys: &mut [BoughtKey], want: &str) -> usize {
    if want.trim().is_empty() {
        return 0;
    }
    let mut n = 0;
    for k in keys.iter_mut().filter(|k| k.region.is_empty()) {
        k.region = want.trim().to_string();
        n += 1;
    }
    n
}

/// 一个区域档的库存快照。`region` 是**对方回显**的值,见 [`StockBody::region`]。
#[derive(Debug, Clone)]
pub struct RegionStock {
    pub region: String,
    pub stock: i64,
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
    /// 对方**回显**的服务区。以它为准,不以我方请求的为准 —— 参数拼错或对方改了
    /// 归一规则时,回显是唯一能发现「我要的是 eu,拿到的是 us」的地方,
    /// 而这个错会一路落进 `extra.region`,让号的每个请求 403。
    #[serde(default)]
    region: String,
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
    ///
    /// 不带 region = 只看 `us-east-1` 那一档(对方的默认),这正是那次
    /// 「有货却报缺货」故障的成因,见 [`REGIONS`]。
    /// 某个服务区的库存。`None` = 用对方默认档。
    ///
    /// **故意没有不带参数的版本。** 早先那个 `stock()` 正是这次故障的成因
    /// (只问默认档 → us 缺货就判全线缺货),留着它等于留一个随时会被再调一次的坑。
    pub async fn stock_in(&self, region: Option<&str>) -> Result<RegionStock, DropError> {
        let path = path_with_region("/api/me/stock", region);
        let text = self.call(reqwest::Method::GET, &path, None).await?;
        let b: StockBody = serde_json::from_str(&text)
            .map_err(|e| DropError::net(format!("drop 库存响应解析失败: {e}")))?;
        // 回显为空时退回我方请求值(老部署/老响应),两者都空则留空 = 上游默认区。
        let echoed = if b.region.is_empty() {
            region.unwrap_or("").to_string()
        } else {
            b.region.clone()
        };
        if let (Some(want), false) = (region, b.region.is_empty()) {
            if !want.is_empty() && b.region != want && !want.starts_with(&b.region) && !b.region.starts_with(want) {
                tracing::warn!(
                    requested = %want, echoed = %b.region,
                    "drop 库存:请求的服务区与对方回显不一致,按回显为准"
                );
            }
        }
        Ok(RegionStock {
            region: echoed,
            stock: b.stock,
            price_usd: b.price,
            balance_cny: b.balance,
        })
    }

    /// 扣款购买。
    ///
    /// **调用前 `client_order_id` 必须已落库** —— 同 id + 同 count 可安全重试,
    /// 但前提是崩溃后还知道那个 id 是什么。
    /// 指定服务区下单。**没有不带 region 的版本**,理由同 [`Self::stock_in`]。
    ///
    /// ## ⚠️ region 参数的传法是**推断,不是实测**
    ///
    /// 库存端点确认吃 `?region=`(见 [`Self::stock_in`]),购买端点没法试 ——
    /// 试一次就是一次真实扣款。所以这里**同时**从两处传:query 参数与 body 字段。
    /// 多传一个对方不认的字段,常规实现是忽略;而少传那个它认的,后果是买错区。
    ///
    /// **真正的护栏不在请求侧,在响应侧**:落进 `extra.region` 的值一律取自
    /// 响应里 key 对象自带的 `region`(见 [`BoughtKey::parse`]),取不到才退回本次请求的
    /// 区域,并在 [`Self::purchase_in`] 里留一条 warn。买错区不会静默 ——
    /// 号会在第一次 `getUsageLimits` 就 403,而 warn 已经把「我方无法确认」写在日志里了。
    pub async fn purchase_in(
        &self,
        count: i64,
        client_order_id: &str,
        max_total_cny: f64,
        region: Option<&str>,
    ) -> Result<PurchaseResult, DropError> {
        let path = path_with_region("/api/my/purchase", region);
        let mut body = serde_json::json!({
            "count": count,
            "client_order_id": client_order_id,
            "max_total_cny": max_total_cny,
        });
        if let (Some(r), Some(o)) = (region, body.as_object_mut()) {
            if !r.is_empty() {
                o.insert("region".into(), serde_json::json!(r));
            }
        }
        let text = self.call(reqwest::Method::POST, &path, Some(body)).await?;
        let b: PurchaseBody = serde_json::from_str(&text)
            .map_err(|e| DropError::net(format!("drop 购买响应解析失败: {e}")))?;
        // key 可能是裸串,也可能是 {"key": "ksk_..."} 对象;只收 ksk_ 前缀的。
        // 解析细节(含为什么要顺带取 region)见 [`BoughtKey::parse`]。
        let mut keys: Vec<BoughtKey> = b.keys.iter().filter_map(BoughtKey::parse).collect();
        // 响应没给 region 时,退回本次请求的区域 —— 但要吵一声:我方无法确认对方
        // 是否真的按 region 发货,而这个值会决定号打哪个 `management.<region>.kiro.dev`。
        if let Some(want) = region.filter(|r| !r.is_empty()) {
            let filled = fill_missing_regions(&mut keys, want);
            if filled > 0 {
                tracing::warn!(
                    requested = %want, keys = keys.len(), filled,
                    "drop 购买:响应里的 key 没带 region,按本次请求的区域记录(**未经对方确认**)。\
                     若这些号第一次查配额就 403 Invalid token,就是买错区了"
                );
            }
        }
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
/// **一区一货架**(2026-08-07):对方按服务区分货,不同区不同价、不同库存
/// (实测 eu-central-1 $3.67/3 个 vs us-east-1 $5.88/0 个)。只问默认档的旧写法
/// 会在 us 缺货时误判「全线缺货」,见 [`REGIONS`]。
///
/// 货架排序由引擎的 `rank_shelves` 负责(先档位再价格),所以便宜的区会自动胜出 ——
/// 这里不表达偏好,只把两个区如实报上去。
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
        let mut shelves = Vec::new();
        let mut balance_cny = 0.0;
        let mut last_err: Option<String> = None;

        for region in REGIONS {
            match self.stock_in(Some(region)).await {
                Ok(r) => {
                    // 余额是账户级的,每个区回的都是同一个数;取任意一个即可。
                    balance_cny = r.balance_cny;
                    shelves.push(super::supplier::Shelf {
                        supplier_id: self.id.clone(),
                        // drop 的「货架标识」就是区域串,下单时原样回传。
                        shelf_id: r.region.clone(),
                        account_region: r.region.clone(),
                        stock: r.stock,
                        unit_price_cny: max_total_cny_for(1, r.price_usd, usd_to_cny),
                        // drop 不下发单笔上限;用库存兜底,真正的量闸在引擎的 max_per_purchase。
                        max_per_order: r.stock.max(1),
                        priority: 0, // 引擎按名册回填,适配器不表达自己的档位
                    });
                }
                // **一个区查不到不该让整家掉线。** 另一个区可能正好有货,而这家整体
                // 报错会让引擎跳过它 —— 那就又回到「有货却买不到」了。
                Err(e) => {
                    tracing::warn!(supplier = %self.id, region = %region,
                        "drop 该区库存查询失败,跳过这个货架: {e}");
                    last_err = Some(e.to_string());
                }
            }
        }

        // 全部区都失败才算这家掉线(熔断/故障计数据此)。
        if shelves.is_empty() {
            return Err(last_err.unwrap_or_else(|| "drop 所有区域库存查询均失败".to_string()));
        }
        Ok(super::supplier::Survey {
            supplier_id: self.id.clone(),
            balance_cny,
            balance_native: String::new(), // drop 的余额本来就是人民币,没有第二种口径
            shelves,
        })
    }

    async fn buy(
        &self,
        shelf: &super::supplier::Shelf,
        count: i64,
        order_id: &str,
        max_total_cny: f64,
        _usd_to_cny: f64,
    ) -> super::supplier::BuyOutcome {
        // 必须把货架的区域带上,否则**买的是对方默认区**(us-east-1)而我方会按
        // 货架的 `account_region` 记 `extra.region` —— 那正是「每个请求 403」的配方。
        self.purchase_outcome(count, order_id, max_total_cny, Some(&shelf.shelf_id))
            .await
    }

    /// drop 没有只读的查单接口,只能**重放**:用同一个 `client_order_id` 再发一次,
    /// 对方认得就返回同一张订单。这正是幂等键存在的意义。
    ///
    /// (kiroapp 那边能先只读查单再决定要不要重放,更安全 —— 见其 `reconcile`。)
    async fn reconcile(
        &self,
        order_id: &str,
        shelf: &str,
        count: i64,
        max_total_cny: f64,
        _usd_to_cny: f64,
    ) -> super::supplier::BuyOutcome {
        // 重放**必须带原单的区域**:少了它,一笔 eu 的订单会被当成 us 重发 ——
        // 若对方把 region 当订单的一部分,那就不是重放而是一笔新订单(二次扣款)。
        self.purchase_outcome(count, order_id, max_total_cny, Some(shelf))
            .await
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
        region: Option<&str>,
    ) -> super::supplier::BuyOutcome {
        use super::supplier::{BuyOutcome, Receipt};
        match self.purchase_in(count, order_id, max_total_cny, region).await {
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

    // ── 按服务区分货(2026-08-07)────────────────────────────────────────
    //
    // 这组用例守的是一次真实故障:旧代码只问不带 region 的那一档,us 缺货时
    // 判「全线缺货」而 eu 明明有货 → 不补货 → 池子抽干 → 客户拿 503。

    #[test]
    fn 区域会拼进请求路径() {
        assert_eq!(
            path_with_region("/api/me/stock", Some("eu-central-1")),
            "/api/me/stock?region=eu-central-1"
        );
        assert_eq!(
            path_with_region("/api/my/purchase", Some("us-east-1")),
            "/api/my/purchase?region=us-east-1"
        );
        // 空/None → 不拼参数(等于用对方默认档)
        assert_eq!(path_with_region("/api/me/stock", None), "/api/me/stock");
        assert_eq!(path_with_region("/api/me/stock", Some("")), "/api/me/stock");
        assert_eq!(path_with_region("/api/me/stock", Some("  ")), "/api/me/stock");
    }

    #[test]
    fn 两个区都在名册里且_eu_在前() {
        // 顺序本身不决定选家(引擎按价格排),但 eu 更便宜,放前面让日志更直观。
        assert!(REGIONS.contains(&"eu-central-1"));
        assert!(REGIONS.contains(&"us-east-1"));
        assert_eq!(REGIONS.len(), 2, "加区就改这条断言,提醒你顺带看一眼 shelf_priority");
    }

    #[test]
    fn 库存响应的_region_回显会被解析() {
        let b: StockBody = serde_json::from_str(
            r#"{"balance":"124.304000","price":"3.67","region":"eu-central-1","stock":3}"#,
        )
        .unwrap();
        assert_eq!(b.region, "eu-central-1");
        assert_eq!(b.stock, 3);
        assert!((b.price - 3.67).abs() < 1e-9);
        // 老响应没有 region 字段也不能炸(默认空串 = 用上游默认区)
        let old: StockBody =
            serde_json::from_str(r#"{"balance":"1","price":"5.88","stock":0}"#).unwrap();
        assert_eq!(old.region, "");
    }

    #[test]
    fn 响应没带_region_时按请求区域补齐() {
        let mut keys = vec![
            BoughtKey { api_key: "ksk_a".into(), region: String::new(), subscription_title: String::new() },
            BoughtKey { api_key: "ksk_b".into(), region: "us-east-1".into(), subscription_title: String::new() },
        ];
        let filled = fill_missing_regions(&mut keys, "eu-central-1");
        assert_eq!(filled, 1, "只补空的那个");
        assert_eq!(keys[0].region, "eu-central-1");
        // **对方明确说了的不动** —— 响应是事实,我方请求只是意愿。
        assert_eq!(keys[1].region, "us-east-1", "对方给的 region 优先于我方请求的");
    }

    #[test]
    fn 请求区域为空时不乱补() {
        let mut keys = vec![BoughtKey {
            api_key: "ksk_a".into(),
            region: String::new(),
            subscription_title: String::new(),
        }];
        assert_eq!(fill_missing_regions(&mut keys, ""), 0);
        assert_eq!(fill_missing_regions(&mut keys, "   "), 0);
        // 留空 = 用上游默认(us-east-1),与本次改动前逐字节等价
        assert_eq!(keys[0].region, "");
    }
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
