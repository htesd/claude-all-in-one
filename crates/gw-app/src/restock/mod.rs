//! 自动补货:从**多家货源**买 Kiro `ksk_` 号,导入 caio 并提到高优先级。
//!
//! 货源目前两家:`drop.kiro.ss` 与 `kiroapp.io`。接第二家的理由**不是价差**
//! (两边贴身跟价),而是**缺货**:近 7 天 drop 有 854 轮「水位已破、闸门全过、就差有货」,
//! 其中 2026-08-04 一天连断 4.7 小时,那期间流量全落到贵号池(¥0.068/积分 vs 自购号
//! 中位 ¥0.021)。多一家的价值是「总有一家有货」,不是「省那几毛」。
//!
//! 选家规则只有一条:**能买的里面最便宜的那个**,见 [`engine::choose_shelf`]。
//! 货源的抽象见 [`supplier`],名册与配置见 [`registry`]。
//!
//! **背景**:Kiro 企业号约每 1–3 小时死一批(上游 403 `TEMPORARILY_SUSPENDED`,实测
//! 0/35 个号在冷却后恢复过),夜里无人补号就断供。本模块把「买」和「上号」串起来。
//!
//! **它会真的花钱**,所以每个决定都带闸门,且默认全关:
//! - `system.yaml` 的 `restock.enabled` 决定这个二进制允不允许参与(默认 false);
//! - DB 里的运行开关决定业务上开不开(默认 false,面板可改);
//! - **DB 租约决定哪个进程真正跑** —— 生产上有两个以上 `--mode router`,
//!   共用同一个 control.db,不做互斥就是各买各的、重复扣款。
//!
//! 与旧的独立 Python 服务(`/root/kiro-restock`)相比,搬进来之后上号不再走 HTTP,
//! 而且积分数据源换成了 `usage_records`(**永不裁剪**,线上已有 51 天历史),
//! 周画像因此上线即成熟,没有冷启动。

pub mod drop;
pub mod engine;
pub mod forecast;
pub mod hunt;
pub mod kiroapp;
pub mod notify;
pub mod params;
pub mod registry;
pub mod supplier;

use std::sync::Arc;
use std::time::Duration;

use gw_core::config::{RestockConfig, WorkerConfig};
use gw_store::SqliteStore;

/// 租约 TTL 相对轮询间隔的倍数。3 倍留出「一轮跑久了」的余量,
/// 又不至于在持有者被 SIGKILL 后让接任者等太久。
const LEASE_TTL_MULT: i64 = 3;

/// 每轮最多推进多少条 `usage_records` 的聚合。
///
/// 首次上线要回填 51 天(约 105 万行)。一次干完会长时间占住只读连接,
/// 所以切成批、每轮跑几批,几分钟内追平,期间不影响数据面。
const ROLLUP_BATCH: i64 = 50_000;
const ROLLUP_BATCHES_PER_TICK: usize = 8;

/// 各家的库存/余额在快照里的最长保鲜期。面板每 15s 轮询,但这两个数不需要秒级精度,
/// 没必要为此持续打对方接口(而且现在是**乘以货源家数**的请求量)。
const STOCK_MAX_AGE_SECS: i64 = 300;

/// 本进程在租约里的身份。同一台机上多个 router 容器的 pid 可能相同
/// (各自 pid namespace),所以再拼一个启动时刻。
fn holder_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    format!(
        "{}-{}-{nanos:09}",
        hostname(),
        std::process::id()
    )
}

fn hostname() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|_| "unknown".into())
}

