//! Cursor 官方账期用量(`DashboardService/GetCurrentPeriodUsage`)。
//!
//! 只读 ConnectRPC JSON,用账号专属出口 + Bearer(access_token)。**不发推理**,
//! 与 Kiro 的 `getUsageLimits` 同角色:admin 账号表展示本周期已用/限额。
//!
//! 端点与字段口径对齐社区逆向(openusage / onWatch):金额单位是 **美分**,
//! 我们入库/展示换算成美元。

use gw_core::account::Account;
use gw_core::error::{UpstreamError, UpstreamErrorKind};
use gw_core::provider::{AccountQuota, OnDemandQuota, QuotaWindow};
use serde::Deserialize;

/// Connect 协议版本头(与 Run / GetServerConfig 一致)。
const CONNECT_PROTOCOL_VERSION: &str = "1";

const TIMEOUT_SECS: u64 = 20;

/// 上游用 i32 上限表示「不限额」(客户端 UI 的 Unlimited 档)。当真额度显示会变成
/// `$2147483647`,必须识别成哨兵值。
const UNLIMITED_SENTINEL: i64 = i32::MAX as i64;

/// 调一个 DashboardService 方法,回响应体文本。
///
/// 三个端点(用量 / 读超额 / 写超额)共用同一套鉴权头、超时与错误映射:上游对它们的
/// 错误语义一致(401/403 = token 废,429 = 限流,5xx = 服务端),不该各写一遍。
async fn dashboard_call(
    client: &reqwest::Client,
    account: &Account,
    api_host: &str,
    method: &str,
    body: &serde_json::Value,
) -> Result<String, UpstreamError> {
    let token = account
        .extra_str("access_token")
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            UpstreamError::new(
                UpstreamErrorKind::TokenInvalid,
                format!("cursor 账号没有 access_token,无法调 {method}"),
            )
        })?;

    let host = api_host.trim().trim_end_matches('/');
    let url = format!("https://{host}/aiserver.v1.DashboardService/{method}");

    let resp = client
        .post(&url)
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json")
        .header("connect-protocol-version", CONNECT_PROTOCOL_VERSION)
        .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
        .json(body)
        .send()
        .await
        .map_err(|e| UpstreamError::network(format!("Cursor {method} 请求失败: {e}")))?;

    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| UpstreamError::network(format!("读 Cursor {method} 响应失败: {e}")))?;

    if !status.is_success() {
        let kind = match status.as_u16() {
            401 | 403 => UpstreamErrorKind::TokenInvalid,
            429 => UpstreamErrorKind::RateLimited,
            500..=599 => UpstreamErrorKind::ServerError,
            _ => UpstreamErrorKind::Other,
        };
        // 上游把可读原因埋在 details[].debug.details 里(如「Payment method required」),
        // 直接透 message 只会得到一个没信息量的 "Error"。
        let detail = extract_error_detail(&text);
        return Err(UpstreamError::new(
            kind,
            format!(
                "Cursor {method} {}: {}",
                status.as_u16(),
                detail.unwrap_or_else(|| text.chars().take(200).collect::<String>())
            ),
        )
        .with_status(status.as_u16()));
    }

    Ok(text)
}

/// 从 Connect 错误体里抽出人能看懂的原因。
///
/// 未绑支付方式时上游回的是 `{"code":"failed_precondition","message":"Error",
/// "details":[{"debug":{"details":{"title":"Payment method required","detail":"..."}}}]}`
/// —— `message` 恒为 "Error",真正的原因在 details 里,不抽出来运维只能看到 "Error"。
fn extract_error_detail(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let d = v
        .get("details")?
        .as_array()?
        .iter()
        .find_map(|it| it.pointer("/debug/details"))?;
    let title = d.get("title").and_then(|t| t.as_str()).unwrap_or_default();
    let detail = d.get("detail").and_then(|t| t.as_str()).unwrap_or_default();
    let msg = format!("{title} {detail}").trim().to_string();
    (!msg.is_empty()).then_some(msg)
}

