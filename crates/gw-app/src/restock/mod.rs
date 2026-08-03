//! 自动补货:从 drop.kiro.ss 买 Kiro `ksk_` 号,导入 caio 并提到高优先级。
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
pub mod params;

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

/// drop 的库存/余额在快照里的最长保鲜期。面板每 15s 轮询,但这两个数不需要秒级精度,
/// 没必要为此持续打对方接口。
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
    if !cfg.is_configured() {
        if cfg.enabled {
            tracing::warn!("补货:restock.enabled 为 true 但 api_key 为空,不启动(fail-closed)");
        }
        return;
    }
    let drop_client = match drop::DropClient::new(cfg.base_url(), &cfg.api_key) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("补货:drop 客户端构造失败,不启动: {e}");
            return;
        }
    };
    // worker /health 与 /sync 都是 loopback,短超时即可 —— worker 离线要快速跳过,
    // 不能让补货循环卡在一个下线的 worker 上。
    let http = match reqwest::Client::builder().timeout(Duration::from_secs(3)).build() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("补货:HTTP 客户端构造失败,不启动: {e}");
            return;
        }
    };
    let eng = Arc::new(engine::Engine {
        store,
        drop: drop_client,
        workers: Arc::new(workers),
        http,
    });
    let holder = holder_id();
    tracing::info!(holder = %holder, "补货:后台循环已启动(是否真正执行由 DB 租约决定)");

    tokio::spawn(async move {
        let mut reconciled = false;
        let mut last_reclaim = 0i64;
        let mut last_prune = 0i64;
        loop {
            let p = eng.params();
            let interval = p.poll_interval_secs.clamp(10, 3600);

            // ① 先抢租约。抢不到 = 别的 router 在跑,本轮什么都不做。
            //    注意**连汇总都不做** —— 两个进程同时推进同一个游标虽然有事务保护,
            //    但白白重复扫表没有意义。
            let won = eng
                .store
                .try_acquire_restock_lease(&holder, interval * LEASE_TTL_MULT)
                .unwrap_or(false);
            if !won {
                tokio::time::sleep(Duration::from_secs(interval as u64)).await;
                continue;
            }

            // ② 启动对账只做一次,且必须在当选之后 —— 没当选的进程不该去重放订单。
            if !reconciled {
                reconciled = true;
                eng.reconcile_pending().await;
            }

            // ③ 积分汇总(读 usage_records → 物化小时聚合)。
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

            // ④ 面板快照(健康度每轮、drop 库存 300s 节流)。
            let started = engine::now_ts();
            eng.refresh_snapshot(STOCK_MAX_AGE_SECS, false).await;

            // ⑤ 决策。整轮兜住异常 —— 补货服务自己崩掉比某一轮判断失败严重得多。
            match tokio::time::timeout(Duration::from_secs(120), eng.run_once(false)).await {
                Ok(d) if d.act => {
                    // 刚下过单,余额和库存都变了。不强刷的话面板会在几分钟内显示
                    // 购买前的余额 —— 看到「买了但余额没动」必然让人以为没扣款。
                    eng.refresh_snapshot(STOCK_MAX_AGE_SECS, true).await;
                }
                Ok(_) => {}
                Err(e) => tracing::error!("补货:本轮决策超时被放弃: {e}"),
            }

            // ⑤ 回收与清理。
            let now = engine::now_ts();
            if now - last_reclaim >= 3600 {
                last_reclaim = now;
                eng.reclaim().await;
            }
            if now - last_prune >= 86400 {
                last_prune = now;
                let _ = eng.store.restock_prune_decisions(14);
            }

            let elapsed = (engine::now_ts() - started).max(0);
            let wait = (interval - elapsed).max(1) as u64;
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
    });
}
