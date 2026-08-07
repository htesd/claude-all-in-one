//! 抢货状态机:缺货时把轮询提速到秒级,并决定什么时候该叫人。
//!
//! ## 为什么单独抽出来
//!
//! 这套逻辑的每一个分支都要**几十分钟到几小时**才在生产上表现出来 ——
//! 「抢了一整晚但从没通知」和「每 5 秒发一条通知」都是真实可能的写法,
//! 而两者都要等到下一次断供才看得见。放在异步循环里就只能靠在生产上观察。
//!
//! 所以这里是**纯逻辑**:不碰网络、不碰 DB、不看时钟(`now` 由调用方传)。
//! 它只回答两件事:下一轮多久之后、这一轮要不要叫人。
//!
//! ## 触发条件是「就差有货」,不是「没买成」
//!
//! 只有 [`super::engine::Decision::out_of_stock`](一件都没有)才提速。
//! 「有货但过不了闸门」不提速:闸门不会因为多问几次就放行,加密度只是白打对方接口。
//! 「询价全失败」也不提速:那时最可能的原因恰恰是对方在限流我们。

/// 抢货状态。`since == 0` 表示不在抢货。
#[derive(Debug, Default, Clone)]
pub struct Hunt {
    since: i64,
    probes: i64,
    /// 本轮抢货是否已经就「抢不到」通知过人。**每轮抢货只叫一次** ——
    /// 断供 4.7 小时期间每 30 分钟响一次的下场是被静音,而静音之后
    /// 真正要紧的熔断通知也一起收不到了。
    notified: bool,
}

/// 一次 [`Hunt::step`] 产生的动作。用标志位而不是枚举:同一轮里
/// 「该通知了」与「抢货到时长上限了」可以同时成立(两个阈值配成一样时),
/// 枚举会逼着丢掉其中一个。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HuntStep {
    /// 刚进入抢货模式。
    pub entered: bool,
    /// 该发「抢不到号」通知了。
    pub notify: bool,
    /// 达到时长上限,已退回常规轮询。
    pub exhausted: bool,
    /// 刚退出抢货模式(有货了 / 闸门变了 / 开关关了)。
    pub exited: bool,
    /// 本轮抢货已持续多久(秒)。`entered`/`notify`/`exhausted`/`exited` 时有意义。
    pub waited: i64,
    /// 本轮抢货共探测了多少次。
    pub probes: i64,
    /// 退出时:此前是否已就缺货通知过人。**没叫过人就不用告诉人「好了」**。
    pub was_notified: bool,
}

impl Hunt {
    pub fn active(&self) -> bool {
        self.since != 0
    }

    /// 本轮抢货是否已经叫过人。只给面板显示用 —— 判「该不该叫」在 [`Self::step`] 里。
    pub fn notified(&self) -> bool {
        self.notified
    }