/// 查询当前账期用量(含超额)。`api_host` 默认 `api2.cursor.sh`。
pub async fn get_account_quota(
    client: &reqwest::Client,
    account: &Account,
    api_host: &str,
) -> Result<AccountQuota, UpstreamError> {
    let text = dashboard_call(
        client,
        account,
        api_host,
        "GetCurrentPeriodUsage",
        &serde_json::json!({}),
    )
    .await?;
    parse_period_usage(&text)
}

/// 读超额(on-demand)开关与上限:`GetHardLimit`。
pub async fn get_on_demand(
    client: &reqwest::Client,
    account: &Account,
    api_host: &str,
) -> Result<OnDemandQuota, UpstreamError> {
    let text =
        dashboard_call(client, account, api_host, "GetHardLimit", &serde_json::json!({})).await?;
    parse_hard_limit(&text)
}

/// 设超额上限:`SetHardLimit`。`limit_usd = None` 或 `Some(0)` = **关闭**超额。
///
/// 参数形态照抄官方客户端的个人号分支(团队号要带 team_id + hardLimitPerUser,
/// 这里不涉及):
/// - 开启: `{hardLimit: N, noUsageBasedAllowed: false, preserveHardLimitPerUser: true}`
/// - 关闭: `{noUsageBasedAllowed: true, preserveHardLimitPerUser: true}`(不带 hardLimit)
///
/// ⚠️ `hard_limit` 是**美元整数**(上游 i32),不是美分。
pub async fn set_on_demand(
    client: &reqwest::Client,
    account: &Account,
    api_host: &str,
    limit_usd: Option<u32>,
) -> Result<(), UpstreamError> {
    // 0 与 None 同义(客户端亦如此:金额填 0 即关闭),统一收敛成「关闭」。
    let off = limit_usd.is_none_or(|v| v == 0);
    let body = if off {
        serde_json::json!({
            "noUsageBasedAllowed": true,
            "preserveHardLimitPerUser": true,
        })
    } else {
        serde_json::json!({
            "hardLimit": limit_usd.unwrap_or_default(),
            "noUsageBasedAllowed": false,
            "preserveHardLimitPerUser": true,
        })
    };
    dashboard_call(client, account, api_host, "SetHardLimit", &body).await?;
    Ok(())
}

/// 解析 `GetHardLimit` JSON → [`OnDemandQuota`](不含已用金额,那在用量接口里)。
pub fn parse_hard_limit(body: &str) -> Result<OnDemandQuota, UpstreamError> {
    let v: HardLimitResponse = serde_json::from_str(body).map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("Cursor GetHardLimit 响应不是 JSON: {e}"),
        )
    })?;
    // proto3 零值不序列化:`noUsageBasedAllowed` 缺省 = false = **已开启**。
    let enabled = !v.no_usage_based_allowed.unwrap_or(false);
    let raw = v.hard_limit.unwrap_or(0);
    let unlimited = enabled && raw >= UNLIMITED_SENTINEL;
    Ok(OnDemandQuota {
        enabled,
        // 未开启 / 不限额 / 上游没给 → 不报额度数字(避免把哨兵值或 0 当真上限显示)。
        limit: (enabled && !unlimited && raw > 0).then_some(raw as f64),
        used: 0.0, // 由用量接口填,见 parse_period_usage。
        unlimited,
    })
}

