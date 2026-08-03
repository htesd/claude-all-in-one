//! 按周画像预测积分消耗。
//!
//! **为什么按「星期几 × 钟点」而不是只按钟点**:用户的判断是「一周的波动是有规律的」——
//! 工作日白天和周末白天不是一回事,只按钟点会把两者平均成一条没人对得上的曲线。
//!
//! **为什么用中位数而不是均值**:一次上游抖动或一个跑批客户就能让某个钟点出现 5 倍尖峰,
//! 均值会被它永久拉高,从此每周同一时刻都虚报需求、催出不必要的购买。
//!
//! **预测的是「总需求」而不是「ksk_ 号的消耗」。** 这是个踩过的坑:ksk_ 号是最近才引入的,
//! 47 天历史里绝大多数格子的 ksk_ 值是 0,拿它建画像预测出来全是 0。更糟的是会形成
//! 循环论证 —— 号全死了 → ksk_ 消耗为 0 → 预测说没需求 → 闲时抑制拦住补货,
//! 正好在最该补货的时刻。需求由客户流量决定,与哪种号在承接无关,所以必须用总量。
//! (图上仍然分层显示 ksk_ / 其它,那是「谁在承接」,是另一件事。)
//!
//! **为什么不引 chrono-tz**:这里只需要「epoch → 本地星期几/钟点」。Asia/Shanghai 自 1991 年
//! 起无夏令时,一个固定 UTC 偏移就是精确的,而且可测、零依赖。真要支持有 DST 的时区再说 ——
//! 那时应该引库,而不是自己写规则。

/// 一个格子至少要有这么多样本才敢用。2 = 至少见过两次同一个「周几+钟点」;
/// 单次观测可能整个是异常(比如一次全局扫号),不足以成规律。
pub const MIN_SAMPLES: usize = 2;

pub const BASIS_WEEK: &str = "周画像";
pub const BASIS_HOUR: &str = "日画像";
pub const BASIS_RECENT: &str = "近期均值";
pub const BASIS_NONE: &str = "数据不足";

/// 预测的一个小时。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Point {
    /// 该小时的 UTC 整点 epoch。
    pub ts: i64,
    /// 本地星期几,0 = 周一。
    pub weekday: i64,
    /// 本地钟点 0–23。
    pub hour: i64,
    /// 预测的**总**积分消耗(不是 ksk_ 那一份,见模块开头)。
    pub credits: f64,
    /// 这个数是拿什么算出来的 —— 面板要能显示依据,否则冷启动期的预测会被当成
    /// 和攒满历史之后一样可信。
    pub basis: &'static str,
    pub samples: usize,
}

/// 画像成熟度。面板据此告诉用户「现在的预测能信几分」。
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct Coverage {
    pub hours_collected: usize,
    pub days_collected: f64,
    /// 满格 168(7 天 × 24 小时)。
    pub week_cells_ready: usize,
    /// 满格 24。
    pub hour_cells_ready: usize,
    pub mature: bool,
}

/// epoch → (本地天序号, 本地钟点)。`offset_secs` 是 UTC 偏移(东八区 = 28800)。
///
/// 用 `div_euclid`/`rem_euclid` 而不是 `/`、`%`:后者对负数向零取整,
/// 1970 年之前的时间戳会算出负钟点。这里的数据不会早于 1970,但这类错误一旦发生
/// 极难察觉,不值得省这两个字符。
fn local_parts(ts: i64, offset_secs: i64) -> (i64, i64) {
    let local = ts + offset_secs;
    (local.div_euclid(86400), local.rem_euclid(86400) / 3600)
}