    /// 丢弃状态(失去租约时用)。
    ///
    /// 不是 leader 就不该维持抢货状态:否则重新当选时会带着一份早已过期的
    /// 「已等待多久」,通知里的时长直接失真 —— 而那个时长正是人判断
    /// 「要不要自己去别处找货」的依据。
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// 推进一轮。
    ///
    /// - `out_of_stock`:本轮决策是不是「闸门全过、就差有货」。
    /// - `notify_after`:连续缺货多少秒后叫人。**0 = 不叫**。
    /// - `hunt_max`:连续抢货的时长上限(秒)。**0 = 不限**。
    ///
    /// 决策超时(没有结论)时调用方**不要调它** —— 一次超时不代表有货了,
    /// 按「非缺货」处理会让状态机进进出出,把流水搅成一堆开关记录。
    pub fn step(&mut self, now: i64, out_of_stock: bool, notify_after: i64, hunt_max: i64) -> HuntStep {
        if !out_of_stock {
            if !self.active() {
                return HuntStep::default();
            }
            let out = HuntStep {
                exited: true,
                waited: now - self.since,
                probes: self.probes,
                was_notified: self.notified,
                ..Default::default()
            };
            self.reset();
            return out;
        }

        let mut out = HuntStep::default();
        if !self.active() {
            self.since = now;
            self.probes = 0;
            self.notified = false;
            out.entered = true;
        }
        self.probes += 1;
        let waited = now - self.since;
        out.waited = waited;
        out.probes = self.probes;

        if !self.notified && notify_after > 0 && waited >= notify_after {
            self.notified = true;
            out.notify = true;
        }
        // 上限判定放在通知**之后**:两个阈值配成一样时,人应当同时收到
        // 「抢不到」和「已退回常规轮询」这两条信息,而不是只收到后者。
        if hunt_max > 0 && waited >= hunt_max {
            out.exhausted = true;
            // 退回常规轮询,但**不算 `exited`** —— 那是给「有货了/闸门变了」用的,
            // 两者在流水里是完全不同的两句话。
            self.reset();
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缺货第一轮进入,之后只是累加,不重复「进入」。
    #[test]
    fn 首轮进入之后只累加不重复进入() {
        let mut h = Hunt::default();
        let s = h.step(1000, true, 600, 0);
        assert!(s.entered);
        assert_eq!(s.probes, 1);
        assert_eq!(s.waited, 0);

        let s = h.step(1005, true, 600, 0);
        assert!(!s.entered, "同一轮抢货不该反复记「进入」");
        assert_eq!(s.probes, 2);
        assert_eq!(s.waited, 5);
    }

    /// 不缺货时什么都不做 —— 绝不能空转出一条「退出」。
    #[test]
    fn 没进抢货时不缺货一律无事发生() {
        let mut h = Hunt::default();
        assert_eq!(h.step(1000, false, 600, 0), HuntStep::default());
        assert!(!h.active());
    }

    #[test]
    fn 有货了就退出并带上等了多久探了多少次() {
        let mut h = Hunt::default();
        h.step(1000, true, 600, 0);
        h.step(1005, true, 600, 0);
        let s = h.step(1010, false, 600, 0);
        assert!(s.exited);
        assert_eq!(s.waited, 10);
        assert_eq!(s.probes, 2, "退出时报的是进入以来的探测次数");
        assert!(!h.active());
    }

    /// **每轮抢货只叫一次人。** 这条守的是「被静音」这个最终失败模式:
    /// 断供 4.7 小时期间每 5 秒响一次,人会关掉通知,然后连熔断也收不到。
    #[test]
    fn 一轮抢货只通知一次() {
        let mut h = Hunt::default();
        h.step(1000, true, 600, 0);
        for t in (1005..1600).step_by(5) {
            assert!(!h.step(t, true, 600, 0).notify, "还没到 600s 就不该通知");
        }
        assert!(h.step(1600, true, 600, 0).notify, "到点了要叫一次");
        for t in (1605..3000).step_by(5) {
            assert!(!h.step(t, true, 600, 0).notify, "叫过就不再叫");
        }
    }

    /// 新一轮抢货要重新获得叫人的资格,否则「昨晚叫过了」会让今晚的断供静默。
    #[test]
    fn 退出后再次缺货是新的一轮可以再通知() {
        let mut h = Hunt::default();
        h.step(1000, true, 600, 0);
        assert!(h.step(1600, true, 600, 0).notify);
        let s = h.step(1605, false, 600, 0);
        assert!(s.exited && s.was_notified, "退出时要告诉调用方此前叫过人");

        h.step(2000, true, 600, 0);
        assert!(h.step(2600, true, 600, 0).notify, "新一轮必须能重新通知");
    }

    #[test]
    fn 没叫过人就不该在恢复时报喜() {
        let mut h = Hunt::default();
        h.step(1000, true, 600, 0);
        let s = h.step(1030, false, 600, 0);
        assert!(s.exited);
        assert!(!s.was_notified, "只抢了 30 秒就买到了,不必打扰人");
    }

    #[test]
    fn 通知阈值为零表示不通知() {
        let mut h = Hunt::default();
        h.step(1000, true, 0, 0);
        for t in (1005..9000).step_by(300) {
            assert!(!h.step(t, true, 0, 0).notify);
        }
    }

    /// 上限为 0 = 一直抢。默认就是它 —— 实测断供能连着 4.7 小时,
    /// 而那 4.7 小时里池子是空的,「抢累了歇会儿」等于主动放弃最要紧的时段。
    #[test]
    fn 上限为零时永远不会自己退回常规轮询() {
        let mut h = Hunt::default();
        h.step(0, true, 0, 0);
        for t in (60..86400).step_by(600) {
            assert!(!h.step(t, true, 0, 0).exhausted);
        }
        assert!(h.active());
    }

    #[test]
    fn 到时长上限就退回常规轮询且状态清干净() {
        let mut h = Hunt::default();
        h.step(1000, true, 0, 60);
        let s = h.step(1060, true, 0, 60);
        assert!(s.exhausted);
        assert!(!s.exited, "「抢累了」和「有货了」在流水里是两句话");
        assert!(!h.active(), "退回常规轮询后状态必须清干净");

        // 下一轮仍然缺货 → 重新进入,重新计时。
        let s = h.step(1065, true, 0, 60);
        assert!(s.entered);
        assert_eq!(s.waited, 0);
    }

    /// 两个阈值配成一样时,两件事都要发生 —— 用枚举表达就会丢掉一个。
    #[test]
    fn 通知与上限同时到达时两个动作都要发出() {
        let mut h = Hunt::default();
        h.step(1000, true, 60, 60);
        let s = h.step(1060, true, 60, 60);
        assert!(s.notify, "人必须知道「抢不到」");
        assert!(s.exhausted, "也必须知道「已经退回常规轮询」");
    }

    #[test]
    fn 失去租约后重置不会把旧时长带进下一轮() {
        let mut h = Hunt::default();
        h.step(1000, true, 600, 0);
        h.reset();
        assert!(!h.active());
        let s = h.step(9999, true, 600, 0);
        assert!(s.entered);
        assert_eq!(s.waited, 0, "重新当选后必须从零开始计时,否则通知里的时长是假的");
    }

    /// 时钟回拨不能把 `waited` 变成负数(通知里出现「已等待 -3 分钟」)。
    #[test]
    fn 时钟回拨不会算出负的等待时长() {
        let mut h = Hunt::default();
        h.step(1000, true, 600, 0);
        let s = h.step(900, true, 600, 0);
        // 要保证的是不会因为负数意外触发通知或上限(两者都用 `>=` 比较)。
        assert!(!s.notify);
        assert!(!s.exhausted);
    }
}