/// 解析 `GetCurrentPeriodUsage` JSON → [`AccountQuota`](美元口径)。
pub fn parse_period_usage(body: &str) -> Result<AccountQuota, UpstreamError> {
    let v: PeriodUsageResponse = serde_json::from_str(body).map_err(|e| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            format!("Cursor 用量响应不是 JSON: {e}"),
        )
    })?;
    let plan = v.plan_usage.ok_or_else(|| {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            "Cursor 用量响应缺少 planUsage",
        )
    })?;

    // includedSpend/limit 必须同时在场:缺了就是上游改了字段或账号异常,
    // unwrap_or(0) 会把故障显示成「零用量/零额度」,比报错更难查。
    let missing = || {
        UpstreamError::new(
            UpstreamErrorKind::Other,
            "Cursor 用量响应缺 includedSpend/limit 字段",
        )
    };
    // 官方单位美分 → 美元。剩余优先用服务端 remaining,缺则 limit - includedSpend。
    let used = cents_to_usd(plan.included_spend.ok_or_else(missing)?);
    let limit = cents_to_usd(plan.limit.ok_or_else(missing)?);
    let remaining = match plan.remaining {
        Some(r) => cents_to_usd(r),
        None => limit - used,
    };
    let mut q = AccountQuota::from_used_limit(used, limit);
    // from_used_limit 重算 remaining;若服务端给了 remaining(含超额负值语义外的口径),
    // 以服务端为准,避免和 includedSpend/limit 舍入不一致。
    q.remaining = remaining;
    if limit > 0.0 {
        q.percent_used = used / limit * 100.0;
    } else if let Some(p) = plan.total_percent_used.filter(|p| p.is_finite() && *p >= 0.0) {
        q.percent_used = p;
    }
    q.currency = Some("USD".into());
    // Cursor 有**三条**用量,不止超额一条(用户 2026-08-13 点名要齐):
    //   1. 自家模型(Auto/Composer)用量 → planUsage.autoPercentUsed
    //   2. 第三方前沿模型(claude/gpt)用量 → planUsage.apiPercentUsed
    //   3. 超额(on-demand)→ spendLimitUsage(下面的 on_demand,已有)
    // 前两条上游只给百分比,不给金额拆分 —— 用 QuotaWindow 承载(它就是为
    // 「只有利用率%的多条用量」设计的,Anthropic 的 5h/7d 窗口同款),
    // 不新增 AccountQuota 字段。缺省/非法值不造窗口,绝不把故障显示成 0%。
    let mut windows = Vec::new();
    let valid = |p: &f64| p.is_finite() && *p >= 0.0;
    if let Some(p) = plan.auto_percent_used.filter(valid) {
        windows.push(QuotaWindow { label: "auto".into(), percent_used: p, reset_at: None });
    }
    if let Some(p) = plan.api_percent_used.filter(valid) {
        windows.push(QuotaWindow { label: "api".into(), percent_used: p, reset_at: None });
    }
    q.windows = windows;
    // 超额已用/上限(美分 → 美元)。`spendLimitUsage` 在未开启超额时只有 limitType,
    // 此时 limit/used 都推不出数字 → 不造 on_demand(开关状态由 GetHardLimit 定,
    // 本接口无从判断,填 enabled=false 会和「已开启但零消费」撞车)。
    q.on_demand = v.spend_limit_usage.as_ref().and_then(|s| {
        let limit = s.limit_cents();
        let used = s.used_cents();
        // 上限与已用全无 → 该号没开超额(或上游没给),不报。
        if limit.is_none() && used == 0 {
            return None;
        }
        Some(OnDemandQuota {
            // 能给出超额额度/用量,说明超额是开着的。
            enabled: true,
            limit: limit.filter(|l| *l > 0 && *l < UNLIMITED_SENTINEL).map(cents_to_usd),
            used: cents_to_usd(used),
            unlimited: limit.is_some_and(|l| l >= UNLIMITED_SENTINEL),
        })
    });
    Ok(q)
}

fn cents_to_usd(cents: i64) -> f64 {
    cents as f64 / 100.0
}

/// Protobuf JSON 把 int64 编成**字符串**(`"includedSpend":"23222"`),
/// 官方样例又是 number —— 两种都收。
fn de_i64_lenient<'de, D>(d: D) -> Result<Option<i64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Num {
        I(i64),
        S(String),
    }
    match Option::<Num>::deserialize(d)? {
        Some(Num::I(i)) => Ok(Some(i)),
        Some(Num::S(s)) => s
            .trim()
            .parse()
            .map(Some)
            .map_err(serde::de::Error::custom),
        None => Ok(None),
    }
}

/// `GetHardLimitResponse` —— 超额开关与上限。
///
/// ⚠️ `hard_limit` 单位是**美元整数**(与同 service 的 spendLimitUsage 美分口径不同)。
/// `no_usage_based_allowed` 缺省(proto3 零值)= false = 已开启超额。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HardLimitResponse {
    #[serde(default, deserialize_with = "de_i64_lenient")]
    hard_limit: Option<i64>,
    no_usage_based_allowed: Option<bool>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeriodUsageResponse {
    plan_usage: Option<PlanUsage>,
    /// 超额(on-demand)用量。**未开启超额时上游只回 `limitType`**,数字字段全缺省。
    spend_limit_usage: Option<SpendLimitUsage>,
}

