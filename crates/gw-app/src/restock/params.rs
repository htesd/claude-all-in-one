//! 补货的**运行时可调参数**。整段 JSON 存在 control.db 的 `settings` 表
//! (键 `restock_params`),面板改完即时生效、无需重启。
//!
//! **为什么不放 `SystemSettings`**:那个结构体有一条回滚地板(见其注释)——
//! 2026-07-31 之前的镜像仍带 `deny_unknown_fields`,给它加字段会让回滚变成全量 503。
//! 补货参数走自己的键天然避开,顺带也不会经 `GET /admin/api/settings` 回显。
//!
//! **默认值必须是「不花钱」**:`enabled` 与 `dry_run` 的默认组合是「关着 + 演练」,
//! 部署完不会有任何扣款,必须人为打开。

use serde::{Deserialize, Serialize};

/// 参数上下限。面板渲染、写入校验、默认值三处共用这张表,避免
/// 「面板能填 999999 但引擎只认到 1000」这类分叉。
pub struct Bound {
    pub key: &'static str,
    pub kind: &'static str, // bool / int / float / hhmm
    pub min: f64,
    pub max: f64,
    pub label: &'static str,
    pub hint: &'static str,
}

pub const BOUNDS: &[Bound] = &[
    Bound { key: "enabled", kind: "bool", min: 0.0, max: 0.0,
        label: "自动补货", hint: "总开关。关闭后只观察不购买" },
    Bound { key: "dry_run", kind: "bool", min: 0.0, max: 0.0,
        label: "DRY-RUN 演练", hint: "跑完整决策链但不真正扣款" },
    Bound { key: "min_healthy", kind: "int", min: 0.0, max: 100.0,
        label: "补货水位", hint: "状态「正常」的 ksk_ 号少于此值才补" },
    Bound { key: "max_per_purchase", kind: "int", min: 1.0, max: 10.0,
        label: "单次购买量", hint: "每次最多买几个。号成批死亡,批量买不产生缓冲" },
    Bound { key: "daily_cap_cny", kind: "float", min: 0.0, max: 100000.0,
        label: "日花费上限 ¥", hint: "当日实际扣款达此值即停买" },
    Bound { key: "rate_cap", kind: "float", min: 1.0, max: 50.0,
        label: "限价汇率", hint: "算 max_total_cny 用的 USD→CNY 上限汇率" },
    Bound { key: "min_balance_reserve_cny", kind: "float", min: 0.0, max: 100000.0,
        label: "余额保留 ¥", hint: "余额低于「本单价 + 此值」就不买" },
    Bound { key: "import_fail_breaker", kind: "int", min: 1.0, max: 20.0,
        label: "熔断阈值", hint: "连续几次「买到却没上号」即熔断停买" },
    Bound { key: "max_price_usd", kind: "float", min: 0.01, max: 100.0,
        label: "最高买入价 $", hint: "异常兜底,不是择价工具。超过即不买" },
    Bound { key: "poll_interval_secs", kind: "int", min: 10.0, max: 3600.0,
        label: "轮询间隔 秒", hint: "多久跑一次决策" },
    Bound { key: "grave_ttl_hours", kind: "int", min: 1.0, max: 8760.0,
        label: "回收时限 小时", hint: "自购死号存在多久后删除。只回收自己买的号" },
    Bound { key: "peak_start", kind: "hhmm", min: 0.0, max: 0.0,
        label: "高峰起", hint: "HH:MM" },
    Bound { key: "peak_end", kind: "hhmm", min: 0.0, max: 0.0,
        label: "高峰止", hint: "HH:MM。小于起点表示跨零点,如 09:00–02:00" },
    Bound { key: "utc_offset_minutes", kind: "int", min: -720.0, max: 840.0,
        label: "时区偏移 分钟", hint: "高峰窗口与周画像按此换算本地时间。东八区 = 480" },
    Bound { key: "new_account_concurrency", kind: "int", min: 1.0, max: 256.0,
        label: "新号并发", hint: "建号时被硬编码为 2,上号后补到此值" },
    Bound { key: "new_account_queue_enabled", kind: "bool", min: 0.0, max: 0.0,
        label: "新号排队模式", hint: "429 时排队而非让账号下线,尽量把额度榨完" },
    Bound { key: "forecast_hours", kind: "int", min: 1.0, max: 72.0,
        label: "预测时长 小时", hint: "用「星期几×钟点」画像预测未来多少小时" },
    Bound { key: "idle_skip_ratio", kind: "float", min: 0.0, max: 1.0,
        label: "闲时抑制阈值", hint: "预测消耗低于历史峰值的此比例时不补。0 = 关闭" },
];

