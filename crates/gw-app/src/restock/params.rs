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
    Bound { key: "max_unit_cost_cny_per_credit", kind: "float", min: 0.0, max: 10.0,
        label: "可接受单位成本 ¥/积分",
        hint: "预期「单价÷(需求×寿命)」高于此值就不买。0 = 关闭这道闸" },
    Bound { key: "account_throughput_credits_per_hour", kind: "float", min: 1.0, max: 100000.0,
        label: "单号吞吐上限 分/时", hint: "算预期产出的封顶。实测约 1900" },
    Bound { key: "expected_lifetime_secs", kind: "int", min: 60.0, max: 86400.0,
        label: "预期寿命 秒", hint: "号从上号到死的时长。实测中位数约 2880,面板会显示实测值" },
    Bound { key: "demand_window_secs", kind: "int", min: 60.0, max: 86400.0,
        label: "需求测量窗口 秒", hint: "用最近这么久的实测速率代表当前需求" },
    Bound { key: "liveness_window_secs", kind: "int", min: 60.0, max: 86400.0,
        label: "判活窗口 秒", hint: "回看这么久:有成功=活,有尝试零成功=僵尸,没尝试=不下结论" },
    Bound { key: "new_account_grace_secs", kind: "int", min: 0.0, max: 86400.0,
        label: "新号宽限 秒", hint: "刚上号还没跑过请求,这段时间内一律算健康" },
    Bound { key: "lead_time_secs", kind: "int", min: 0.0, max: 3600.0,
        label: "提前量 秒", hint: "活号预计还剩这么久就提前下单。0 = 关(提前买会折掉新号同样长的寿命)" },
    Bound { key: "hunt_interval_secs", kind: "int", min: 2.0, max: 300.0,
        label: "抢货探测间隔 秒",
        hint: "缺货且该买时改用这个间隔重探。号稀缺时慢一秒就被别人买走" },
    Bound { key: "hunt_max_secs", kind: "int", min: 0.0, max: 86400.0,
        label: "抢货最长时长 秒", hint: "连续抢这么久还没货就退回常规轮询。0 = 一直抢" },
    Bound { key: "notify_url", kind: "url", min: 0.0, max: 500.0,
        label: "通知 Webhook",
        hint: "缺货久了/抢到号/熔断时回调它。按域名自动适配企业微信、钉钉、飞书、Slack,其余发通用 JSON。留空 = 不通知" },
    Bound { key: "notify_after_secs", kind: "int", min: 0.0, max: 86400.0,
        label: "缺货多久后通知 秒", hint: "连续抢货超过这么久仍没抢到就回调。0 = 不发缺货通知" },
    Bound { key: "notify_min_gap_secs", kind: "int", min: 60.0, max: 86400.0,
        label: "同类通知最小间隔 秒", hint: "同一种事件两次回调之间至少隔这么久,免得刷屏" },
];