/// `GetCurrentPeriodUsageResponse.SpendLimitUsage` —— 超额账期用量。
///
/// ⚠️ 单位是**美分**(与同响应的 planUsage 一致),但与 `GetHardLimit.hard_limit` 的
/// **美元**口径不同,换算别串。proto3 零值不序列化 → 无超额消费时 `individual_used`
/// 缺省,按 0 处理(不是「未知」)。
///
/// 个人号看 `individual_*`;团队号才有 `pooled_*` / `overall_*`,这里一并收下,
/// 取值时按 individual → overall 优先级回退。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SpendLimitUsage {
    #[serde(default, deserialize_with = "de_i64_lenient")]
    individual_limit: Option<i64>,
    #[serde(default, deserialize_with = "de_i64_lenient")]
    individual_used: Option<i64>,
    #[serde(default, deserialize_with = "de_i64_lenient")]
    individual_remaining: Option<i64>,
    #[serde(default, deserialize_with = "de_i64_lenient")]
    overall_limit: Option<i64>,
    #[serde(default, deserialize_with = "de_i64_lenient")]
    overall_used: Option<i64>,
    #[serde(default, deserialize_with = "de_i64_lenient")]
    overall_remaining: Option<i64>,
}

impl SpendLimitUsage {
    /// 上限(美分):个人额度优先,回退团队总额度。
    fn limit_cents(&self) -> Option<i64> {
        self.individual_limit.or(self.overall_limit)
    }