fn d_false() -> bool { false }
fn d_true() -> bool { true }
fn d_1() -> i64 { 1 }
fn d_200() -> f64 { 200.0 }
fn d_rate() -> f64 { 7.2 }
fn d_reserve() -> f64 { 40.0 }
fn d_2() -> i64 { 2 }
fn d_price() -> f64 { 6.0 }
fn d_60() -> i64 { 60 }
fn d_24() -> i64 { 24 }
fn d_peak_start() -> String { "09:00".into() }
fn d_peak_end() -> String { "02:00".into() }
fn d_offset() -> i64 { 480 }
fn d_conc() -> i64 { 100 }
fn d_forecast() -> i64 { 12 }
fn d_zero_f() -> f64 { 0.0 }
fn d_group() -> String { "G0".into() }
fn d_member_groups() -> Vec<String> { vec!["G0@0".into(), "GECO@0".into(), "GLOW@0".into()] }
fn d_egress() -> String { "auto".into() }

/// 补货运行时参数。缺字段一律取默认(老库、老版本写入的 JSON 都能读)。
///
/// ⚠️ **不要在结构体上加 `#[serde(default)]`**:那会让缺字段回落到
/// `Params::default()`,而下面的 `Default` 实现正是靠反序列化 `{}` 来的 —— 无限递归、
/// 栈溢出。每个字段各自带 `default = "..."` 才是单一真源。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Params {
    /// 业务总开关。**默认关** —— 部署完不会花钱,必须在面板打开。
    #[serde(default = "d_false")]
    pub enabled: bool,
    /// 演练模式:跑完整决策链、写流水,但不真正扣款。**默认开**。
    #[serde(default = "d_true")]
    pub dry_run: bool,

    /// 状态「正常」的 ksk_ 号少于此值才补。
    ///
    /// 固定为 1 而不是「留个缓冲」:Kiro 号**成批死亡**,一次买 3 个不等于 3 倍缓冲,
    /// 只等于 3 倍花费和同一个归零时刻。要缓冲只能靠错开购买时间。
    #[serde(default = "d_1")]
    pub min_healthy: i64,
    #[serde(default = "d_1")]
    pub max_per_purchase: i64,

    #[serde(default = "d_200")]
    pub daily_cap_cny: f64,
    #[serde(default = "d_rate")]
    pub rate_cap: f64,
    #[serde(default = "d_reserve")]
    pub min_balance_reserve_cny: f64,
    #[serde(default = "d_2")]
    pub import_fail_breaker: i64,
    /// 最高买入价(USD)。**异常兜底,不是择价工具** —— 运营决定「有货就买不等降价」,
    /// 断供的代价高于那点差价。默认 6.0 ≈ 正常价($2.20–$2.95)的两倍以上。
    #[serde(default = "d_price")]
    pub max_price_usd: f64,

    #[serde(default = "d_60")]
    pub poll_interval_secs: i64,
    #[serde(default = "d_24")]
    pub grave_ttl_hours: i64,

    #[serde(default = "d_peak_start")]
    pub peak_start: String,
    #[serde(default = "d_peak_end")]
    pub peak_end: String,
    /// 本地时区相对 UTC 的偏移(分钟)。东八区 = 480。
    /// 高峰窗口判定与周画像都按它换算,见 `forecast` 模块开头关于「为什么不引 chrono-tz」。
    #[serde(default = "d_offset")]
    pub utc_offset_minutes: i64,

    /// 新号导入进哪个组(`accounts.group_name`,决定归属)。
    #[serde(default = "d_group")]
    pub import_group: String,
    /// 新号要挂的成员边,形如 `G0@0`。**组内优先级的事实源是成员边**
    /// (`account_groups.priority`),不是账号的 `extra.priority`。
    #[serde(default = "d_member_groups")]
    pub member_groups: Vec<String>,
    #[serde(default = "d_egress")]
    pub egress: String,
    #[serde(default = "d_conc")]
    pub new_account_concurrency: i64,
    #[serde(default = "d_true")]
    pub new_account_queue_enabled: bool,

    #[serde(default = "d_forecast")]
    pub forecast_hours: i64,
    /// 闲时抑制:预测消耗低于历史峰值的此比例时不补货。**0 = 关闭**。
    /// 这道闸**只会阻止购买、永不促成购买**,所以预测错了最坏也只是晚买。
    #[serde(default = "d_zero_f")]
    pub idle_skip_ratio: f64,
}