/// 起补货后台循环。**只在配置完整时启动**,且真正干活前还要抢到 DB 租约。
///
/// 为什么租约不可省:生产上有两个以上 `--mode router` 进程(kiro 一个、dario 一个,
/// 开了 exp 栈还有第三个),共用同一个 control.db。不做互斥就是各买各的、重复扣款。
/// 这条对「将来再加一个 router」「部署时新旧容器短暂重叠」同样成立,不依赖部署纪律。
pub fn spawn(store: Arc<SqliteStore>, workers: Vec<WorkerConfig>, cfg: &RestockConfig) {
    // 门只看「这个二进制允不允许参与」。**哪些货源可用交给名册** ——
    // 原先这里要求 yaml 里必须有 drop 的密钥,那会让「只配了 kiroapp」的部署起不来,
    // 而那恰恰是多供应商要支持的形态。名册为空时循环照跑,只是每轮都跳过,不花钱。
    if !cfg.enabled {
        return;
    }
    if cfg.api_key.trim().is_empty() {
        tracing::warn!("补货:system.yaml 未配 drop 密钥;drop 这家将被跳过,除非名册里另填");
    }
    // worker /health 与 /sync 都是 loopback,短超时即可 —— worker 离线要快速跳过,
    // 不能让补货循环卡在一个下线的 worker 上。
    let http = match reqwest::Client::builder().timeout(Duration::from_secs(3)).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("补货:HTTP 客户端构造失败,不启动: {e}");
            return;
        }
    };
    let holder = holder_id();
    let workers = Arc::new(workers);
    let cfg = cfg.clone();
    let mut eng = Arc::new(engine::Engine::build(
        store,
        &cfg,
        workers.clone(),
        http.clone(),
        holder.clone(),
    ));
    if eng.suppliers.is_empty() {
        tracing::warn!("补货:名册里没有任何可用货源,循环仍会启动(面板加货源后自动生效)");
    }
    tracing::info!(
        holder = %holder,
        suppliers = eng.suppliers.len(),
        "补货:后台循环已启动(是否真正执行由 DB 租约决定)"
    );

    tokio::spawn(async move {
        let mut last_reclaim = 0i64;
        let mut last_prune = 0i64;
        let mut last_rollup = 0i64;
        let mut last_snapshot = 0i64;
        let mut roster_seen = eng.roster.clone();
        // ── 抢货模式(2026-08-06)──
        //
        // 缺货时本循环改用 `hunt_interval_secs` 重探。**只在「闸门全过、就差有货」
        // 时进入**(见 `Decision::out_of_stock`)。其它任何跳过理由都不提速:
        // 闸门不会因为多问几次就放行。状态机本身是纯逻辑,见 [`hunt`]。
        let mut hunt = hunt::Hunt::default();
        loop {
            let p = eng.params();
            let interval = p.poll_interval_secs.clamp(10, 3600);
            let hunt_interval = p.hunt_interval_secs.clamp(2, 300);
            let hunting = hunt.active();

            // ⓪ 名册变了就重建客户端。面板改完**下一轮生效**,与其它补货参数同款,
            //    不用重启 —— 断供时能立刻加一家,正是多供应商最要紧的那个动作。
            //    重建整个 Engine 而不是原地改字段:客户端里带着连接池,换密钥必须换实例。
            let raw = eng.store.get_kv(registry::KEY_SUPPLIERS).ok().flatten();
            let roster_now = registry::parse_roster(raw.as_deref());
            if roster_now != roster_seen {
                tracing::warn!("补货:货源名册已变更,重建客户端");
                roster_seen = roster_now;
                eng = Arc::new(engine::Engine::build(
                    eng.store.clone(),
                    &cfg,
                    workers.clone(),
                    http.clone(),
                    holder.clone(),
                ));
            }

            // ① 先抢租约。抢不到 = 别的 router 在跑,本轮什么都不做。
            //    注意**连汇总都不做** —— 两个进程同时推进同一个游标虽然有事务保护,
            //    但白白重复扫表没有意义。
            //
            //    ⚠️ TTL 一律按**常规轮询间隔**算,绝不能用抢货间隔:抢货时 TTL 会缩到
            //    15 秒,而一旦退出抢货就要睡 30 秒 —— 租约在睡眠中途就过期了,
            //    另一个 router 顺势接管,两边轮流当 leader。提速反而把互斥搞坏。
            let won = eng
                .store
                .try_acquire_restock_lease(&holder, interval * LEASE_TTL_MULT)
                .unwrap_or(false);
            if !won {
                // 不是 leader 就不该维持抢货状态 —— 否则重新当选时会带着一份
                // 早已过期的「已等待多久」,通知里的时长直接失真。
                hunt.reset();
                tokio::time::sleep(Duration::from_secs(interval as u64)).await;
                continue;
            }

            // ② 对账。**每轮都跑**,且必须在当选之后 —— 没当选的进程不该去重放订单。
            //
            // 原先只在进程当选后跑一次(`if !reconciled`)。那样只覆盖得到「重启前
            // 遗留的在途单」,而**运行期新产生的在途单**(购买被 120s 外层超时掐断、
            // 或网络失败后按新规则保持 `pending`)永远等不到确认 —— 只要进程不重启,
            // 那笔钱就一直悬着。
            //
            // 每轮跑的代价接近零:没有够龄的在途单时 `reconcile_pending` 一次上游请求
            // 都不发。「够龄」这道过滤是安全必需的,见 `restock_pending_orders` 的文档。
            eng.reconcile_pending().await;

            // ③ 积分汇总(读 usage_records → 物化小时聚合)。
            //    抢货时降到每 60s 一次:它是批量补历史的活,晚几十秒毫无影响,
            //    而抢货那一轮的每一毫秒都在跟别人抢同一批号。
            //    (需求速率不受影响 —— `restock_recent_credit_rate` 直接读
            //    `usage_records`,不经这份物化聚合。)
            let now0 = engine::now_ts();
            if !hunting || now0 - last_rollup >= 60 {
                last_rollup = now0;
                for _ in 0..ROLLUP_BATCHES_PER_TICK {
                    match eng.store.restock_rollup_advance(ROLLUP_BATCH) {
                        Ok((_, more)) => {
                            if !more {
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("补货:积分汇总失败: {e}");
                            break;
                        }
                    }
                }
            }

            // ④ 面板快照(健康度每轮、各家库存 300s 节流)。
            //    抢货时降到每 30s:决策本身每轮都在问库存,快照只负责面板显示,
            //    没必要跟着一起加密度。
            let started = engine::now_ts();
            if !hunting || started - last_snapshot >= 30 {
                last_snapshot = started;
                eng.refresh_snapshot(STOCK_MAX_AGE_SECS, false).await;
            }

            // ⑤ 决策。整轮兜住异常 —— 补货服务自己崩掉比某一轮判断失败严重得多。
            //    抢货时**不逐轮写跳过流水**,见 `run_once_opts` 的注释。
            let outcome = match tokio::time::timeout(
                Duration::from_secs(120),
                eng.run_once_opts(false, !hunting),
            )
            .await
            {
                Ok(d) => {
                    if d.act {
                        // 刚下过单,余额和库存都变了。不强刷的话面板会在几分钟内显示
                        // 购买前的余额 —— 看到「买了但余额没动」必然让人以为没扣款。
                        eng.refresh_snapshot(STOCK_MAX_AGE_SECS, true).await;
                        last_snapshot = engine::now_ts();
                    }
                    Some(d)
                }
                Err(e) => {
                    tracing::error!("补货:本轮决策超时被放弃: {e}");
                    None
                }
            };

            // ⑥ 抢货状态机。
            //
            //    **决策超时(`None`)时不推进状态机** —— 一次超时不代表有货了,
            //    按「非缺货」处理会让它进进出出,把流水搅成一堆开关记录。
            let now = engine::now_ts();
            if let Some(d) = &outcome {
                let st = hunt.step(now, d.out_of_stock, p.notify_after_secs, p.hunt_max_secs);
                let mmss = |s: i64| format!("{} 分 {} 秒", s / 60, s % 60);

                if st.entered {
                    eng.log_msg(
                        "skip",
                        &format!("缺货,进入抢货模式:改为每 {hunt_interval}s 重探(期间不再逐轮记流水)"),
                    );
                    tracing::warn!("补货:缺货,进入抢货模式,每 {hunt_interval}s 重探一次");
                }

                // 抢不到就得叫人 —— 对方长时间不上架时机器已经尽力了,
                // 剩下的只能是人去别处想办法,而人得先知道。
                if st.notify {
                    eng.notify(
                        notify::EV_OUT_OF_STOCK,
                        &format!(
                            "【补货抢不到号】已连续 {} 没货(探测 {} 次)。\n当前存活 ksk_ 号 {} 个。\n{}",
                            mmss(st.waited),
                            st.probes,
                            d.healthy.unwrap_or(-1),
                            d.reason,
                        ),
                        serde_json::json!({
                            "waited_secs": st.waited,
                            "probes": st.probes,
                            "healthy": d.healthy,
                            "reason": d.reason,
                        }),
                    )
                    .await;
                }

                if st.exhausted {
                    eng.log_msg(
                        "skip",
                        &format!(
                            "抢货已持续 {}、探测 {} 次仍无货,退回 {interval}s 常规轮询",
                            mmss(st.waited),
                            st.probes
                        ),
                    );
                    tracing::warn!("补货:抢货达时长上限,退回常规轮询");
                }

                // 抢货实况落库,面板据此显示「抢货中,已 N 分钟」。
                // 抢货期间不写决策流水,没有这一条的话面板会连着几小时一动不动。
                //
                // `at` 是心跳:进程被 SIGKILL 时这个键会原样留在库里,
                // 没有心跳的话面板会永远显示「正在抢货」。读侧据此判过期。
                //
                // 只在抢货中或刚结束时写:平时每轮写一个空串是纯粹的无用写入。
                if hunt.active() || st.exited || st.exhausted {
                    let _ = eng.store.upsert_kv(
                        engine::KEY_HUNT,
                        &if hunt.active() {
                            serde_json::json!({
                                "since": now - st.waited,
                                "at": now,
                                "probes": st.probes,
                                "interval_secs": hunt_interval,
                                "notified": hunt.notified(),
                            })
                            .to_string()
                        } else {
                            String::new()
                        },
                    );
                }

                if st.exited {
                    eng.log_msg(
                        "skip",
                        &format!(
                            "退出抢货模式:探测 {} 次、持续 {} —— {}",
                            st.probes,
                            mmss(st.waited),
                            d.reason
                        ),
                    );
                    // 只在**通知过缺货**之后才报恢复:没叫过人就不用告诉人「好了」。
                    if d.act && st.was_notified {
                        eng.notify(
                            notify::EV_RESTOCKED,
                            &format!("【补货已恢复】等了 {} 后抢到号。\n{}", mmss(st.waited), d.reason),
                            serde_json::json!({
                                "waited_secs": st.waited,
                                "probes": st.probes,
                                "reason": d.reason,
                            }),
                        )
                        .await;
                    }
                }
            }

            // ⑦ 回收与清理。
            if now - last_reclaim >= 3600 {
                last_reclaim = now;
                eng.reclaim().await;
            }
            if now - last_prune >= 86400 {
                last_prune = now;
                let _ = eng.store.restock_prune_decisions(14);
            }

            // 抢货时按 `hunt_interval` 走。减去本轮耗时是有意的:询价往返 3–4 秒,
            // 「间隔」指的是**周期**而不是间隙,否则 5s 的设置实际会变成 9s 一轮。
            //
            // 取 `min` 而不是直接用 `hunt_interval`:抢货**永远不该比常规轮询更慢**。
            // 把它调到大于轮询间隔(比如为了收敛请求密度调到 60s,而轮询是 30s)
            // 本意是「别抢那么凶」,直接用的话会变成「缺货时反而看得更少」。
            // 顺带这也给出了干净的关闭方式:调到 ≥ 轮询间隔即等于不提速。
            let elapsed = (engine::now_ts() - started).max(0);
            let base = if hunt.active() { hunt_interval.min(interval) } else { interval };
            let wait = (base - elapsed).max(1) as u64;
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
    });
}