fn d_false() -> bool { false }
fn d_true() -> bool { true }
fn d_1() -> i64 { 1 }
fn d_200() -> f64 { 200.0 }
fn d_rate() -> f64 { 7.2 }
/// 保留额默认 **0**:它是在「本单价格」**之上**再留的余量,写 40 就意味着
/// ¥58 的余额买不了 ¥21 的号 —— 实测 2026-08-04 卡住 25 轮。想留缓冲请显式填。
fn d_reserve() -> f64 { 0.0 }
fn d_2() -> i64 { 2 }
fn d_price() -> f64 { 6.0 }
/// 轮询 30s 而不是 60s:号一死到补上的空档 = 检测延迟 + 上号耗时(~25s)。
/// 号只活 45 分钟,每个周期少丢 30 秒就是少丢 1% 的覆盖,而多跑一轮几乎不花钱
/// (health 是 loopback,drop 库存另有 300s 节流)。
fn d_30() -> i64 { 30 }
fn d_24() -> i64 { 24 }
fn d_600() -> i64 { 600 }
fn d_300() -> i64 { 300 }
fn d_1800() -> i64 { 1800 }
fn d_lifetime() -> i64 { 2700 }
fn d_unit_cost() -> f64 { 0.04 }
fn d_throughput() -> f64 { 1900.0 }
fn d_zero_i() -> i64 { 0 }
/// 抢货间隔 **5 秒**:对方接口往返 3.4–4.2s,5s 已接近「上一轮刚回来就发下一轮」。
/// 再快只是在等待同一个响应,不会更早看见货,却会翻倍地增加被限流的风险。
fn d_hunt() -> i64 { 5 }
/// 缺货 10 分钟才通知。断货几分钟是常态(近 7 天 854 轮),分钟级就叫人等于让人学会忽略。
fn d_notify_after() -> i64 { 600 }
fn d_notify_gap() -> i64 { 1800 }
fn d_empty() -> String { String::new() }
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

    #[serde(default = "d_30")]
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
    ///
    /// 与 [`Self::max_unit_cost_cny_per_credit`] 的分工:这条是**相对**判据
    /// (比历史峰值),那条是**绝对**判据(比钱)。默认只开后者 —— 相对判据说不出
    /// 「这单到底亏不亏」,而这正是要回答的问题。
    #[serde(default = "d_zero_f")]
    pub idle_skip_ratio: f64,

    /// **可接受的单位成本上限(¥/积分)**。这是买号策略的主旋钮。
    ///
    /// 实测的决定性事实:号按**墙上时钟**死(22 个号烧速差 3 倍、存活一律 0.7–0.9h,
    /// 烧得最猛的反而活得最久),所以一个号的产出 = 需求速率 × 寿命,**跟号本身无关**。
    /// 于是「该不该买」就是一道算术题:
    ///
    /// ```text
    /// 预期产出   = min(需求速率, 单号吞吐上限) × 预期寿命
    /// 预期单位成本 = 本单价格 / 预期产出
    /// ```
    ///
    /// 这比写死钟点表耐用:drop 降价时门槛自动放宽,客户作息变了也不用改配置。
    /// 默认 0.04 对照两个数:当前实测 ¥0.0191/积分,而贵号池基准约 ¥0.068/积分
    /// (0.06 元/次 ÷ 0.88 积分/次)。按 ¥20 单价反解,门槛落在约 670 积分/时 ——
    /// 凌晨 6–8 点(77–391 分/时)因此不买,那几个钟点买号是**亏的**。
    /// **0 = 关闭这道闸。**
    #[serde(default = "d_unit_cost")]
    pub max_unit_cost_cny_per_credit: f64,
    /// 单号吞吐上限(积分/时),给预期产出封顶。实测最高持续速率约 1900。
    /// 需求再高也吃不下,不封顶会在高峰期把预期产出算得过于乐观。
    #[serde(default = "d_throughput")]
    pub account_throughput_credits_per_hour: f64,
    /// 预期寿命(秒)。实测中位数约 2880,取 2700 略保守。
    /// 面板会同时显示**实测滚动中位数**供人工校准,但**不自动套用** ——
    /// 这个值估短的后果是每轮都提前下单,花费直接翻倍。
    #[serde(default = "d_lifetime")]
    pub expected_lifetime_secs: i64,
    /// 用最近多少秒的实测消耗代表「当前需求」。
    #[serde(default = "d_1800")]
    pub demand_window_secs: i64,

    /// 判活回看窗口(秒)。见 `Engine::health` 的三态判定。
    #[serde(default = "d_600")]
    pub liveness_window_secs: i64,
    /// 新号宽限期(秒):刚上号还没跑过任何请求,这段时间内一律算健康,
    /// 否则新号会因为「窗口内零成功」被自己判成僵尸,于是不停买。
    #[serde(default = "d_300")]
    pub new_account_grace_secs: i64,
    /// 提前量(秒):活号预计还剩这么久就先下单。**默认 0 = 关**。
    ///
    /// 算过账才关的:检测(≤1 轮 30s)+ 上号(~25s)的空档约占 45 分钟周期的 2%,
    /// 而提前 N 秒下单会让新号白折掉自己 N 秒的寿命(同样是墙上时钟)——
    /// 提前 180s 是拿 6.7% 的产出换 2% 的连续性,净亏。将来若判断连续性更值钱再开。
    #[serde(default = "d_zero_i")]
    pub lead_time_secs: i64,

    // ───────────────────── 抢货 ─────────────────────
    //
    // 2026-08-06 起速刷号变稀缺:货一上架就被别人买走,常规 30s 轮询几乎必然错过。
    // 「缺货」与「不该买」是两种完全不同的状态,前者要的是**盯着**,后者要的是**别动**。
    // 所以只在「闸门全过、就差有货」时才提速 —— 那时每多等一秒都是纯损失,
    // 而其它任何跳过理由(不在窗口、需求撑不起、日上限)提速都只是白打对方接口。
    /// 缺货且该买时的重探间隔(秒)。
    #[serde(default = "d_hunt")]
    pub hunt_interval_secs: i64,
    /// 连续抢货的时长上限(秒),超了就退回常规轮询。**0 = 不限**。
    ///
    /// 默认不限是有意的:实测断供能连着 4.7 小时,而那 4.7 小时里池子是空的 ——
    /// 「抢累了就歇会儿」在这里等于「主动放弃最要紧的那几个小时」。
    /// 留这个旋钮是为了对方限流时能人工收敛,不是为了日常省请求。
    #[serde(default = "d_zero_i")]
    pub hunt_max_secs: i64,

    // ───────────────────── 通知 ─────────────────────
    /// 事件回调地址。**留空 = 完全不发**(默认)。
    ///
    /// 有它是因为抢货是**可能失败**的:对方长时间没货时,系统已经尽力了,
    /// 剩下的只能是人去别处找货 —— 而人得先知道。
    #[serde(default = "d_empty")]
    pub notify_url: String,
    /// 连续抢货超过这么久仍没抢到就回调一次。**0 = 不发缺货通知**。
    #[serde(default = "d_notify_after")]
    pub notify_after_secs: i64,
    /// 同一类事件两次回调的最小间隔(秒)。断供期间每 5 秒来一条会让人直接静音。
    #[serde(default = "d_notify_gap")]
    pub notify_min_gap_secs: i64,
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

    /// 一个号在当前需求下的预期产出(积分)。
    ///
    /// 号按墙上时钟死,所以产出只由「这段时间里有多少活儿」决定,
    /// 再被单号吞吐上限封顶 —— 需求 3000 分/时也不代表一个号能吃下 3000。
    pub fn expected_yield(&self, demand_rate: f64) -> f64 {
        let rate = demand_rate.max(0.0).min(self.account_throughput_credits_per_hour.max(0.0));
        rate * (self.expected_lifetime_secs.max(0) as f64 / 3600.0)
    }

    /// 本单的预期单位成本(¥/积分)。预期产出为 0 时返回 `INFINITY`
    /// —— 那正是「买了也没人用」,必须被闸门挡住,不能当成「算不出来所以放行」。
    pub fn expected_unit_cost(&self, price_cny: f64, demand_rate: f64) -> f64 {
        let y = self.expected_yield(demand_rate);
        if y <= 0.0 {
            f64::INFINITY
        } else {
            price_cny.max(0.0) / y
        }
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
        // 空串是合法值,意思是「不配」。`max` 当成字符长度上限用。
        //
        // 只认 http/https:填错协议(或手滑粘了一段 `curl -X POST ...`)的后果是每次
        // 事件都在日志里失败一次,而通知的全部意义就是「出事时人能知道」——
        // 一个从来发不出去的通知比没有通知更坏。
        "url" => {
            let s = raw.as_str().ok_or_else(|| format!("{} 需要字符串", b.label))?.trim();
            if s.is_empty() {
                return Ok(serde_json::Value::from(""));
            }
            if !(s.starts_with("http://") || s.starts_with("https://")) {
                return Err(format!("{} 必须以 http:// 或 https:// 开头", b.label));
            }
            if s.chars().count() as f64 > b.max {
                return Err(format!("{} 最长 {} 个字符", b.label, b.max));
            }
            Ok(serde_json::Value::from(s))
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
        assert_eq!(p.lead_time_secs, 0, "提前量默认关闭:提前买会等长折掉新号寿命");
        assert_eq!(
            p.min_balance_reserve_cny, 0.0,
            "保留额是在本单价格之上再留的,默认 40 会让 ¥58 买不了 ¥21 的号"
        );
    }

    #[test]
    fn 抢货与通知的默认值不改变任何既有行为() {
        let p = Params::default();
        assert_eq!(p.hunt_interval_secs, 5, "缺货时 5s 重探;对方往返 3–4s,再快只是在等同一个响应");
        assert_eq!(p.hunt_max_secs, 0, "0 = 一直抢。实测断供能连着 4.7 小时,那时最不该歇");
        assert!(p.notify_url.is_empty(), "没填地址就一条都不发 —— 默认不许对外发请求");
        assert_eq!(p.notify_min_gap_secs, 1800);
    }

    #[test]
    fn 通知地址只收http开头的且空串合法() {
        let ok = |s: &str| coerce("notify_url", &serde_json::json!(s));
        assert_eq!(ok("").unwrap(), serde_json::json!(""), "空 = 不配,是合法值");
        assert_eq!(ok("  ").unwrap(), serde_json::json!(""), "只有空白也当成不配");
        assert!(ok("https://qyapi.weixin.qq.com/x?key=1").is_ok());
        assert!(ok("http://127.0.0.1:9000/hook").is_ok(), "自建中转常在 loopback");
        // 填错协议的后果是每次事件都在日志里失败一次,而通知的意义就是「出事时人能知道」。
        assert!(ok("ftp://x/y").is_err());
        assert!(ok("qyapi.weixin.qq.com/x").is_err(), "少了协议头的粘贴很常见,必须当场拒绝");
        assert!(coerce("notify_url", &serde_json::json!(123)).is_err());
        assert!(ok(&format!("https://x/{}", "a".repeat(600))).is_err(), "超长要拒绝");
    }

    #[test]
    fn 抢货间隔的下限挡住把自己打成限流的配置() {
        assert!(coerce("hunt_interval_secs", &serde_json::json!(1)).is_err());
        assert!(coerce("hunt_interval_secs", &serde_json::json!(2)).is_ok());
        assert!(coerce("hunt_interval_secs", &serde_json::json!(301)).is_err());
        // 0 是「不限时长」的合法值,不能被下限挡掉。
        assert!(coerce("hunt_max_secs", &serde_json::json!(0)).is_ok());
    }

    #[test]
    fn 预期单位成本按需求算且被吞吐上限封顶() {
        let mut p = Params::default();
        p.expected_lifetime_secs = 3600; // 整一小时,便于心算
        p.account_throughput_credits_per_hour = 1900.0;

        // 需求 1000 分/时 × 1h = 1000 分产出,¥20 → ¥0.02/分
        assert!((p.expected_unit_cost(20.0, 1000.0) - 0.02).abs() < 1e-9);
        // 需求再高也吃不下:3000 被封到 1900
        assert!((p.expected_unit_cost(20.0, 3000.0) - 20.0 / 1900.0).abs() < 1e-9);
        // 凌晨低谷:100 分/时 → ¥0.2/分,远高于默认上限 0.04,必须被挡
        assert!(p.expected_unit_cost(20.0, 100.0) > p.max_unit_cost_cny_per_credit);
        // 零需求不是「算不出来」,是「买了也没人用」——必须是 INFINITY 而不是 0
        assert_eq!(p.expected_unit_cost(20.0, 0.0), f64::INFINITY);
    }

    #[test]
    fn 新参数的边界表与结构体字段一一对应() {
        // BOUNDS 漏一条,面板就改不了这个参数,而代码里它已经在起作用了 —— 静默分叉。
        let v = serde_json::to_value(Params::default()).unwrap();
        let obj = v.as_object().unwrap();
        for b in BOUNDS {
            assert!(obj.contains_key(b.key), "BOUNDS 里的 {} 在 Params 上不存在", b.key);
        }
        for k in ["max_unit_cost_cny_per_credit", "expected_lifetime_secs",
                  "liveness_window_secs", "new_account_grace_secs",
                  "account_throughput_credits_per_hour", "demand_window_secs",
                  "lead_time_secs"] {
            assert!(BOUNDS.iter().any(|b| b.key == k), "{k} 没进 BOUNDS,面板改不了");
        }
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