impl Default for Params {
    fn default() -> Self {
        serde_json::from_str("{}").expect("Params 的 serde 默认值必须自洽")
    }
}

impl Params {
    pub fn utc_offset_secs(&self) -> i64 {
        self.utc_offset_minutes * 60
    }

    /// `(组名, 组内优先级)` 列表。解析失败的条目被跳过并原样返回给调用方记日志 ——
    /// 一个 priority 没提上去的号等于白买(排在 100 档吃不到流量),不能静默。
    pub fn parsed_member_groups(&self) -> (Vec<(String, i64)>, Vec<String>) {
        let mut ok = Vec::new();
        let mut bad = Vec::new();
        for raw in &self.member_groups {
            match raw.split_once('@') {
                Some((g, p)) => match p.trim().parse::<i64>() {
                    Ok(pri) if !g.trim().is_empty() => ok.push((g.trim().to_string(), pri)),
                    _ => bad.push(raw.clone()),
                },
                None => bad.push(raw.clone()),
            }
        }
        (ok, bad)
    }

    /// 当前 epoch 是否落在高峰窗口内。`start == end` 视为全天。
    pub fn in_peak_window(&self, now_ts: i64) -> bool {
        let (Some(s), Some(e)) = (hhmm_to_minutes(&self.peak_start), hhmm_to_minutes(&self.peak_end))
        else {
            // 解析不出来就当全天开 —— 宁可多买也不要因为一个格式错误导致整夜断供。
            return true;
        };
        if s == e {
            return true;
        }
        let local = (now_ts + self.utc_offset_secs()).rem_euclid(86400) / 60;
        if s < e {
            local >= s && local < e
        } else {
            // 跨零点:落在 [start, 24:00) 或 [00:00, end) 都算窗口内。
            local >= s || local < e
        }
    }

    /// 配置时区下「今天 00:00」对应的 epoch(日预算按本地自然日结算)。
    pub fn local_day_start(&self, now_ts: i64) -> i64 {
        let off = self.utc_offset_secs();
        (now_ts + off).div_euclid(86400) * 86400 - off
    }
}

/// `"HH:MM"` → 从零点起的分钟数。非法返回 `None`。
pub fn hhmm_to_minutes(s: &str) -> Option<i64> {
    let (h, m) = s.trim().split_once(':')?;
    let h: i64 = h.trim().parse().ok()?;
    let m: i64 = m.trim().parse().ok()?;
    if !(0..24).contains(&h) || !(0..60).contains(&m) {
        return None;
    }
    Some(h * 60 + m)
}