    /// 已用超额(美分)。上游给 `used` 就用它;只给 `limit`+`remaining` 时按差值推
    /// (实测未消费时 `individualUsed` 缺省、`individualRemaining` == `individualLimit`)。
    fn used_cents(&self) -> i64 {
        if let Some(u) = self.individual_used.or(self.overall_used) {
            return u;
        }
        match (self.limit_cents(), self.individual_remaining.or(self.overall_remaining)) {
            // 差值可能因上游舍入为负,clamp 到 0:超额「已用」不存在负数语义。
            (Some(l), Some(r)) => (l - r).max(0),
            _ => 0,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlanUsage {
    /// 计入套餐限额的已用(美分)。
    #[serde(default, deserialize_with = "de_i64_lenient")]
    included_spend: Option<i64>,
    /// 套餐限额(美分)。
    #[serde(default, deserialize_with = "de_i64_lenient")]
    limit: Option<i64>,
    /// 剩余(美分)。
    #[serde(default, deserialize_with = "de_i64_lenient")]
    remaining: Option<i64>,
    total_percent_used: Option<f64>,
    /// Cursor 自家模型(Auto/Composer)用量百分比 —— 面板第 1 条用量。
    auto_percent_used: Option<f64>,
    /// 第三方前沿模型(claude/gpt)用量百分比 —— 面板第 2 条用量。
    api_percent_used: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 解析官方样例_美分换美元() {
        let body = r#"{
          "billingCycleStart": "1768399334000",
          "billingCycleEnd": "1771077734000",
          "planUsage": {
            "totalSpend": 23222,
            "includedSpend": 23222,
            "bonusSpend": 0,
            "remaining": 16778,
            "limit": 40000,
            "autoPercentUsed": 0,
            "apiPercentUsed": 46.444,
            "totalPercentUsed": 58.055
          },
          "enabled": true
        }"#;
        let q = parse_period_usage(body).expect("应解析");
        assert!((q.used - 232.22).abs() < 1e-9, "used={}", q.used);
        assert!((q.limit - 400.0).abs() < 1e-9, "limit={}", q.limit);
        assert!((q.remaining - 167.78).abs() < 1e-9, "remaining={}", q.remaining);
        assert!(q.percent_used > 50.0 && q.percent_used < 60.0);
        assert!(
            q.currency.as_deref().is_some_and(|c| c.contains("USD")),
            "currency={:?}",
            q.currency
        );
        // Cursor 的三条用量里前两条(自家模型 auto / 第三方 api)只给百分比,
        // 装进 QuotaWindow —— 官方样例 autoPercentUsed=0 / apiPercentUsed=46.444。
        assert_eq!(q.windows.len(), 2, "windows={:?}", q.windows);
        assert_eq!(q.windows[0].label, "auto");
        assert!((q.windows[0].percent_used - 0.0).abs() < 1e-9);
        assert_eq!(q.windows[1].label, "api");
        assert!((q.windows[1].percent_used - 46.444).abs() < 1e-9);
    }

    /// 上游没给两条百分比时不造窗口(缺省 ≠ 0%),前端据此不渲染空进度条。
    #[test]
    fn 缺百分比字段时不造用量窗口() {
        let body = r#"{"planUsage":{"includedSpend":100,"limit":40000}}"#;
        let q = parse_period_usage(body).expect("应解析");
        assert!(q.windows.is_empty(), "windows={:?}", q.windows);
        // 非法值(负数/NaN)同样不造窗口。
        let body = r#"{"planUsage":{"includedSpend":100,"limit":40000,"autoPercentUsed":-1.0}}"#;
        let q = parse_period_usage(body).expect("应解析");
        assert!(q.windows.is_empty(), "负百分比不该进窗口: {:?}", q.windows);
    }

    #[test]
    fn 缺_planUsage_报错() {
        let err = parse_period_usage("{}").expect_err("空对象应失败");
        assert!(err.to_string().contains("planUsage"));
    }

    #[test]
    fn protobuf_json_的_int64_字符串形态也收() {
        let body = r#"{
          "planUsage": {
            "includedSpend": "23222",
            "remaining": "16778",
            "limit": "40000",
            "totalPercentUsed": 58.055
          }
        }"#;
        let q = parse_period_usage(body).expect("字符串数字应解析");
        assert!((q.used - 232.22).abs() < 1e-9, "used={}", q.used);
        assert!((q.limit - 400.0).abs() < 1e-9, "limit={}", q.limit);
        assert!((q.remaining - 167.78).abs() < 1e-9, "remaining={}", q.remaining);
    }

    #[test]
    fn 缺金额字段报错而不是显示零额度() {
        let err = parse_period_usage(r#"{"planUsage":{"totalPercentUsed":3.0}}"#)
            .expect_err("缺 includedSpend/limit 应失败");
        assert!(err.to_string().contains("includedSpend"));
    }

    // ─────────── 超额(on-demand)───────────
    // 下列样例均来自 2026-08-12 对真号(ultra-test / ultra2)的实测响应。

    /// 未开启超额:上游只回 `{"noUsageBasedAllowed":true}`(ultra2 实测)。
    #[test]
    fn 超额未开启() {
        let od = parse_hard_limit(r#"{"noUsageBasedAllowed":true}"#).expect("应解析");
        assert!(!od.enabled, "noUsageBasedAllowed=true 即未开启");
        assert_eq!(od.limit, None);
        assert!(!od.unlimited);
    }

    /// 已开启并设了 $75:回读 `{"hardLimit":75}`,**noUsageBasedAllowed 缺省即已开启**。
    /// 这条是回归闸:若把缺省当 "未开启",面板会把开着的号显示成关的。
    #[test]
    fn 超额已开启_缺省字段视为开启() {
        let od = parse_hard_limit(r#"{"hardLimit":75}"#).expect("应解析");
        assert!(od.enabled, "字段缺省 = false = 已开启");
        assert_eq!(od.limit, Some(75.0), "hardLimit 是美元整数,不换算");
        assert!(!od.unlimited);
    }

    /// i32 上限 = 客户端的「不限额」档,绝不能当成 $2147483647 的真额度显示。
    #[test]
    fn 超额不限额哨兵值() {
        let od = parse_hard_limit(r#"{"hardLimit":2147483647}"#).expect("应解析");
        assert!(od.enabled);
        assert!(od.unlimited, "i32::MAX 应识别为不限额");
        assert_eq!(od.limit, None, "不限额时不报具体数字");
    }

    /// 未开启超额时 `spendLimitUsage` 只有 limitType(ultra2 实测)→ 不造 on_demand。
    #[test]
    fn 用量响应_未开超额时不报超额() {
        let body = r#"{
          "planUsage": {"includedSpend":15158,"limit":40000,"remaining":24842},
          "spendLimitUsage": {"limitType":"user"}
        }"#;
        let q = parse_period_usage(body).expect("应解析");
        assert!(q.on_demand.is_none(), "只有 limitType 时不该报超额");
    }

    /// 已开启但零消费(ultra-test 实测):`individualUsed` **缺省**,
    /// `individualRemaining == individualLimit`。已用应为 0 而非「未知」。
    #[test]
    fn 用量响应_开了超额但零消费() {
        let body = r#"{
          "planUsage": {"includedSpend":40000,"limit":40000,"bonusSpend":1733},
          "spendLimitUsage": {"individualLimit":7500,"individualRemaining":7500,"limitType":"user"}
        }"#;
        let q = parse_period_usage(body).expect("应解析");
        let od = q.on_demand.expect("应报超额");
        assert!(od.enabled);
        assert_eq!(od.limit, Some(75.0), "7500 美分 = $75");
        assert_eq!(od.used, 0.0, "零值缺省应视为 0,不是未知");
    }

    /// 有超额消费:美分换美元,已用按 limit - remaining 推。
    #[test]
    fn 用量响应_有超额消费() {
        let body = r#"{
          "planUsage": {"includedSpend":40000,"limit":40000},
          "spendLimitUsage": {"individualLimit":7500,"individualRemaining":5250,"limitType":"user"}
        }"#;
        let q = parse_period_usage(body).expect("应解析");
        let od = q.on_demand.expect("应报超额");
        assert!((od.used - 22.5).abs() < 1e-9, "used={} 应为 $22.5", od.used);
        assert_eq!(od.limit, Some(75.0));
    }

    /// 上游直接给 `individualUsed` 时以它为准(优先于差值推算)。
    #[test]
    fn 用量响应_优先用服务端已用值() {
        let body = r#"{
          "planUsage": {"includedSpend":40000,"limit":40000},
          "spendLimitUsage": {"individualLimit":7500,"individualUsed":1234,"individualRemaining":6266,"limitType":"user"}
        }"#;
        let q = parse_period_usage(body).expect("应解析");
        let od = q.on_demand.expect("应报超额");
        assert!((od.used - 12.34).abs() < 1e-9, "used={}", od.used);
    }

    /// 团队号只给 overall_*:应回退取用,不能因为没有 individual_* 就当无超额。
    #[test]
    fn 用量响应_团队号回退_overall() {
        let body = r#"{
          "planUsage": {"includedSpend":40000,"limit":40000},
          "spendLimitUsage": {"overallLimit":20000,"overallUsed":4500,"limitType":"team"}
        }"#;
        let q = parse_period_usage(body).expect("应解析");
        let od = q.on_demand.expect("应报超额");
        assert_eq!(od.limit, Some(200.0));
        assert!((od.used - 45.0).abs() < 1e-9, "used={}", od.used);
    }

    /// 未绑支付方式时上游的 `message` 恒为 "Error",真正原因在 details 里 —— 必须抽出来,
    /// 否则运维在面板上只看到一个 "Error"(ultra2 实测响应)。
    #[test]
    fn 错误详情从_details_里抽出() {
        let body = r#"{"code":"failed_precondition","message":"Error","details":[{"type":"aiserver.v1.ErrorDetails","debug":{"error":"ERROR_BAD_REQUEST","details":{"title":"Payment method required","detail":"We need to collect a payment method before enabling on-demand.","isRetryable":false}}}]}"#;
        let d = extract_error_detail(body).expect("应抽出详情");
        assert!(d.contains("Payment method required"), "d={d}");
        assert!(d.contains("collect a payment method"), "d={d}");
    }

    /// 非 Connect 错误体(纯文本/别的 JSON)不该 panic,回 None 让调用方退回原文。
    #[test]
    fn 错误详情_无_details_时回_none() {
        assert!(extract_error_detail("not json").is_none());
        assert!(extract_error_detail(r#"{"message":"boom"}"#).is_none());
    }
}
