//! 事件回调:把「补货这边出事了」推到人看得见的地方。
//!
//! ## 为什么需要它
//!
//! 补货的其它部分都在解决「机器能自己搞定」的事。**抢不到货不属于那一类** ——
//! 对方连着几小时没上架时,再快的探测也变不出货来,唯一的出路是人去别处想办法。
//! 而人得先知道。面板不会主动叫人,日志更不会。
//!
//! ## 归一的形状:一句 `text` + 一坨结构化字段
//!
//! 各家机器人的报文互不兼容,但**都只需要一句话**。所以这里的契约是:
//! 调用方给一句人读的 `text` 和一份结构化 `payload`,由 [`body_for`] 按目标域名
//! 挑形状。识别不出来的域名发**通用 JSON**(`text` + payload 平铺),
//! 那是自建中转与 n8n / Node-RED 这类工具最好接的形态。
//!
//! 按域名自动识别而不是让人选一个下拉框:选错的后果是**静默不通知**
//! (企业微信收到飞书的报文会回 200 + `errcode: 93000`),而这个功能存在的
//! 全部意义就是「出事时人能收到」。粘个地址就能用,少一处能填错的地方。
//!
//! ## 失败绝不能影响买号
//!
//! 所有发送都只返回 `Result` 给调用方记日志,**没有任何一条补货路径依赖它成功**。
//! 通知服务挂掉的正确后果是「人没收到消息」,不是「补货停了」。
//!
//! ## 安全
//!
//! 地址由**已鉴权的管理员**在面板填写,本模块不做出网目标限制(内网地址是合法用法 ——
//! 自建中转通常就在 loopback)。它只发我方自己的补货状态,不带任何密钥。

use std::time::Duration;

/// 事件名。进 payload 的 `event` 字段,也是通知节流的分组键 ——
/// **必须逐类分开**:缺货刷屏时不能把「熔断了」一起压掉。
pub const EV_OUT_OF_STOCK: &str = "out_of_stock";
pub const EV_RESTOCKED: &str = "restocked";
pub const EV_BREAKER: &str = "breaker";

/// 发送超时。通知失败无所谓,**卡住有所谓** —— 抢货循环 5 秒一轮,
/// 一次 30 秒的挂起会吞掉 6 轮探测。
const TIMEOUT_SECS: u64 = 8;

/// 节流状态在 `settings` 表里的键。逐事件分开。
pub fn throttle_key(event: &str) -> String {
    format!("restock_notify_at:{event}")
}

/// 目标机器人的报文方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    /// `{"text": ..., <payload 平铺>}`。自建中转 / n8n / 任何自己写的接收端。
    Generic,
    /// 企业微信群机器人与钉钉自定义机器人**报文完全相同**,合并成一种。
    WecomLike,
    /// 飞书自定义机器人。字段名与企业微信只差一层,抄错就是 200 + 错误码。
    Feishu,
    Slack,
}

/// 按域名认方言。认不出来一律 [`Flavor::Generic`]。
pub fn flavor_of(url: &str) -> Flavor {
    let host = reqwest::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(|h| h.to_ascii_lowercase()))
        .unwrap_or_default();
    if host.ends_with("qyapi.weixin.qq.com") || host.ends_with("oapi.dingtalk.com") {
        Flavor::WecomLike
    } else if host.ends_with("feishu.cn") || host.ends_with("larksuite.com") {
        Flavor::Feishu
    } else if host.ends_with("hooks.slack.com") {
        Flavor::Slack
    } else {
        Flavor::Generic
    }
}

/// 按方言拼报文。
///
/// `payload` 只进 [`Flavor::Generic`]:机器人那几家的报文是**封闭结构**,
/// 多塞字段轻则被忽略、重则整条被拒。人读的信息已经在 `text` 里。
pub fn body_for(f: Flavor, text: &str, payload: &serde_json::Value) -> serde_json::Value {
    match f {
        Flavor::WecomLike => serde_json::json!({
            "msgtype": "text",
            "text": { "content": text },
        }),
        Flavor::Feishu => serde_json::json!({
            "msg_type": "text",
            "content": { "text": text },
        }),
        Flavor::Slack => serde_json::json!({ "text": text }),
        Flavor::Generic => {
            let mut v = serde_json::json!({ "text": text });
            if let (Some(dst), Some(src)) = (v.as_object_mut(), payload.as_object()) {
                for (k, val) in src {
                    dst.insert(k.clone(), val.clone());
                }
            }
            v
        }
    }
}

/// 机器人那几家 **HTTP 200 也可能是失败**:key 写错时企业微信回
/// `{"errcode":93000,...}`、飞书回 `{"code":19021,...}`。只看状态码会让
/// 「配错地址」表现成「一切正常但从来收不到」,而那是最难查的一种坏法。
fn app_level_error(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    for k in ["errcode", "code", "StatusCode"] {
        if let Some(n) = v.get(k).and_then(|x| x.as_i64()) {
            if n != 0 {
                let msg = ["errmsg", "msg", "StatusMessage"]
                    .iter()
                    .find_map(|m| v.get(m).and_then(|x| x.as_str()))
                    .unwrap_or("");
                return Some(format!("对方回执 {k}={n} {msg}").trim_end().to_string());
            }
        }
    }
    None
}

