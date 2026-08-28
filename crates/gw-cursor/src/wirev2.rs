//! wire v2 的**反应式帧序驱动**:生产与探针共用的唯一一份实现。
//!
//! ## 为什么必须只有一份(codex 终审 #4)
//!
//! 早先探针自己拼完 13 帧一次性 half-close、并把所有点名硬编码成「拿不出来」,
//! 生产走另一套反应式路径。两份实现的直接后果是**探针结论不可迁移**:它既验证不了
//! 内容槽真的能上传成功,还会把生产其实供得出的点名误报成失败。
//!
//! 所以把「反应式帧序 + 点名应答」这两件必须一致的事收进 [`WireDriver`],
//! 由双方各自驱动。**帧的构造留在调用方**——探针要能故意递一个坏描述符做负面对照,
//! 那是 go/no-go 的另一半(验回退检测器真响),不能塞进生产路径当 test-only 钩子。
//!
//! ## 帧序(2026-08-23 实物,三轮一致)
//!
//! ```text
//! 帧0 →  服务端 ack {1:{13:""}}
//!     →  [缓存过期时] 4.2 逐条点名 → 客户端交内容槽(slot 0,1,2…)
//!     →  会话登记通知 {2:{10:{2:conv_id},…}}
//!     →  客户端才发 ENV + 控制帧
//! ```
//!
//! 触发 ENV 的是**会话登记通知**,不是「内容交换完成」:turn4 通知来得晚是因为
//! 服务端要先拿到内容才能登记会话,点名先于 ENV 是结果不是原因。turn1/turn2 零点名、
//! 通知直接来、ENV 随即发 —— 按通知触发对两种情形都成立。

use std::time::{Duration, Instant};

use crate::run;

/// 请求体通道的发送端(HTTP/2 请求流保持打开时往它灌帧)。
pub type FrameSink = tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>;

/// 往请求体通道发一帧的**上限**。
///
/// 裸 `send().await` 会在 HTTP/2 流控停排时无界等待,而响应读取、stall 看门狗、
/// 下游关闭检测都在同一个任务里 —— 一次停排把三件事一起冻住,表现是「号没坏但永远
/// busy」。加上限之后最坏情况只是这一帧发失败,由调用方按内容供给失败走判决。
pub const SEND_TIMEOUT: Duration = Duration::from_secs(10);

/// 推迟帧的兜底期限:没等到会话登记通知也要把 ENV 发出去,免得死等。
///
/// ⚠️ 调用方**必须**在自己的 `select!` 里挂一个独立的
/// `sleep_until(driver.deferred_deadline())` 分支(见 [`WireDriver::deferred_deadline`])。
/// 只在「收到响应帧时」检查这个期限是不够的:ack 之后如果上游静默,根本没有帧来触发
/// 检查,ENV 就会一路拖到 stall 闸才被发现。
pub const DEFERRED_DEADLINE: Duration = Duration::from_secs(3);

/// 描述符是否**可被接受**(生产与探针共用这一条判定,codex 四轮 #4)。
///
/// `.3` 只在 turn_commit 同帧或之后才作数。commit 之前收到的是**未完结轮次的中间态**:
/// 提交它 = 下一轮回放一个缺本轮的状态,而精确匹配挡不住(链是对的、内容是错的)。
///
/// 实物依据(2026-08-23 三轮,尾段顺序完全一致):
/// `提交记录 → .3 ×3 → 1.14 用量帧`。提交记录**永远**先到,所以这道门不会误伤任何
/// 真实描述符;缺提交记录的流是合成出来的形态。
///
/// 抽成函数是因为探针原先「任何帧见 `.3` 就覆盖」,会回放生产明确拒绝的中间态 ——
/// 两边规则不同,探针结论就不可迁移。
pub fn descriptor_acceptable(draining: bool, commit_frame: bool) -> bool {
    draining || commit_frame
}

/// 处理一帧的副作用汇总(调用方据此更新自己的计数与日志)。
#[derive(Debug, Clone, Copy, Default)]
pub struct FrameEffect {
    /// 本帧收下的回显内容节数。
    pub sections_stored: usize,
    /// 本帧被点名的分节数。
    pub demanded: usize,
    /// 其中**没供出去**的(表里没有,或发送失败)。这才是失败信号。
    pub unavailable: usize,
    /// 本帧触发发出的推迟帧数。
    pub deferred_sent: usize,
    /// 本帧是不是会话登记通知。
    pub session_notice: bool,
}