/// 本地天序号 → 星期几(0 = 周一)。1970-01-01 是**周四**,故 +3。
fn weekday_of(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

/// epoch → 本地星期几(0 = 周一)。
pub fn local_weekday(ts: i64, offset_secs: i64) -> i64 {
    weekday_of(local_parts(ts, offset_secs).0)
}

/// epoch → 本地钟点。
pub fn local_hour(ts: i64, offset_secs: i64) -> i64 {
    local_parts(ts, offset_secs).1
}

fn median(xs: &mut Vec<f64>) -> f64 {
    if xs.is_empty() {
        return 0.0;
    }
    xs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = xs.len();
    if n % 2 == 1 {
        xs[n / 2]
    } else {
        (xs[n / 2 - 1] + xs[n / 2]) / 2.0
    }
}

/// 历史小时序列 → (周画像 168 格, 日画像 24 格, 近 24 小时均值)。
///
/// `hours` 形如 `[(hour_ts, ksk_credits, total_credits)]`,升序。
/// **末尾那个未走完的小时必须由调用方剔除** —— 它天然偏低,混进画像会把每个钟点都往下
/// 拉一点,而且拉多少取决于你什么时候看,同一份数据每次算出来还不一样。
fn build_profile(
    hours: &[(i64, f64, f64)],
    offset_secs: i64,
) -> (Vec<Vec<f64>>, Vec<Vec<f64>>, Option<f64>) {
    let mut week: Vec<Vec<f64>> = vec![Vec::new(); 168];
    let mut hour: Vec<Vec<f64>> = vec![Vec::new(); 24];
    // 注意取的是**总量**(第 3 项)而不是 ksk_ 那一份(第 2 项),理由见模块开头。
    for &(ts, _, total) in hours {
        let (days, hr) = local_parts(ts, offset_secs);
        let wd = weekday_of(days);
        week[(wd * 24 + hr) as usize].push(total);
        hour[hr as usize].push(total);
    }
    let tail: Vec<f64> = hours.iter().rev().take(24).map(|&(_, _, t)| t).collect();
    let recent = if tail.is_empty() {
        None
    } else {
        Some(tail.iter().sum::<f64>() / tail.len() as f64)
    };
    (week, hour, recent)
}

/// 预测 `start_ts` 起 `ahead` 个整点的 ksk_ 积分消耗。
///
/// 逐级退化:周画像 → 日画像 → 近期均值 → 无预测,并把依据一起报出去。
pub fn forecast(
    hours: &[(i64, f64, f64)],
    offset_secs: i64,
    start_ts: i64,
    ahead: i64,
) -> Vec<Point> {
    let (week, hour_prof, recent) = build_profile(hours, offset_secs);
    let mut out = Vec::with_capacity(ahead.max(0) as usize);
    for i in 0..ahead.max(0) {
        let ts = start_ts + i * 3600;
        let (days, hr) = local_parts(ts, offset_secs);
        let wd = weekday_of(days);
        let wcell = &week[(wd * 24 + hr) as usize];
        let hcell = &hour_prof[hr as usize];
        let (val, basis, samples) = if wcell.len() >= MIN_SAMPLES {
            (median(&mut wcell.clone()), BASIS_WEEK, wcell.len())
        } else if hcell.len() >= MIN_SAMPLES {
            (median(&mut hcell.clone()), BASIS_HOUR, hcell.len())
        } else if let Some(r) = recent {
            (r, BASIS_RECENT, hours.len().min(24))
        } else {
            (0.0, BASIS_NONE, 0)
        };
        out.push(Point {
            ts,
            weekday: wd,
            hour: hr,
            credits: (val * 10.0).round() / 10.0,
            basis,
            samples,
        });
    }
    out
}

/// 画像成熟度。
pub fn coverage(hours: &[(i64, f64, f64)], offset_secs: i64) -> Coverage {
    let (week, hour_prof, _) = build_profile(hours, offset_secs);
    let week_cells_ready = week.iter().filter(|v| v.len() >= MIN_SAMPLES).count();
    let hour_cells_ready = hour_prof.iter().filter(|v| v.len() >= MIN_SAMPLES).count();
    Coverage {
        hours_collected: hours.len(),
        days_collected: ((hours.len() as f64 / 24.0) * 10.0).round() / 10.0,
        week_cells_ready,
        hour_cells_ready,
        // 每个「周几+钟点」要 2 个样本 → 攒满约 2 周才算成熟。
        // 留 28 格余量:总有些钟点因为服务重启/流量为零而缺样本。
        mature: week_cells_ready >= 140,
    }
}

/// 预测窗口内的总需求(积分)。
pub fn total_demand(points: &[Point]) -> f64 {
    (points.iter().map(|p| p.credits).sum::<f64>() * 10.0).round() / 10.0
}

#[cfg(test)]
mod tests {
    use super::*;

    const CST: i64 = 8 * 3600;

    /// 2026-08-03 00:00 CST 的 epoch(周一)。
    const MON_0000: i64 = 1_785_686_400 - 8 * 3600 + 8 * 3600;

    fn at(day_offset: i64, hour: i64) -> i64 {
        // 以「2026-08-03 00:00 CST」为基准平移。
        mon_base() + day_offset * 86400 + hour * 3600
    }

    /// 找一个确定是周一 00:00 CST 的 epoch,不依赖外部常量算错。
    fn mon_base() -> i64 {
        let mut t = MON_0000 - MON_0000.rem_euclid(3600);
        // 往回退到本地 00:00
        t -= (t + CST).rem_euclid(86400);
        // 往回退到周一
        while local_weekday(t, CST) != 0 {
            t -= 86400;
        }
        t
    }

    /// `(ts, 值)` → `(ts, ksk 份额, 总量)`。**预测读的是总量**,所以把给定值放在第 3 位;
    /// 第 2 位刻意给一个不同的数,用来证明预测确实没在读 ksk_ 那一份。
    fn hist(points: &[(i64, f64)]) -> Vec<(i64, f64, f64)> {
        points.iter().map(|&(t, v)| (t, v * 0.3, v)).collect()
    }

    #[test]
    fn 本地钟点与星期几按偏移换算() {
        let base = mon_base();
        assert_eq!(local_weekday(base, CST), 0, "基准必须是周一");
        assert_eq!(local_hour(base, CST), 0);
        assert_eq!(local_hour(base + 10 * 3600, CST), 10);
        // 跨零点:周一 23:00 的下一小时是周二 00:00,不是周一 24:00。
        assert_eq!((local_weekday(at(0, 23), CST), local_hour(at(0, 23), CST)), (0, 23));
        assert_eq!((local_weekday(at(1, 0), CST), local_hour(at(1, 0), CST)), (1, 0));
        // 同一时刻按 UTC 看是前一天 16 点 —— 若按 UTC 归并,整张画像会整体错位 8 小时。
        assert_eq!(local_hour(base, 0), 16);
    }

    #[test]
    fn 同一周几同一钟点满两个样本就用周画像() {
        let h = hist(&[(at(-14, 10), 100.0), (at(-7, 10), 120.0), (at(0, 10), 110.0)]);
        let p = &forecast(&h, CST, at(7, 10), 1)[0];
        assert_eq!(p.basis, BASIS_WEEK);
        assert_eq!(p.credits, 110.0, "三个样本的中位数");
        assert_eq!(p.weekday, 0);
        assert_eq!(p.hour, 10);
    }

    #[test]
    fn 周画像样本不足时退到日画像() {
        // 周一 10 点只有 1 个样本,但 10 点整体有 3 个。
        let h = hist(&[(at(0, 10), 100.0), (at(1, 10), 200.0), (at(2, 10), 300.0)]);
        let p = &forecast(&h, CST, at(7, 10), 1)[0];
        assert_eq!(p.basis, BASIS_HOUR);
        assert_eq!(p.credits, 200.0);
        assert_eq!(p.samples, 3);
    }

    #[test]
    fn 日画像也不足时退到近期均值() {
        let h = hist(&[(at(0, 9), 100.0)]);
        let p = &forecast(&h, CST, at(7, 10), 1)[0];
        assert_eq!(p.basis, BASIS_RECENT);
        assert_eq!(p.credits, 100.0);
    }

    #[test]
    fn 完全没数据时不瞎猜() {
        let p = &forecast(&[], CST, at(7, 10), 1)[0];
        assert_eq!(p.basis, BASIS_NONE);
        assert_eq!(p.credits, 0.0);
    }

    #[test]
    fn 单个尖峰不会永久抬高该钟点的预测() {
        // 一次跑批客户造成 100 倍尖峰。用均值这个钟点会被永久拉到 2578,
        // 此后每周同一时刻都虚报需求、催出不必要的购买。
        let h = hist(&[
            (at(-21, 14), 100.0),
            (at(-14, 14), 110.0),
            (at(-7, 14), 10000.0),
            (at(0, 14), 105.0),
        ]);
        let p = &forecast(&h, CST, at(7, 14), 1)[0];
        assert_eq!(p.basis, BASIS_WEEK);
        assert!((p.credits - 107.5).abs() < 1e-6, "应为 (105+110)/2,实际 {}", p.credits);
        assert!(p.credits < 200.0, "尖峰必须被中位数按住");
    }

    #[test]
    fn 冷启动时明确报告未成熟() {
        let h = hist(&(0..6).map(|i| (at(0, i), 100.0)).collect::<Vec<_>>());
        let c = coverage(&h, CST);
        assert_eq!(c.hours_collected, 6);
        assert!(!c.mature);
        assert_eq!(c.week_cells_ready, 0, "每格只有 1 个样本");
    }

    #[test]
    fn 攒满两周后周画像成熟() {
        let mut pts = Vec::new();
        for d in -13..=0 {
            for h in 0..24 {
                pts.push((at(d, h), 100.0));
            }
        }
        let c = coverage(&hist(&pts), CST);
        assert_eq!(c.week_cells_ready, 168);
        assert!(c.mature);
        assert!((c.days_collected - 14.0).abs() < 1e-9);
    }

    #[test]
    fn 窗口总需求是逐小时求和() {
        let h = hist(&[(at(0, 10), 50.0), (at(0, 11), 50.0)]);
        let pts = forecast(&h, CST, at(7, 10), 3);
        let sum: f64 = pts.iter().map(|p| p.credits).sum();
        assert!((total_demand(&pts) - sum).abs() < 1e-6);
        assert_eq!(pts.len(), 3);
    }
}