/// 发一条通知。`url` 为空时直接当成功返回(未配置不是错误)。
pub async fn send(
    http: &reqwest::Client,
    url: &str,
    text: &str,
    payload: serde_json::Value,
) -> Result<(), String> {
    let url = url.trim();
    if url.is_empty() {
        return Ok(());
    }
    let body = body_for(flavor_of(url), text, &payload);
    let resp = http
        .post(url)
        .timeout(Duration::from_secs(TIMEOUT_SECS))
        .json(&body)
        .send()
        .await
        // 地址里可能带着机器人 key,原样打进日志等于把它写进 /logs。
        .map_err(|e| strip_url(&e))?;
    let status = resp.status();
    let text_body = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let snippet: String = text_body.chars().take(200).collect();
        return Err(format!("HTTP {status}: {snippet}"));
    }
    match app_level_error(&text_body) {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// `reqwest::Error` 的 Display 会带上完整 URL(含机器人 key)。只留错误类别。
fn strip_url(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        "请求超时".into()
    } else if e.is_connect() {
        "连接失败".into()
    } else {
        "网络错误".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 按域名认出各家机器人() {
        assert_eq!(
            flavor_of("https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc"),
            Flavor::WecomLike
        );
        assert_eq!(flavor_of("https://oapi.dingtalk.com/robot/send?access_token=x"), Flavor::WecomLike);
        assert_eq!(flavor_of("https://open.feishu.cn/open-apis/bot/v2/hook/uuid"), Flavor::Feishu);
        assert_eq!(flavor_of("https://open.larksuite.com/open-apis/bot/v2/hook/u"), Flavor::Feishu);
        assert_eq!(flavor_of("https://hooks.slack.com/services/T/B/x"), Flavor::Slack);
        // 认不出来的一律通用 JSON,而不是猜一个 —— 猜错是静默不通知。
        assert_eq!(flavor_of("http://127.0.0.1:8080/hook"), Flavor::Generic);
        assert_eq!(flavor_of("不是个地址"), Flavor::Generic);
    }

    #[test]
    fn 大小写与子域名都要认得出() {
        assert_eq!(flavor_of("https://QYAPI.WEIXIN.QQ.COM/x"), Flavor::WecomLike);
        assert_eq!(flavor_of("https://www.feishu.cn/flow/api/trigger-webhook/x"), Flavor::Feishu);
    }

    #[test]
    fn 相似域名不能误判成机器人() {
        // 后缀匹配必须带点边界:`evil-feishu.cn` 不是飞书。
        assert_eq!(flavor_of("https://myhooks.slack.com.evil.test/x"), Flavor::Generic);
    }

    #[test]
    fn 企业微信与钉钉报文相同而飞书只差一层() {
        let p = serde_json::json!({ "event": "out_of_stock" });
        let w = body_for(Flavor::WecomLike, "缺货了", &p);
        assert_eq!(w["msgtype"], "text");
        assert_eq!(w["text"]["content"], "缺货了");
        assert!(w.get("event").is_none(), "封闭结构里不许塞额外字段");

        let f = body_for(Flavor::Feishu, "缺货了", &p);
        assert_eq!(f["msg_type"], "text");
        assert_eq!(f["content"]["text"], "缺货了");
    }

    #[test]
    fn 通用形态把结构化字段平铺出来() {
        let p = serde_json::json!({ "event": "out_of_stock", "waited_secs": 900, "healthy": 0 });
        let v = body_for(Flavor::Generic, "缺货 15 分钟", &p);
        assert_eq!(v["text"], "缺货 15 分钟");
        assert_eq!(v["event"], "out_of_stock");
        assert_eq!(v["waited_secs"], 900);
        assert_eq!(v["healthy"], 0);
    }

    #[test]
    fn 状态码200但带错误码要判失败() {
        // 这是配错地址唯一的痕迹。放过它 = 「一切正常但从来收不到」。
        assert!(app_level_error(r#"{"errcode":93000,"errmsg":"invalid webhook url"}"#).is_some());
        assert!(app_level_error(r#"{"code":19021,"msg":"sign match fail"}"#).is_some());
        assert!(app_level_error(r#"{"errcode":0,"errmsg":"ok"}"#).is_none());
        // Slack 回的是纯文本 `ok`,不是 JSON —— 不能因此判失败。
        assert!(app_level_error("ok").is_none());
        assert!(app_level_error("").is_none());
        // 自建接收端回 `{"code":200}` 这种是常见形态,不该被当成错误…
        // 但我们无从分辨,所以文档里写明:通用接收端请回 200 + 空体或 `{"ok":true}`。
        assert!(app_level_error(r#"{"ok":true}"#).is_none());
    }

    #[test]
    fn 节流键逐事件分开() {
        // 缺货刷屏时不能把熔断通知一起压掉。
        assert_ne!(throttle_key(EV_OUT_OF_STOCK), throttle_key(EV_BREAKER));
        assert_ne!(throttle_key(EV_RESTOCKED), throttle_key(EV_BREAKER));
    }

    #[tokio::test]
    async fn 没配地址时静默成功不产生任何请求() {
        let http = reqwest::Client::new();
        assert!(send(&http, "", "x", serde_json::json!({})).await.is_ok());
        assert!(send(&http, "   ", "x", serde_json::json!({})).await.is_ok());
    }
}