/// 反应式帧序驱动。一次请求一个实例。
pub struct WireDriver {
    /// ENV + 控制帧,等会话登记通知(或兜底期限)后按序发出。
    deferred: Vec<Vec<u8>>,
    /// 内容分节库:存回显、按 hash 供给。
    sections: std::sync::Arc<crate::ContentSections>,
    /// 下一个内容槽序号:**按点名顺序**从 0 递增(实物 7 个需求 → slot 0..6)。
    next_slot: u64,
    started: Instant,
    /// 主段(turn_commit 之前)累计 —— **只有这段进本轮 verdict**。
    demanded_total: usize,
    unavailable_total: usize,
    /// 排水段(turn_commit 之后)累计 —— 仅供观测。
    ///
    /// 隔离理由(codex 四轮 #2 / 五轮 #1):commit 之后本轮已经成功,尾帧里若出现
    /// `4.2` 而我方供不出,计进本轮就会把一个**已提交的成功轮**判成
    /// `ContentUnavailable` → 回退失败 → 客户拿到错误,而模型其实已正常答完。
    /// 生产早先在调用侧用 `if !draining` 隔离;现在收进 driver,**探针自动同源**。
    drain_demanded: usize,
    drain_unavailable: usize,
    /// 是否已进入排水段(由调用方在 turn_commit 时置位)。
    draining: bool,
    /// **供给失败锁存**(codex 四轮 #3 + 五轮 #2)。
    ///
    /// 每个点名各带一次 [`SEND_TIMEOUT`] 的话,7 个点名最坏能堵 ~70s,而响应读取、
    /// stall 看门狗、下游关闭检测都在同一个任务里 —— 等于把看门狗按住 70 秒。
    /// 更糟的是槽位错位:首次失败时 `next_slot` 不前进,恢复后下一份内容会被塞进
    /// **前一个需求**的槽号里,服务端拿到的内容与它点名的 hash 对不上。
    ///
    /// 所以一旦失败就锁存,本轮后续供给一律**快速失败**(不再尝试发送、直接计
    /// unavailable)。宁可本轮判 `ContentUnavailable` 重铺,也不许串槽。
    ///
    /// ⚠️ **缺节(`sections.get()` 为 None)同样要锁存**(五轮 #2,真 bug):
    /// 早先只在发送失败时锁存,于是「第一个 hash 缺失、第二个命中」时,`next_slot`
    /// 还停在 0,第二份内容就被发进**第一个需求**的槽号 —— 服务端拿到的内容与它点名
    /// 的 hash 对不上。负控②(把首节改成随机 ref)每次都会踩这条路。
    supply_failed: bool,
    /// 单帧发送上限。生产恒为 [`SEND_TIMEOUT`];**测试可调小** —— 用 10s 真值去验
    /// 「锁存之后不再逐个等超时」那条断言要跑 30 秒。
    send_timeout: Duration,
}

impl WireDriver {
    pub fn new(deferred: Vec<Vec<u8>>, sections: std::sync::Arc<crate::ContentSections>) -> Self {
        Self {
            deferred,
            sections,
            next_slot: 0,
            started: Instant::now(),
            demanded_total: 0,
            unavailable_total: 0,
            drain_demanded: 0,
            drain_unavailable: 0,
            draining: false,
            supply_failed: false,
            send_timeout: SEND_TIMEOUT,
        }
    }

    /// 测试用:把单帧发送上限调小(见 [`Self::send_timeout`])。
    #[cfg(test)]
    pub(crate) fn set_send_timeout(&mut self, d: Duration) {
        self.send_timeout = d;
    }

    /// 推迟帧的兜底期限(绝对时刻)。调用方在 `select!` 里挂独立分支用它。
    pub fn deferred_deadline(&self) -> Instant {
        self.started + DEFERRED_DEADLINE
    }

    pub fn has_deferred(&self) -> bool {
        !self.deferred.is_empty()
    }

    /// 进入排水段(turn_commit 之后)。之后的供给结果不再进本轮 verdict。
    pub fn set_draining(&mut self) {
        self.draining = true;
    }