/// 校验并归一面板传来的一个参数值。非法即拒,绝不放行。
pub fn coerce(key: &str, raw: &serde_json::Value) -> Result<serde_json::Value, String> {
    let Some(b) = BOUNDS.iter().find(|b| b.key == key) else {
        return Err(format!("未知参数 {key}"));
    };
    match b.kind {
        "bool" => raw
            .as_bool()
            .map(serde_json::Value::from)
            .ok_or_else(|| format!("{} 需要 true/false", b.label)),
        "hhmm" => {
            let s = raw.as_str().ok_or_else(|| format!("{} 需要 HH:MM 字符串", b.label))?;
            let m = hhmm_to_minutes(s).ok_or_else(|| format!("{} 格式非法(要 HH:MM)", b.label))?;
            Ok(serde_json::Value::from(format!("{:02}:{:02}", m / 60, m % 60)))
        }
        "int" | "float" => {
            let n = raw
                .as_f64()
                .ok_or_else(|| format!("{} 需要数字", b.label))?;
            if !n.is_finite() || n < b.min || n > b.max {
                return Err(format!("{} 必须在 {} 与 {} 之间", b.label, b.min, b.max));
            }
            Ok(if b.kind == "int" {
                serde_json::Value::from(n.round() as i64)
            } else {
                serde_json::Value::from(n)
            })
        }
        _ => Err(format!("参数 {key} 类型未知")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 默认值必须是不花钱的组合() {
        let p = Params::default();
        assert!(!p.enabled, "默认必须关着 —— 部署完不该有任何扣款");
        assert!(p.dry_run, "默认必须是演练");
        assert_eq!(p.min_healthy, 1);
        assert_eq!(p.max_per_purchase, 1);
        assert_eq!(p.daily_cap_cny, 200.0);
        assert_eq!(p.idle_skip_ratio, 0.0, "闲时抑制默认关闭");
    }

    #[test]
    fn 缺字段的旧json也能读出默认() {
        let p: Params = serde_json::from_str(r#"{"min_healthy":3}"#).unwrap();
        assert_eq!(p.min_healthy, 3);
        assert_eq!(p.daily_cap_cny, 200.0, "没写的字段取默认");
        assert!(!p.enabled);
    }

    #[test]
    fn 跨零点的高峰窗口() {
        let mut p = Params::default(); // 09:00-02:00, 东八区
        // 找一个本地 00:00 的 epoch
        let t0: i64 = 1_785_600_000;
        let base = t0 - (t0 + 8 * 3600).rem_euclid(86400);
        let at = |h: i64| base + h * 3600;
        for h in [9, 12, 20, 23, 0, 1] {
            assert!(p.in_peak_window(at(h)), "{h}:00 应在 09:00–02:00 内");
        }
        for h in [2, 5, 8] {
            assert!(!p.in_peak_window(at(h)), "{h}:00 应在窗口外");
        }
        // 普通(不跨零点)窗口
        p.peak_start = "09:00".into();
        p.peak_end = "18:00".into();
        assert!(p.in_peak_window(at(10)));
        assert!(!p.in_peak_window(at(19)));
        // 起止相同 = 全天
        p.peak_end = "09:00".into();
        assert!(p.in_peak_window(at(3)));
    }

    #[test]
    fn 窗口格式错时放行而不是整夜断供() {
        let mut p = Params::default();
        p.peak_start = "不是时间".into();
        assert!(p.in_peak_window(0), "解析失败应当全天放行,而不是全天拒绝");
    }

    #[test]
    fn 成员边解析出组名与优先级并报告坏条目() {
        let mut p = Params::default();
        p.member_groups = vec!["G0@0".into(), "GECO@100".into(), "坏的".into(), "@5".into()];
        let (ok, bad) = p.parsed_member_groups();
        assert_eq!(ok, vec![("G0".to_string(), 0), ("GECO".to_string(), 100)]);
        assert_eq!(bad.len(), 2, "坏条目必须报出来,不能静默丢弃");
    }

    #[test]
    fn 参数校验拒绝越界与错类型() {
        assert!(coerce("min_healthy", &serde_json::json!(5)).is_ok());
        assert!(coerce("min_healthy", &serde_json::json!(999)).is_err(), "超上限必须拒");
        assert!(coerce("min_healthy", &serde_json::json!(-1)).is_err());
        assert!(coerce("enabled", &serde_json::json!("yes")).is_err(), "bool 不收字符串");
        assert!(coerce("不存在的键", &serde_json::json!(1)).is_err());
        // hhmm 归一成两位数
        assert_eq!(coerce("peak_start", &serde_json::json!("9:5")).unwrap(), "09:05");
        assert!(coerce("peak_start", &serde_json::json!("25:00")).is_err());
        // NaN/Inf 必须挡住,否则会污染整条决策链
        assert!(coerce("daily_cap_cny", &serde_json::json!(f64::NAN)).is_err());
    }

    #[test]
    fn 日预算按本地自然日结算() {
        let p = Params::default(); // 东八区
        let t = 1_785_600_000;
        let start = p.local_day_start(t);
        assert!(start <= t);
        assert_eq!((start + p.utc_offset_secs()).rem_euclid(86400), 0, "必须落在本地 00:00");
        assert!(t - start < 86400);
    }
}
