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
}

#[derive(Debug, Clone, Copy)]
pub struct Stock {
    pub stock: i64,
    /// **USD**。对方报价用美元、扣款走人民币,别把它当人民币用。
    pub price_usd: f64,
    pub balance_cny: f64,
}

#[derive(Debug, Clone)]
pub struct PurchaseResult {
    pub client_order_id: String,
    pub purchased: i64,
    pub remaining_cny: f64,
    pub status: String,
    pub keys: Vec<String>,
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
    base: String,
    api_key: String,
    http: reqwest::Client,
}

impl DropClient {
    /// **不复用 `AdminState.http`** —— 那是 2s 超时的管理面客户端(为的是 worker 离线时
    /// 快速跳过),而 drop 往返实测要 3.4–4.2s。仓库里已有两处同样规避它的先例
    /// (账号探针 120s、OAuth 换码 30s)。
    pub fn new(base_url: &str, api_key: &str) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(TIMEOUT_SECS))
            .build()?;
        Ok(Self {
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
        let keys = b
            .keys
            .iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(o) => {
                    o.get("key").and_then(|k| k.as_str()).map(str::to_string)
                }
                _ => None,
            })
            .filter(|k| k.starts_with("ksk_"))
            .collect();
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
    fn 冲突409不计熔断其余算故障() {
        let conflict = DropError { message: String::new(), status: 409, code: String::new() };
        assert!(conflict.is_price_or_stock_conflict());
        for s in [0u16, 400, 401, 500, 502] {
            let e = DropError { message: String::new(), status: s, code: String::new() };
            assert!(!e.is_price_or_stock_conflict(), "HTTP {s} 不该被当成竞争失败");
        }
    }
}