    /// **主段**被点名数(进本轮 verdict)。
    pub fn demanded(&self) -> usize {
        self.demanded_total
    }

    /// **主段**拿不出来的数(进本轮 verdict)。
    pub fn unavailable(&self) -> usize {
        self.unavailable_total
    }

    /// 排水段的观测值 `(点名, 拿不出)` —— 不进 verdict。
    pub fn drain_counts(&self) -> (usize, usize) {
        (self.drain_demanded, self.drain_unavailable)
    }

    /// 带期限地发一帧。
    async fn send(&self, sink: Option<&FrameSink>, frame: Vec<u8>) -> Result<(), &'static str> {
        let Some(sink) = sink else {
            return Err("请求流未保持打开");
        };
        match tokio::time::timeout(self.send_timeout, sink.send(Ok(bytes::Bytes::from(frame))))
            .await
        {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err("请求流已关"),
            Err(_) => Err("发送超时(HTTP/2 流控停排?)"),
        }
    }

    /// 把推迟的 ENV + 控制帧按序发出,返回实际发出的帧数。
    pub async fn flush_deferred(&mut self, sink: Option<&FrameSink>, why: &str) -> usize {
        if self.deferred.is_empty() {
            return 0;
        }
        let total = self.deferred.len();
        let mut sent = 0usize;
        for f in std::mem::take(&mut self.deferred) {
            if let Err(e) = self.send(sink, f).await {
                tracing::warn!(why, sent, total, error = e, "wire v2:推迟帧发送中断");
                break;
            }
            sent += 1;
        }
        tracing::debug!(why, sent, total, "wire v2:推迟帧(ENV + 控制帧)已发出");
        sent
    }

    /// 处理一个响应帧:①收下回显的内容节 ②应答 `4.2` 点名 ③按登记通知发推迟帧。
    ///
    /// 顺序有意义:先存回显再应答点名 —— 同一帧里既回显又点名时,刚回显的那节
    /// 必须能立刻供出去。推迟帧最后发,与实物一致(点名先于 ENV)。
    pub async fn on_frame(&mut self, payload: &[u8], sink: Option<&FrameSink>) -> FrameEffect {
        let mut eff = FrameEffect::default();

        // ① 存入:服务端创建的每个内容节都会回显一次,节字节就是 `4.3.2` 那层的
        //    原始字节,sha256 即描述符里的引用。没存下来,后续被点名就交不出去。
        for s in run::section_echoes(payload) {
            self.sections.insert(s);
            eff.sections_stored += 1;
        }

        // ② 应答:按点名顺序交出内容槽。**点名本身不是错误**(缓存过期后按需索取
        //    是正常协议事件),失败的只有「点名了我方拿不出来」。
        for h in run::content_hash_demands(payload) {
            eff.demanded += 1;
            if self.draining {
                self.drain_demanded += 1;
            } else {
                self.demanded_total += 1;
            }
            // 锁存后快速失败:不再 await 任何发送,免得每个点名各堵一个 SEND_TIMEOUT,
            // 也免得 slot 与需求错位。
            let served = if self.supply_failed {
                false
            } else {
                match self.sections.get(&h) {
                    Some(data) => {
                        let frame = crate::wire::frame(&crate::cli::content_slot_frame(
                            self.next_slot,
                            &data,
                        ));
                        match self.send(sink, frame).await {
                            Ok(()) => {
                                // slot 与需求严格对位:只有真发出去才前进。
                                self.next_slot += 1;
                                true
                            }
                            Err(e) => {
                                self.supply_failed = true;
                                tracing::warn!(
                                    error = e,
                                    slot = self.next_slot,
                                    "wire v2:内容槽发不出去,本轮后续供给快速失败(防串槽)"
                                );
                                false
                            }
                        }
                    }
                    None => {
                        // 缺节也锁存:后续命中若继续发,就会用错的 slot 号。
                        self.supply_failed = true;
                        tracing::warn!(
                            slot = self.next_slot,
                            "wire v2:点名的分节不在表内,本轮后续供给快速失败(防串槽)"
                        );
                        false
                    }
                }
            };
            if !served {
                eff.unavailable += 1;
                if self.draining {
                    self.drain_unavailable += 1;
                } else {
                    self.unavailable_total += 1;
                }
            }
        }

        // ③ 推迟帧:会话登记通知是触发器(见模块头的帧序说明)。
        if run::is_session_notice(payload) {
            eff.session_notice = true;
            eff.deferred_sent = self.flush_deferred(sink, "会话登记通知").await;
        }
        eff
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 造一帧带 `n` 个 `4.2` 内容点名的响应帧。
    ///
    /// ⚠️ 字节**手写**,不用 `Writer`/解析器同一套常量(lessons §7:逆向出来的协议
    /// 若拿同一套常量造帧再解,字段号整套写错也照样绿)。
    /// 结构 `{4:{2:{1:bin[32]}}}` = `22 24 | 12 22 | 0a 20 | <32B>`。
    fn demand_frame(hashes: &[[u8; 32]]) -> Vec<u8> {
        let mut out = Vec::new();
        for h in hashes {
            out.extend_from_slice(&[0x22, 0x24, 0x12, 0x22, 0x0a, 0x20]);
            out.extend_from_slice(h);
        }
        out
    }

    /// 高3 失败路径:首个点名发送失败后**锁存**,本轮后续供给快速失败。
    ///
    /// 三条断言对应 codex 四轮 #3 的三个要求:
    /// ① 不再逐个等 timeout —— 3 个点名只付一次超时的时间;
    /// ② slot 与需求严格对位 —— 锁存后一个字节都不再发,不可能串槽;
    /// ③ 本轮 verdict 落 `ContentUnavailable`。
    #[tokio::test]
    async fn 发送失败锁存_不叠加超时且不串槽() {
        let sections = std::sync::Arc::new(crate::ContentSections::default());
        // 三节都在表里 —— 保证「拿不出来」只可能来自发送失败,不是缺节。
        let h1 = sections.insert(b"section-one");
        let h2 = sections.insert(b"section-two");
        let h3 = sections.insert(b"section-three");
        assert_eq!(sections.len().1, 3, "三节都应在表内");

        let mut driver = WireDriver::new(Vec::new(), sections.clone());
        // 100ms:没有锁存的话 3 个点名要付 3 次 ≈300ms。
        driver.set_send_timeout(Duration::from_millis(100));

        // 容量 1 且预先塞满、接收端**保持存活**:后续 send 会一直阻塞到超时。
        // (丢掉 rx 会让 send 立刻返回 Err,那样测不出「叠加超时」这件事。)
        let (tx, _rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(1);
        tx.try_send(Ok(bytes::Bytes::from_static(b"filler")))
            .expect("预填一帧把通道占满");

        let frame = demand_frame(&[h1, h2, h3]);
        let t0 = Instant::now();
        let eff = driver.on_frame(&frame, Some(&tx)).await;
        let elapsed = t0.elapsed();

        // ① 只付一次超时:锁存生效。留足余量,但远低于 3×100ms。
        assert!(
            elapsed < Duration::from_millis(250),
            "锁存后不该逐个等超时,实测 {elapsed:?}"
        );
        assert_eq!(eff.demanded, 3, "三个点名都要计数");
        assert_eq!(eff.unavailable, 3, "一个都没供出去");

        // ② 一个内容槽都没进通道 —— 通道里只有预填那一帧,不可能串槽。
        assert_eq!(tx.capacity(), 0, "通道仍被预填帧占满,说明没有内容槽被写入");

        // 锁存是**本轮**级的:即使通道腾空,后续点名仍快速失败,绝不半途续发
        // (续发就会把内容塞进前一个需求的槽号)。
        drop(_rx);
        let eff2 = driver.on_frame(&demand_frame(&[h1]), Some(&tx)).await;
        assert_eq!(eff2.unavailable, 1, "锁存后仍然拿不出来");
        assert_eq!(driver.demanded(), 4);
        assert_eq!(driver.unavailable(), 4);

        // ③ 本轮判决落 ContentUnavailable。
        let verdict = crate::run::WireTurnOutcome {
            saw_descriptor: true,
            saw_finish: true,
            content_chars: 42,
            demanded: driver.demanded(),
            unavailable: driver.unavailable(),
        }
        .verdict();
        assert_eq!(
            verdict,
            crate::run::WireVerdict::ContentUnavailable {
                missing: 4,
                demanded: 4
            }
        );
        assert!(verdict.should_fallback(), "必须本轮回退重铺");
    }

    /// 五轮 #2(真 bug)回归:**第一个 hash 缺节、后续命中时不许串槽**。
    ///
    /// 旧实现只在「发送失败」时锁存,缺节走 `None => false` 不锁存。于是:
    /// 需求① 缺节 → `next_slot` 停在 0;需求② 命中 → 内容被发进 **slot 0**,
    /// 而服务端认为 slot 0 装的是需求①的内容 —— 需求与内容整体错位一位。
    /// 负控②(把描述符首个 ref 改成随机)每次都会走这条路。
    #[tokio::test]
    async fn 缺节也锁存_后续命中不串槽() {
        let sections = std::sync::Arc::new(crate::ContentSections::default());
        // 只放第二、三节;第一个点名故意指向一个**不在表内**的 hash。
        let missing = crate::ContentSections::hash(b"never-stored");
        let h2 = sections.insert(b"section-two");
        let h3 = sections.insert(b"section-three");

        let mut driver = WireDriver::new(Vec::new(), sections.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);
        let eff = driver
            .on_frame(&demand_frame(&[missing, h2, h3]), Some(&tx))
            .await;

        assert_eq!(eff.demanded, 3);
        assert_eq!(
            eff.unavailable, 3,
            "首个缺节即锁存 → 后两个也不许供(供了就是串槽)"
        );
        // 关键断言:**一个内容槽都没发出去**。
        assert!(
            rx.try_recv().is_err(),
            "缺节锁存后不得再发任何内容槽 —— 发了就会用错的 slot 号"
        );
        // 判决落 ContentUnavailable → 本轮重铺。
        let v = crate::run::WireTurnOutcome {
            saw_descriptor: true,
            saw_finish: true,
            content_chars: 10,
            demanded: driver.demanded(),
            unavailable: driver.unavailable(),
        }
        .verdict();
        assert_eq!(
            v,
            crate::run::WireVerdict::ContentUnavailable {
                missing: 3,
                demanded: 3
            }
        );
        assert!(v.should_fallback());
    }

    /// 排水段的供给结果**不进主段计数**(五轮 #1):否则已 commit 的成功轮会被
    /// 尾帧的一个供不出的点名翻成 `ContentUnavailable`。
    #[tokio::test]
    async fn 排水段计数不进本轮判决() {
        let sections = std::sync::Arc::new(crate::ContentSections::default());
        let missing = crate::ContentSections::hash(b"nope");
        let mut driver = WireDriver::new(Vec::new(), sections.clone());
        let (tx, _rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);

        // 进入排水段之后才出现点名。
        driver.set_draining();
        let eff = driver.on_frame(&demand_frame(&[missing]), Some(&tx)).await;

        assert_eq!(eff.demanded, 1, "本帧确实被点名了");
        assert_eq!(eff.unavailable, 1, "也确实供不出");
        assert_eq!(driver.demanded(), 0, "主段计数必须为 0");
        assert_eq!(driver.unavailable(), 0, "主段计数必须为 0");
        assert_eq!(driver.drain_counts(), (1, 1), "记进排水段观测值");
        // 本轮判决不受影响 —— 仍是 Ok。
        let v = crate::run::WireTurnOutcome {
            saw_descriptor: true,
            saw_finish: true,
            content_chars: 10,
            demanded: driver.demanded(),
            unavailable: driver.unavailable(),
        }
        .verdict();
        assert_eq!(v, crate::run::WireVerdict::Ok, "已 commit 的成功轮不该被翻");
    }

    /// 对照:通道通畅时三个点名全部供出,slot 依次 0/1/2。
    #[tokio::test]
    async fn 通道通畅时点名全部供出() {
        let sections = std::sync::Arc::new(crate::ContentSections::default());
        let h = [
            sections.insert(b"a"),
            sections.insert(b"b"),
            sections.insert(b"c"),
        ];
        let mut driver = WireDriver::new(Vec::new(), sections.clone());
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(16);
        let eff = driver.on_frame(&demand_frame(&h), Some(&tx)).await;
        assert_eq!((eff.demanded, eff.unavailable), (3, 0), "全部供出");
        let mut got = 0;
        while rx.try_recv().is_ok() {
            got += 1;
        }
        assert_eq!(got, 3, "三个内容槽都进了请求体通道");
    }
}
