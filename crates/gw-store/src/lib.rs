//! gw-store —— SQLite(WAL) 持久化。
//!
//! Phase 0:控制面最小实现(api_keys 表 + 鉴权)。WAL 模式以支持
//! 多进程(router + 多 worker)并发读、控制面写少。
//! usage / 状态机 / request_cache 在 P2/P4 按 IMPROVEMENTS.md 补。

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use gw_core::account::Account;
use gw_core::config::AccountsConfig;
use gw_core::store::{
    AccountPatch, AccountRow, ApiKeyPatch, ApiKeyRow, AuthenticatedKey, ControlStore,
    CreditRollupRow, GroupRow, LogBlob, RequestLog, RequestLogDetail, RequestLogFilter,
    RequestLogRow, RestockAccountRow, RestockDecision, RestockOrder, UsageByKey,
    UsageByModel, UsageFilter,
    UsageRecord, UsageSink, UsageSummary,
};
use rusqlite::types::Value;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension};

/// [`SqliteStore::delete_group`] 的结果。删除会把引用该组的 key 打回"未分组",
/// 而未分组会被 router 回落到主组,所以某些情况必须拒绝而不是照删(见该函数文档)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteGroupOutcome {
    Deleted,
    NotFound,
    /// 本组仍有 N 把 key 绑定;删了这些客户会静默提权到主组(见 `delete_group`)。
    HasKeys(u64),
    /// 本组仍是 N 个账号的 **owner**;删了会把归属清空,那些账号变成没有 worker 加载的
    /// 孤儿,而借用它们的其它组会当场全量 503(见 `delete_group`)。
    IsOwner(u64),
}

/// 建成员边的结果。见 [`SqliteStore::upsert_membership`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MembershipOutcome {
    Ok,
    /// 账号或分组不存在 —— 悬空边只会让该组静默少一个号,必须在写入侧拦掉。
    MissingAccountOrGroup,
    /// 该组已有成员归属 `existing`,而这个号归属 `incoming`:跨 owner 会让组内
    /// priority 不再是全局排序(见 `upsert_membership` 文档)。
    CrossOwner { existing: String, incoming: String },
}

/// 改账号配置的结果。见 [`SqliteStore::update_account`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpdateAccountOutcome {
    Ok,
    NotFound,
    /// 改**归属**会让 `group` 这个组同时出现两个 owner。
    ///
    /// `upsert_membership` 守着"一组一 owner",但改归属是从**另一头**破坏同一个不变量:
    /// 边一条没动,却把边另一端的 owner 换了。不在这里拦,后端精心维护的约束就有一条
    /// 绕行通道 —— 而且是运维在 UI 上点一下就能走通的那种。
    CrossOwner { group: String, existing: String, incoming: String },
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS api_keys (
    key       TEXT PRIMARY KEY,
    label     TEXT,
    disabled  INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- 原始 usage 事件日志(#130 UsageSink)。每次上游调用结束追加一行;
-- 上层(按账号/按客户聚合、成本统计)从此表读,本表只管忠实记录,永不裁剪。
-- client_key_id 暂可为空("":router 未把客户 key 透传给内网 worker,v61 另开按客户归属)。
CREATE TABLE IF NOT EXISTS usage_records (
    id            INTEGER PRIMARY KEY,
    client_key_id TEXT    NOT NULL DEFAULT '',
    account_id    TEXT    NOT NULL DEFAULT '',
    model         TEXT    NOT NULL DEFAULT '',
    input_tokens          INTEGER NOT NULL DEFAULT 0,
    output_tokens         INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    real_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    metering_credit        REAL    NOT NULL DEFAULT 0,
    success       INTEGER NOT NULL DEFAULT 1,
    created_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
-- 时间序(全局/按模型时间窗聚合的基础);account/client 维度各带 created_at 便于钉维度后排序。
CREATE INDEX IF NOT EXISTS idx_usage_created  ON usage_records(created_at);
CREATE INDEX IF NOT EXISTS idx_usage_account ON usage_records(account_id, created_at);
CREATE INDEX IF NOT EXISTS idx_usage_client  ON usage_records(client_key_id, created_at);

-- 分组(账号组,同时用于 key 归类)。name 即 accounts.yaml 的组名(G0/G1...)。
-- 组的成员由 account_groups 定义(N:M),本表只存组自身的元数据。
CREATE TABLE IF NOT EXISTS groups (
    name       TEXT PRIMARY KEY,
    color      TEXT NOT NULL DEFAULT '',
    note       TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- 上游账号(配置态;运行态在 worker 调度器内存里,经 /status 暴露)。
-- extra 为 provider 专属字段 JSON(refresh_token 等,与 Account.extra 对应);
-- worker 刷新 token 后回写本表,重启不再丢 rolling refresh_token。
CREATE TABLE IF NOT EXISTS accounts (
    account_id      TEXT PRIMARY KEY,
    group_name      TEXT NOT NULL DEFAULT '',
    provider        TEXT NOT NULL DEFAULT 'kiro',
    max_concurrency INTEGER NOT NULL DEFAULT 2,
    disabled        INTEGER NOT NULL DEFAULT 0,
    extra           TEXT NOT NULL DEFAULT '{}',
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_accounts_group ON accounts(group_name);

-- 账号↔分组的**成员边**(N:M)。与 accounts.group_name 是两件事,别混:
--   accounts.group_name = **归属**:哪个 worker 进程独占管理该号的运行态
--     (并发信号量/冷却/连续失败计数/rolling refresh_token 单飞刷新)。
--     必须唯一 —— 两个进程持有同一个号会让并发上限翻倍、且各自刷新出的
--     token 互相覆盖,账号直接 invalid_grant 报废。这是物理约束。
--   account_groups     = **权限 + 组内排序**:哪些客户能用它、在那个组里排第几。
-- priority 挂在**边**上而不是账号上,所以同一个号可以在 A 组当主力(0)、
-- 在 B 组当兜底(100)。低价档「小号优先、压满才溢出到主力号」就是这么配的。
CREATE TABLE IF NOT EXISTS account_groups (
    account_id TEXT NOT NULL,
    group_name TEXT NOT NULL,
    priority   INTEGER NOT NULL DEFAULT 100,
    PRIMARY KEY (account_id, group_name)
);
CREATE INDEX IF NOT EXISTS idx_acctgrp_group ON account_groups(group_name);

-- 系统热调设置(单行 key='system',value=SystemSettings JSON overlay)。
-- 字段级覆盖叠在 system.yaml 基线之上;router 写、worker 30s 轮询热应用,无需重启。
CREATE TABLE IF NOT EXISTS settings (
    key        TEXT PRIMARY KEY,
    value      TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);

-- 请求日志(调试用):每次上游调用结束追加一行,保存【发 Kiro 前的完整报文】+【用户原始
-- 报文】+ 用量/耗时元数据。**环形保留最新 N 条**(insert_request_log 按 cap 裁旧),不无限增长。
CREATE TABLE IF NOT EXISTS request_logs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at    INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    client_key_id TEXT    NOT NULL DEFAULT '',
    account_id    TEXT    NOT NULL DEFAULT '',
    model         TEXT    NOT NULL DEFAULT '',
    stream        INTEGER NOT NULL DEFAULT 0,
    success       INTEGER NOT NULL DEFAULT 1,
    status_code   INTEGER,
    error_kind    TEXT,
    duration_ms   INTEGER,
    ttfb_ms       INTEGER,
    input_tokens          INTEGER NOT NULL DEFAULT 0,
    output_tokens         INTEGER NOT NULL DEFAULT 0,
    cache_read_tokens     INTEGER NOT NULL DEFAULT 0,
    cache_creation_tokens INTEGER NOT NULL DEFAULT 0,
    reported_tokens       INTEGER NOT NULL DEFAULT 0,
    real_cache_read_tokens INTEGER NOT NULL DEFAULT 0,
    metering_credit       REAL    NOT NULL DEFAULT 0,
    client_payload TEXT   NOT NULL DEFAULT '',
    kiro_payload   TEXT   NOT NULL DEFAULT '',
    response_payload TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_reqlog_created ON request_logs(created_at);
CREATE INDEX IF NOT EXISTS idx_reqlog_account ON request_logs(account_id, created_at);

-- 媒体 blob:用户上传的图片/文档,内容寻址(hash=sha256(base64))去重存储。
-- 同一张图在一个会话里每轮都发,只存一份;报文里以 "blob:<hash>" 引用。
CREATE TABLE IF NOT EXISTS log_blobs (
    hash       TEXT    PRIMARY KEY,
    media_type TEXT    NOT NULL DEFAULT '',
    data       TEXT    NOT NULL,
    bytes      INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
-- 日志↔blob 多对多引用。日志环形裁剪时连带删本表行,再清无引用的 log_blobs(GC)。
CREATE TABLE IF NOT EXISTS log_blob_refs (
    log_id INTEGER NOT NULL,
    hash   TEXT    NOT NULL,
    PRIMARY KEY (log_id, hash)
);
CREATE INDEX IF NOT EXISTS idx_blobref_hash ON log_blob_refs(hash);

-- suspend 生命周期(事实源;内存运行态只是它的镜像):连续 suspend 次数、
-- 生命周期原因、冷却到期绝对时刻。独立建表而非塞进 accounts.extra ——
-- extra 有整体替换/导入合并路径,生命周期状态放进去会被误洗。
-- epoch:人工恢复(restore_account)单调递增;worker 的持久化写带它做
-- 条件更新(WHERE epoch = 写入方已知值)——恢复与 worker 待落库队列竞态时,
-- 旧状态写不回、退役号不会被反写重新禁用(对抗审查阻断#3)。
CREATE TABLE IF NOT EXISTS account_lifecycle (
    account_id     TEXT PRIMARY KEY,
    suspend_streak INTEGER NOT NULL DEFAULT 0,
    reason         TEXT,
    retry_at       INTEGER,
    epoch          INTEGER NOT NULL DEFAULT 0,
    revision       INTEGER NOT NULL DEFAULT 0
);

-- ── 自动补货(drop.kiro.ss 买 ksk_ 号并自动上号) ────────────────────────────
-- 订单。**幂等键必须在发出购买请求之前落到这里**:进程崩在请求途中时,重启后靠这张表
-- 才知道那个 client_order_id 是什么,用原 id 重放才能问出真实结果而不是重复扣款。
-- 金额一律记**实际扣款**(购买前后余额之差),不用报价推算——报价是 USD、扣款是 CNY。
CREATE TABLE IF NOT EXISTS restock_orders (
    client_order_id TEXT PRIMARY KEY,
    count           INTEGER NOT NULL,
    max_total_cny   REAL    NOT NULL,
    status          TEXT    NOT NULL,          -- pending/purchased/imported/failed/dry_run
    keys_json       TEXT    NOT NULL DEFAULT '[]',
    spent_cny       REAL    NOT NULL DEFAULT 0,
    balance_before  REAL,
    balance_after   REAL,
    error           TEXT    NOT NULL DEFAULT '',
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    updated_at      INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_restock_orders_created ON restock_orders(created_at);

-- 本服务买入并成功上号的账号。回收**只动这张表里的号** —— 人工上的历史死号
-- (线上有 200+)不归自动化处置。
CREATE TABLE IF NOT EXISTS restock_owned (
    account_id      TEXT PRIMARY KEY,
    client_order_id TEXT NOT NULL,
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now')),
    reclaimed_at    INTEGER
);

-- 每轮决策流水。这是"为什么没补/为什么补了"的唯一可查记录。
CREATE TABLE IF NOT EXISTS restock_decisions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts          INTEGER NOT NULL,
    action      TEXT    NOT NULL,              -- skip/buy/import/reclaim/error
    reason      TEXT    NOT NULL,
    healthy     INTEGER,
    stock       INTEGER,
    price_usd   REAL,
    balance_cny REAL,
    detail      TEXT    NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS idx_restock_decisions_ts ON restock_decisions(ts);

-- 积分消耗的小时级聚合,数据源是 usage_records(**永不裁剪**,线上已有 51 天历史)。
-- 物化成一张小表是为了让面板画图不必每次 GROUP BY 百万行;ksk 维度分开存,
-- 因为补货只关心 ksk_ 号的消耗,但总量能看出整体负载。
CREATE TABLE IF NOT EXISTS restock_credit_rollup (
    hour_ts INTEGER NOT NULL,                  -- UTC 整点 epoch
    model   TEXT    NOT NULL,
    ksk     INTEGER NOT NULL,                  -- 1 = ksk_ 号(补货的对象)
    calls   INTEGER NOT NULL DEFAULT 0,
    success INTEGER NOT NULL DEFAULT 0,
    credits REAL    NOT NULL DEFAULT 0,
    PRIMARY KEY (hour_ts, model, ksk)
);
CREATE INDEX IF NOT EXISTS idx_restock_rollup_hour ON restock_credit_rollup(hour_ts);
"#;

/// 请求日志报文 gzip 压缩后入库(BLOB)——全文不截断,文本压 5-10 倍。压缩失败极罕见
/// (内存),退回原文 UTF-8 字节(读侧按 gzip magic 区分,兼容)。
fn gzip_text(s: &str) -> Vec<u8> {
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    if enc.write_all(s.as_bytes()).is_ok() {
        if let Ok(buf) = enc.finish() {
            return buf;
        }
    }
    s.as_bytes().to_vec()
}

/// 读侧还原报文:gzip(magic `1f 8b`)→ 解压;否则按**旧明文行**(本特性前存的 TEXT)
/// 当 UTF-8 处理。解压失败兜底 lossy,绝不 panic。
fn ungzip_text(bytes: Vec<u8>) -> String {
    if bytes.len() >= 2 && bytes[0] == 0x1f && bytes[1] == 0x8b {
        let mut out = String::new();
        if GzDecoder::new(&bytes[..]).read_to_string(&mut out).is_ok() {
            return out;
        }
        // 解压失败(损坏)→ lossy 兜底。
        return String::from_utf8_lossy(&bytes).into_owned();
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// 报文列读取:新行是 gzip BLOB、旧行是明文 TEXT,故按动态 `Value` 取再还原
/// (rusqlite 的 `Vec<u8>` FromSql 只认 BLOB,不能直接读旧 TEXT 行)。
fn value_to_payload(v: Value) -> String {
    match v {
        Value::Blob(b) => ungzip_text(b),
        Value::Text(s) => s, // 旧明文行,直接用
        _ => String::new(),
    }
}

/// SQLite 控制面存储。
///
/// 连接用 Mutex 包裹:Phase 0 控制面访问量极低,单连接足够;
/// P4 若热点化再换连接池。WAL 模式允许跨进程并发读。
///
/// `stats_conn` 是统计聚合专用的只读连接:admin 看板的全历史 GROUP BY
/// 可能持锁数百毫秒,若与数据面共用一把锁,管理员开个页面就能把客户请求
/// 的鉴权和计费落库排队(对抗审查 Architect#1)。WAL 下读写跨连接并发,
/// 拆开后统计再慢也只占自己的锁。
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
    stats_conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// 打开(或创建)数据库,启用 WAL,建表。
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let conn = Connection::open(path.as_ref())?;
        // busy_timeout 必须最先设:router/worker 同时启动会并发执行 WAL 切换与建表,
        // 没有它任何抢锁直接 "database is locked" 即死(而非等待重试)。
        conn.pragma_update(None, "busy_timeout", 5000)?;
        // WAL:多进程并发读友好;NORMAL 同步在 WAL 下安全且快。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        Self::setup_schema(&conn)?;
        // 统计读连接(主连接建完表后再开);query_only 双保险防误写。
        let stats = Connection::open(path.as_ref())?;
        stats.pragma_update(None, "busy_timeout", 5000)?;
        stats.pragma_update(None, "query_only", true)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
            stats_conn: Arc::new(Mutex::new(stats)),
        })
    }

    /// 内存库(测试用)。独立内存连接是另一个库,统计连接直接共享主连接。
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::setup_schema(&conn)?;
        let conn = Arc::new(Mutex::new(conn));
        Ok(Self {
            stats_conn: conn.clone(),
            conn,
        })
    }

    /// 建表 + 增量列迁移(新旧库统一走这里)。
    fn setup_schema(conn: &Connection) -> anyhow::Result<()> {
        conn.execute_batch(SCHEMA)?;
        // api_keys 增量列(SQLite 不支持 ALTER COLUMN,只能 ADD COLUMN;
        // CREATE TABLE IF NOT EXISTS 不会给已存在的表补列,故逐列探测)。
        Self::ensure_column(conn, "api_keys", "group_name", "group_name TEXT NOT NULL DEFAULT ''")?;
        Self::ensure_column(conn, "api_keys", "quota_tokens", "quota_tokens INTEGER")?;
        Self::ensure_column(conn, "api_keys", "used_tokens", "used_tokens INTEGER NOT NULL DEFAULT 0")?;
        // request_logs 真实命中/原生计费列(存量库热升级:旧表无此列则补)。
        Self::ensure_column(
            conn,
            "request_logs",
            "real_cache_read_tokens",
            "real_cache_read_tokens INTEGER NOT NULL DEFAULT 0",
        )?;
        Self::ensure_column(
            conn,
            "request_logs",
            "metering_credit",
            "metering_credit REAL NOT NULL DEFAULT 0",
        )?;
        // request_logs 模型回复列(存量库热升级:旧表无此列则补,历史行回填为空)。
        Self::ensure_column(
            conn,
            "request_logs",
            "response_payload",
            "response_payload TEXT NOT NULL DEFAULT ''",
        )?;
        // usage_records 成本看板列(存量库热升级:旧表无此列则补,历史行回填为 0)。
        Self::ensure_column(
            conn,
            "usage_records",
            "real_cache_read_tokens",
            "real_cache_read_tokens INTEGER NOT NULL DEFAULT 0",
        )?;
        Self::ensure_column(
            conn,
            "usage_records",
            "metering_credit",
            "metering_credit REAL NOT NULL DEFAULT 0",
        )?;
        // 账号累计成功/失败请求计数(监控用,非计费)。additive:老库升级即补 0,现有号从 0 起算。
        Self::ensure_column(
            conn,
            "accounts",
            "success_count",
            "success_count INTEGER NOT NULL DEFAULT 0",
        )?;
        Self::ensure_column(
            conn,
            "accounts",
            "failure_count",
            "failure_count INTEGER NOT NULL DEFAULT 0",
        )?;
        // 补货订单的货源列(存量库热升级)。历史行回填成空串而不是 'drop':
        // 空串诚实地表示「这单下的时候还没有多供应商概念」,回填一个具体值会让
        // 「drop 一共花了多少」这类统计把历史混进来,而那时的口径与现在并不相同。
        Self::ensure_column(
            conn,
            "restock_orders",
            "supplier",
            "supplier TEXT NOT NULL DEFAULT ''",
        )?;
        Self::ensure_column(conn, "restock_orders", "shelf", "shelf TEXT NOT NULL DEFAULT ''")?;
        // 订单级的 Kiro 服务区。一单里的 key 来自同一个货架,共享同一个区,
        // 所以它是订单属性而不是 key 属性(见 `engine.rs` 落库处的注释)。
        Self::ensure_column(conn, "restock_orders", "region", "region TEXT NOT NULL DEFAULT ''")?;
        Self::backfill_account_groups(conn)?;
        Ok(())
    }

    /// 把「账号 → 归属组」回填成成员边,让新模型的起点与旧行为**逐条等价**。
    ///
    /// 每个号在自己原来的组里、组内优先级还是它原来的 `extra.priority`(缺省 100,
    /// 与 `CredentialState` 的历史默认一致)→ 迁移后 G0 的选号序列与迁移前完全相同。
    ///
    /// `INSERT OR IGNORE` + 复合主键使其**幂等**且**只补不覆盖**:运维后来手工调过的
    /// 组内优先级不会在下次启动时被账号上的旧值冲掉。跨进程并发 open 也安全。
    fn backfill_account_groups(conn: &Connection) -> anyhow::Result<()> {
        let n = conn.execute(
            "INSERT OR IGNORE INTO account_groups (account_id, group_name, priority) \
             SELECT account_id, group_name, \
                    COALESCE(CAST(json_extract(extra, '$.priority') AS INTEGER), 100) \
             FROM accounts WHERE group_name <> ''",
            [],
        )?;
        if n > 0 {
            tracing::info!(edges = n, "account_groups 回填:按账号归属组建立成员边");
        }
        Ok(())
    }

    /// 列不存在则 ADD COLUMN(表名/DDL 为代码常量,无注入面)。
    ///
    /// **跨进程幂等**(审查 Skeptic#1):router 与多个 worker 进程升级后会并发 `open` 同一库,
    /// `pragma_table_info` 探测与 `ALTER TABLE` 之间非原子——两进程都可能先看到列缺失,一个 ADD
    /// 成功、另一个等锁后再 ADD 撞 `duplicate column name`。把该错误视为成功(列已就位即达成目的),
    /// 否则 `open` 失败会让 worker 不落库 / router 控制面降级。
    fn ensure_column(conn: &Connection, table: &str, col: &str, ddl: &str) -> anyhow::Result<()> {
        let exists: bool = conn
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
            ))?
            .exists([col])?;
        if !exists {
            match conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"), []) {
                Ok(_) => {}
                // 另一进程抢先 ADD 了同名列(竞态)→ 目的已达成,当成功。
                Err(rusqlite::Error::SqliteFailure(_, Some(msg)))
                    if msg.contains("duplicate column name") => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }

    /// 新增一个客户端 API key(admin / 播种用)。
    pub fn add_api_key(&self, key: &str, label: Option<&str>) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT OR IGNORE INTO api_keys (key, label) VALUES (?1, ?2)",
            (key, label),
        )?;
        Ok(())
    }

    // ───────── API key CRUD(admin 管理页) ─────────

    const API_KEY_COLS: &'static str =
        "key, label, disabled, group_name, quota_tokens, used_tokens, created_at";

    /// 列出全部客户端 API key(created_at 倒序;同秒按 rowid 倒序兜底,
    /// 保证"新建项在顶部"不被秒级精度破坏)。
    pub fn list_api_keys(&self) -> anyhow::Result<Vec<ApiKeyRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM api_keys ORDER BY created_at DESC, rowid DESC",
            Self::API_KEY_COLS
        ))?;
        let rows = stmt
            .query_map([], Self::row_to_api_key)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 读取单个 key 的元数据(POST/PATCH 响应体用)。
    pub fn get_api_key(&self, key: &str) -> anyhow::Result<Option<ApiKeyRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {} FROM api_keys WHERE key = ?1",
            Self::API_KEY_COLS
        ))?;
        match stmt.query_row([key], Self::row_to_api_key) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn row_to_api_key(r: &rusqlite::Row<'_>) -> rusqlite::Result<ApiKeyRow> {
        Ok(ApiKeyRow {
            key: r.get(0)?,
            label: r.get(1)?,
            disabled: r.get::<_, i64>(2)? != 0,
            group_name: r.get(3)?,
            quota_tokens: r.get(4)?,
            used_tokens: r.get(5)?,
            created_at: r.get(6)?,
        })
    }

    /// 严格新增:已存在返回 `false`(admin 创建需要感知冲突,区别于播种用的
    /// [`Self::add_api_key`] 静默忽略)。
    pub fn create_api_key(
        &self,
        key: &str,
        label: Option<&str>,
        group: Option<&str>,
    ) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO api_keys (key, label, group_name) VALUES (?1, ?2, ?3)",
            (key, label, group.unwrap_or("")),
        )?;
        Ok(changed == 1)
    }

    /// 部分更新(见 [`ApiKeyPatch`] 各字段语义);返回 `false` = key 不存在。
    /// 全字段缺省时只做存在性检查(no-op)。
    pub fn update_api_key(&self, key: &str, patch: &ApiKeyPatch) -> anyhow::Result<bool> {
        // 动态拼 SET 子句(列名是代码常量,值全部参数化,无注入面)。
        let mut sets: Vec<&str> = Vec::new();
        let mut params: Vec<Value> = Vec::new();
        if let Some(l) = &patch.label {
            sets.push("label = ?");
            // 空串=清空 → 统一落 NULL,与创建路径一致(避免 ''/NULL 双态)。
            params.push(if l.is_empty() {
                Value::Null
            } else {
                Value::Text(l.clone())
            });
        }
        if let Some(d) = patch.disabled {
            sets.push("disabled = ?");
            params.push(Value::Integer(d as i64));
        }
        if let Some(g) = &patch.group_name {
            sets.push("group_name = ?");
            params.push(Value::Text(g.clone()));
        }
        if let Some(q) = patch.quota_tokens {
            sets.push("quota_tokens = ?");
            // <= 0 视为清除限额(NULL = 不限)。
            params.push(if q > 0 { Value::Integer(q) } else { Value::Null });
        }
        if patch.reset_used {
            sets.push("used_tokens = 0");
        }
        let conn = self.conn.lock();
        if sets.is_empty() {
            // no-op:只回答 key 是否存在。
            let exists: bool = conn
                .prepare_cached("SELECT 1 FROM api_keys WHERE key = ?1")?
                .exists([key])?;
            return Ok(exists);
        }
        params.push(Value::Text(key.to_string()));
        let sql = format!("UPDATE api_keys SET {} WHERE key = ?", sets.join(", "));
        let changed = conn.execute(&sql, rusqlite::params_from_iter(params))?;
        Ok(changed == 1)
    }

    /// 删除 key;返回 `false` = key 不存在。usage_records 中的历史归属不动。
    pub fn delete_api_key(&self, key: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute("DELETE FROM api_keys WHERE key = ?1", [key])?;
        Ok(changed == 1)
    }

    // ───────── 分组 CRUD(admin 管理页;账号组 + key 归类) ─────────

    /// 列出全部分组(含组内账号数/绑定 key 数,创建序)。
    pub fn list_groups(&self) -> anyhow::Result<Vec<GroupRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT g.name, g.color, g.note, g.created_at, \
             (SELECT COUNT(*) FROM accounts a WHERE a.group_name = g.name), \
             (SELECT COUNT(*) FROM api_keys k WHERE k.group_name = g.name), \
             (SELECT COUNT(*) FROM account_groups m WHERE m.group_name = g.name) \
             FROM groups g ORDER BY g.created_at ASC, g.name ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(GroupRow {
                    name: r.get(0)?,
                    color: r.get(1)?,
                    note: r.get(2)?,
                    created_at: r.get(3)?,
                    account_count: r.get::<_, i64>(4)? as u64,
                    key_count: r.get::<_, i64>(5)? as u64,
                    member_count: r.get::<_, i64>(6)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 严格新增分组;已存在返回 `false`。新组是空的,成员由 `upsert_membership` /
    /// `bulk_add_members` 单独加 —— 没有成员的组服务不了任何请求(503)。
    pub fn create_group(&self, name: &str, color: &str, note: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO groups (name, color, note) VALUES (?1, ?2, ?3)",
            (name, color, note),
        )?;
        Ok(changed == 1)
    }

    /// 部分更新 color/note(`None` 不动);`false` = 组不存在。
    pub fn update_group(
        &self,
        name: &str,
        color: Option<&str>,
        note: Option<&str>,
    ) -> anyhow::Result<bool> {
        let mut sets: Vec<&str> = Vec::new();
        let mut params: Vec<Value> = Vec::new();
        if let Some(c) = color {
            sets.push("color = ?");
            params.push(Value::Text(c.to_string()));
        }
        if let Some(n) = note {
            sets.push("note = ?");
            params.push(Value::Text(n.to_string()));
        }
        let conn = self.conn.lock();
        if sets.is_empty() {
            let exists: bool = conn
                .prepare_cached("SELECT 1 FROM groups WHERE name = ?1")?
                .exists([name])?;
            return Ok(exists);
        }
        params.push(Value::Text(name.to_string()));
        let sql = format!("UPDATE groups SET {} WHERE name = ?", sets.join(", "));
        let changed = conn.execute(&sql, rusqlite::params_from_iter(params))?;
        Ok(changed == 1)
    }

    /// 删除分组,并把引用它的账号/key 的 group_name 清为 ''(未分组);
    /// 四条语句包成事务(含依赖检查),避免删组成功但引用残留、或检查与删除之间被插队。
    ///
    /// ## 仍有 key 绑定时必须拒绝(**安全护栏,别当成多余的校验删掉**)
    /// `group_name = ''` 会被 router 的 `resolve_group` 回落到 `default_group`(= 主组 G0)。
    /// 因此裸删一个还有客户在用的组,等于把这些 key **静默提权**成主组的不受限访问,
    /// 且无任何告警 —— 低价客户当场变成能用全部主力号。
    ///
    /// ## 仍是 owner 时也必须拒绝(**对抗审查 Architect#1**)
    /// 组名同时是 `accounts.group_name` 的取值(归属)。删组若顺手把归属清空,那些账号
    /// 就成了没有任何 worker 加载的孤儿 —— 而**别的组可能正借用它们**,那个组会当场
    /// 全量 503,且删的人完全看不出因果。删权限对象不得改动物理归属:先把账号迁到别的
    /// owner,再删这个组。
    ///
    /// 下线一个组的正确姿势是 [`Self::clear_group_members`](该组随即 503),或先迁走 key 再删。
    pub fn delete_group(&self, name: &str) -> anyhow::Result<DeleteGroupOutcome> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let exists: bool =
            tx.prepare_cached("SELECT 1 FROM groups WHERE name = ?1")?.exists([name])?;
        if !exists {
            return Ok(DeleteGroupOutcome::NotFound);
        }
        let bound: i64 = tx
            .prepare_cached("SELECT COUNT(*) FROM api_keys WHERE group_name = ?1")?
            .query_row([name], |r| r.get(0))?;
        if bound > 0 {
            return Ok(DeleteGroupOutcome::HasKeys(bound as u64));
        }
        let owned: i64 = tx
            .prepare_cached("SELECT COUNT(*) FROM accounts WHERE group_name = ?1")?
            .query_row([name], |r| r.get(0))?;
        if owned > 0 {
            return Ok(DeleteGroupOutcome::IsOwner(owned as u64));
        }
        tx.execute("DELETE FROM groups WHERE name = ?1", [name])?;
        tx.execute("DELETE FROM account_groups WHERE group_name = ?1", [name])?;
        tx.commit()?;
        Ok(DeleteGroupOutcome::Deleted)
    }

    /// 清空一个组的全部成员边,返回删除的边数。**这是下线一个组的正确姿势**:
    /// 组还在、key 还绑着,但选不出任何账号 → 该组立即 503,客户不会被静默提权到主组。
    ///
    /// (对抗审查 Minimalist#1:错误信息让运维"清空本组成员",就得有一步能做完的动作,
    /// 而不是 GET 一遍再发 N 次 DELETE、中途失败留下半下线状态。)
    pub fn clear_group_members(&self, name: &str) -> anyhow::Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM account_groups WHERE group_name = ?1", [name])?)
    }

    // ───────── 账号 CRUD(配置态;运行态见 worker /status) ─────────

    const ACCOUNT_COLS: &'static str =
        "account_id, group_name, provider, max_concurrency, disabled, extra, created_at, \
         success_count, failure_count";

    fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRow> {
        Ok(AccountRow {
            account_id: r.get(0)?,
            group_name: r.get(1)?,
            provider: r.get(2)?,
            max_concurrency: r.get(3)?,
            disabled: r.get::<_, i64>(4)? != 0,
            extra: r.get(5)?,
            created_at: r.get(6)?,
            success_count: r.get(7)?,
            failure_count: r.get(8)?,
        })
    }

    /// 列出全部账号(组名升序、组内 account_id 升序)。extra 含敏感凭据,
    /// admin 端点返回前必须脱敏。
    pub fn list_accounts(&self) -> anyhow::Result<Vec<AccountRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&format!(
            "SELECT {} FROM accounts ORDER BY group_name ASC, account_id ASC",
            Self::ACCOUNT_COLS
        ))?;
        let rows = stmt
            .query_map([], Self::row_to_account)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 读取单个账号。
    pub fn get_account(&self, account_id: &str) -> anyhow::Result<Option<AccountRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {} FROM accounts WHERE account_id = ?1",
            Self::ACCOUNT_COLS
        ))?;
        match stmt.query_row([account_id], Self::row_to_account) {
            Ok(row) => Ok(Some(row)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// 严格新增账号;已存在返回 `false`。`extra_json` 须是合法 JSON 对象文本
    /// (调用方校验;此处只管落库)。
    ///
    /// 同时给归属组建一条成员边(组内优先级取 `extra.priority`,缺省 100)。
    /// **不建边的新号对所有客户都不可见**——账号在 `accounts` 里、却不在任何组的成员
    /// 集里,导入完看着一切正常但永远不会被选中,是最难查的一类"配了没生效"。
    pub fn create_account(
        &self,
        account_id: &str,
        group_name: &str,
        provider: &str,
        max_concurrency: i64,
        extra_json: &str,
    ) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO accounts \
             (account_id, group_name, provider, max_concurrency, extra) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (account_id, group_name, provider, max_concurrency.max(1), extra_json),
        )?;
        if changed == 1 && !group_name.is_empty() {
            conn.execute(
                "INSERT OR IGNORE INTO account_groups (account_id, group_name, priority) \
                 VALUES (?1, ?2, COALESCE(CAST(json_extract(?3, '$.priority') AS INTEGER), 100))",
                (account_id, group_name, extra_json),
            )?;
        }
        Ok(changed == 1)
    }

    /// 部分更新(见 [`AccountPatch`]);`false` = 账号不存在。
    pub fn update_account(
        &self,
        account_id: &str,
        patch: &AccountPatch,
    ) -> anyhow::Result<UpdateAccountOutcome> {
        let mut sets: Vec<&str> = Vec::new();
        let mut params: Vec<Value> = Vec::new();
        if let Some(g) = &patch.group_name {
            sets.push("group_name = ?");
            params.push(Value::Text(g.clone()));
        }
        if let Some(m) = patch.max_concurrency {
            sets.push("max_concurrency = ?");
            params.push(Value::Integer(m.max(1)));
        }
        if let Some(d) = patch.disabled {
            sets.push("disabled = ?");
            params.push(Value::Integer(d as i64));
        }
        if let Some(e) = &patch.extra {
            sets.push("extra = ?");
            params.push(Value::Text(e.clone()));
        }
        let mut conn = self.conn.lock();
        if sets.is_empty() {
            let exists: bool = conn
                .prepare_cached("SELECT 1 FROM accounts WHERE account_id = ?1")?
                .exists([account_id])?;
            return Ok(if exists {
                UpdateAccountOutcome::Ok
            } else {
                UpdateAccountOutcome::NotFound
            });
        }
        let tx = conn.transaction()?;
        // 换归属前先看:这个号参与的每个组里,**别的**成员归属谁?有一个对不上就整单拒绝。
        // 校验与写入必须同一事务 —— 否则并发的建边请求会插在检查与 UPDATE 之间。
        if let Some(incoming) = &patch.group_name {
            let conflict: Option<(String, String)> = tx
                .prepare_cached(
                    "SELECT m.group_name, a.group_name FROM account_groups m \
                     JOIN account_groups other ON other.group_name = m.group_name \
                     JOIN accounts a ON a.account_id = other.account_id \
                     WHERE m.account_id = ?1 AND other.account_id <> ?1 \
                     AND a.group_name <> ?2 ORDER BY m.group_name LIMIT 1",
                )?
                .query_row(rusqlite::params![account_id, incoming], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })
                .optional()?;
            if let Some((group, existing)) = conflict {
                return Ok(UpdateAccountOutcome::CrossOwner {
                    group,
                    existing,
                    incoming: incoming.clone(),
                });
            }
        }
        params.push(Value::Text(account_id.to_string()));
        let sql = format!("UPDATE accounts SET {} WHERE account_id = ?", sets.join(", "));
        let changed = tx.execute(&sql, rusqlite::params_from_iter(params))?;
        tx.commit()?;
        Ok(if changed == 1 { UpdateAccountOutcome::Ok } else { UpdateAccountOutcome::NotFound })
    }

    /// 删除账号;`false` = 不存在。usage_records 历史归属不动;
    /// suspend 生命周期行同事务删除(否则重导同 id 会继承旧号的退役/冷却状态)。
    pub fn delete_account(&self, account_id: &str) -> anyhow::Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let changed = tx.execute("DELETE FROM accounts WHERE account_id = ?1", [account_id])?;
        tx.execute(
            "DELETE FROM account_lifecycle WHERE account_id = ?1",
            [account_id],
        )?;
        tx.commit()?;
        Ok(changed == 1)
    }

    /// 读全部 suspend 生命周期行(worker 启动水合 + sync 周期对账用;
    /// 行数 = 有过 suspend 的号,规模小)。
    pub fn load_suspend_lifecycles(
        &self,
    ) -> anyhow::Result<std::collections::HashMap<String, gw_core::store::SuspendLifecycle>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT account_id, suspend_streak, reason, retry_at, epoch, revision FROM account_lifecycle",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                gw_core::store::SuspendLifecycle {
                    suspend_streak: r.get::<_, i64>(1)? as u32,
                    reason: r.get(2)?,
                    retry_at: r.get(3)?,
                    epoch: r.get(4)?,
                    revision: r.get(5)?,
                },
            ))
        })?;
        let mut out = std::collections::HashMap::new();
        for r in rows {
            let (id, lc) = r?;
            out.insert(id, lc);
        }
        Ok(out)
    }

    /// 持久化一次 suspend 生命周期状态转换。**全序条件写**:仅当 `(epoch, revision)`
    /// 比库内行**严格更新**才生效——epoch 由人工恢复递增(挡旧世代写),revision
    /// 由 worker 每次转换递增(挡同 epoch 的乱序 detached 写,对抗审查二轮阻断#1)。
    /// 返回 `false` = 竞态落败(调用方放弃本次写入,由 sync 对账收敛),不是错误。
    ///
    /// `set_disabled=true` 时同事务把 `accounts.disabled` 置 1(自动退役)——
    /// 且仅在条件写生效时才置,否则退役会被反写到一个刚被人工恢复的号上。
    ///
    /// INSERT 带 `WHERE EXISTS(accounts)`:删除账号后迟到的 detached 写不得为
    /// 不存在的账号重插孤儿行(对抗审查二轮中#4)。
    pub fn persist_suspend_lifecycle(
        &self,
        account_id: &str,
        lc: &gw_core::store::SuspendLifecycle,
        set_disabled: bool,
    ) -> anyhow::Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let applied = tx.execute(
            "INSERT INTO account_lifecycle (account_id, suspend_streak, reason, retry_at, epoch, revision) \
             SELECT ?1, ?2, ?3, ?4, ?5, ?6 \
             WHERE EXISTS (SELECT 1 FROM accounts WHERE account_id = ?1) \
             ON CONFLICT(account_id) DO UPDATE SET \
             suspend_streak=excluded.suspend_streak, reason=excluded.reason, \
             retry_at=excluded.retry_at, epoch=excluded.epoch, revision=excluded.revision \
             WHERE account_lifecycle.epoch < excluded.epoch \
                OR (account_lifecycle.epoch = excluded.epoch \
                    AND account_lifecycle.revision < excluded.revision)",
            rusqlite::params![
                account_id,
                lc.suspend_streak as i64,
                lc.reason,
                lc.retry_at,
                lc.epoch,
                lc.revision
            ],
        )? == 1;
        if applied && set_disabled {
            tx.execute(
                "UPDATE accounts SET disabled = 1 WHERE account_id = ?1",
                [account_id],
            )?;
        }
        tx.commit()?;
        Ok(applied)
    }

    /// 人工恢复(原子):解除配置停用 + 清 suspend 生命周期内容 + **epoch 递增、
    /// revision 归零**。行不删除(写清零行):worker 的 sync 对账靠「库 (epoch,revision)
    /// 比内存新」发现这次恢复,进而清运行态——即使该号只是运行态冷却
    /// (DB disabled 一直是 0,无配置翻转),恢复也能可靠送达 worker(对抗审查阻断#2)。
    pub fn restore_account(&self, account_id: &str) -> anyhow::Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let changed = tx.execute(
            "UPDATE accounts SET disabled = 0 WHERE account_id = ?1",
            [account_id],
        )?;
        // 账号不存在:整单回滚——不得为不存在的号留下孤儿生命周期行
        // (迟到的恢复/PATCH 打错 id 都不该重插行,对抗审查三轮中#2)。
        if changed == 0 {
            tx.rollback()?;
            return Ok(false);
        }
        tx.execute(
            "INSERT INTO account_lifecycle (account_id, suspend_streak, reason, retry_at, epoch, revision) \
             VALUES (?1, 0, NULL, NULL, 1, 0) \
             ON CONFLICT(account_id) DO UPDATE SET \
             suspend_streak=0, reason=NULL, retry_at=NULL, \
             epoch=account_lifecycle.epoch+1, revision=0",
            [account_id],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// worker 刷新 token 后回写 extra(rolling refresh_token 持久化,
    /// 重启不丢);`false` = 账号不存在。
    pub fn update_account_extra(&self, account_id: &str, extra_json: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "UPDATE accounts SET extra = ?1 WHERE account_id = ?2",
            (extra_json, account_id),
        )?;
        Ok(changed == 1)
    }

    /// **增量**合并 extra:只覆盖 `patch_json` 里出现的键,其余字段保持 DB 现值。
    /// worker 刷新回写用它而非整块替换——刷新只改 token 字段,整块替换会把并发的
    /// admin 修改(priority/region 等)用旧内存快照抹掉(对抗审查 Architect#4)。
    /// 事务内读-合-写,跨进程(router admin 写 vs worker 回写)原子。
    pub fn merge_account_extra(&self, account_id: &str, patch_json: &str) -> anyhow::Result<bool> {
        let patch: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_str(patch_json)?;
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let current: Option<String> = {
            let mut stmt = tx.prepare("SELECT extra FROM accounts WHERE account_id = ?1")?;
            match stmt.query_row([account_id], |r| r.get(0)) {
                Ok(v) => Some(v),
                Err(rusqlite::Error::QueryReturnedNoRows) => None,
                Err(e) => return Err(e.into()),
            }
        };
        let Some(current) = current else {
            return Ok(false);
        };
        let mut merged: std::collections::BTreeMap<String, serde_json::Value> =
            serde_json::from_str(&current).unwrap_or_default();
        merged.extend(patch);
        tx.execute(
            "UPDATE accounts SET extra = ?1 WHERE account_id = ?2",
            (serde_json::to_string(&merged)?, account_id),
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// `settings` 表里模型目录快照(`ListAvailableModels` 结果)的键名。
    /// 与 `'system'` 同表不同键 —— 复用现成的表意味着**零 schema 变更、零迁移**。
    pub const KEY_MODEL_CATALOG: &'static str = "model_catalog";

    /// 读 `settings` 表的任意键。无行 = `None`。
    pub fn get_kv(&self, key: &str) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock();
        match conn.query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
            r.get::<_, String>(0)
        }) {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// 写 `settings` 表的任意键(整值覆盖)。
    pub fn upsert_kv(&self, key: &str, value: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, \
             updated_at = strftime('%s','now')",
            [key, value],
        )?;
        Ok(())
    }

    /// 读系统设置 overlay(单行 key='system')。无行 = `None`(用 YAML 默认)。
    pub fn get_settings(&self) -> anyhow::Result<Option<String>> {
        self.get_kv("system")
    }

    /// 写系统设置 overlay(整段 JSON 覆盖单行 key='system')。
    pub fn upsert_settings(&self, json: &str) -> anyhow::Result<()> {
        self.upsert_kv("system", json)
    }

    // ───────────────────────── 自动补货:租约 ─────────────────────────

    /// 补货 leader 租约在 `settings` 表里的键名。
    pub const KEY_RESTOCK_LEASE: &'static str = "restock_lease";
    /// **花钱临界区**的互斥锁键名。见 [`Self::try_acquire_lock`] 的文档。
    pub const KEY_RESTOCK_PURCHASE_LOCK: &'static str = "restock_purchase_lock";
    /// 积分汇总游标(已消费到的 `usage_records.id`)的键名。
    pub const KEY_RESTOCK_CURSOR: &'static str = "restock_rollup_cursor";
    /// 补货运行时参数(面板可改的那些)的键名,整段 JSON。
    pub const KEY_RESTOCK_PARAMS: &'static str = "restock_params";

    /// 抢占一把带 TTL 的跨进程锁。返回 `true` = 拿到了。
    ///
    /// 一条**条件 UPDATE**:只有「上一任已过期」或「本来就是我」两种情况写得进去
    /// (首次是 INSERT,无冲突)。SQLite 单写者 + `busy_timeout=5000` 保证互斥,
    /// 抢锁时会等待而不是当场 `database is locked`。
    ///
    /// ## 为什么「花钱锁」必须与 leader 租约分开
    ///
    /// 两把锁回答的是**不同的问题**:
    /// - leader 租约([`Self::KEY_RESTOCK_LEASE`])= 「**谁来跑轮询**」,由持有者
    ///   长期持有、每轮续租;
    /// - 花钱锁([`Self::KEY_RESTOCK_PURCHASE_LOCK`])= 「**此刻谁在花钱**」,
    ///   只在「读预算 → 落幂等键 → 发购买请求 → 记账」这段临界区内持有,用完立刻释放。
    ///
    /// 把两者合并过(2026-08-05 之前的形态就是只有前者),后果是
    /// `POST /restock/buy-now` **完全不受任何互斥保护** ——
    /// 它不抢租约,直接 `run_once(true)`。手动点两次、或手动与后台轮询撞上,
    /// 两个执行会先读到**同一个** `spent`,再各自插入订单并真实扣款。
    ///
    /// 反过来,若让 buy-now 去抢 leader 租约也不行:后台循环长期持有它,
    /// 手动购买会永远拿不到、变成一个点了没反应的按钮。
    pub fn try_acquire_lock(&self, key: &str, holder: &str, ttl_secs: i64) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let now: i64 =
            conn.query_row("SELECT CAST(strftime('%s','now') AS INTEGER)", [], |r| r.get(0))?;
        let value = serde_json::json!({ "holder": holder, "expires_at": now + ttl_secs.max(1) })
            .to_string();
        let n = conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                            updated_at = strftime('%s','now')
             WHERE CAST(json_extract(settings.value, '$.expires_at') AS INTEGER) < ?3
                OR json_extract(settings.value, '$.holder') = ?4",
            rusqlite::params![key, value, now, holder],
        )?;
        Ok(n == 1)
    }

    /// 释放锁。**只有持有者本人能释放**,避免误伤接任者。
    pub fn release_lock(&self, key: &str, holder: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM settings WHERE key = ?1 AND json_extract(value, '$.holder') = ?2",
            rusqlite::params![key, holder],
        )?;
        Ok(())
    }

    /// 我**此刻**是否仍持有这把锁(未过期且 holder 是我)。
    ///
    /// 花钱之前必须再问一次:租约 TTL 最短只有 30s(`poll_interval` 下限 10s × 3),
    /// 而单轮决策的外层超时是 120s —— 中间隔着健康检查、报价、购买、上号。
    /// 不复验的话,第 31 秒另一个 router 已经接管并下了单,而本进程恢复后还会照下不误。
    pub fn holds_lock(&self, key: &str, holder: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let ok: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM settings WHERE key = ?1
                   AND json_extract(value, '$.holder') = ?2
                   AND CAST(json_extract(value, '$.expires_at') AS INTEGER) >= \
                       CAST(strftime('%s','now') AS INTEGER)",
                rusqlite::params![key, holder],
                |r| r.get(0),
            )
            .optional()?;
        Ok(ok.is_some())
    }

    /// 抢占或续租补货 leader 租约。返回 `true` = 本进程当选,可以执行本轮补货。
    ///
    /// **为什么必须有这个**:生产上有**两个以上** `--mode router` 进程(kiro 一个、
    /// dario 一个,开了 exp 栈还有第三个),它们共用同一个 control.db。把补货循环直接
    /// 挂在 router 角色上会让每个进程各买各的 —— 直接重复扣款。
    /// `README.md` 里"Router 单实例"的说法是单通道时代的过时描述。
    ///
    /// 实现是一条**条件 UPDATE**:只有「上一任已过期」或「本来就是我」两种情况写得进去
    /// (首次是 INSERT,无冲突)。SQLite 单写者 + `busy_timeout=5000` 保证互斥,
    /// 抢锁时会等待而不是当场 `database is locked`。
    ///
    /// 租约**故意不做续期保证**:持有者每轮都要重新调用本方法,进程被 SIGKILL
    /// (docker stop 10s 后就是)时租约自然过期,由别的 router 接手。
    pub fn try_acquire_restock_lease(&self, holder: &str, ttl_secs: i64) -> anyhow::Result<bool> {
        self.try_acquire_lock(Self::KEY_RESTOCK_LEASE, holder, ttl_secs)
    }

    /// 主动让出租约(优雅停机用)。只有持有者本人能让出,避免误伤接任者。
    pub fn release_restock_lease(&self, holder: &str) -> anyhow::Result<()> {
        self.release_lock(Self::KEY_RESTOCK_LEASE, holder)
    }

    /// 当前租约持有者(面板展示用;过期的返回 `None`)。
    pub fn restock_lease_holder(&self) -> anyhow::Result<Option<String>> {
        let conn = self.conn.lock();
        let row: Option<(String, i64)> = conn
            .query_row(
                "SELECT json_extract(value, '$.holder'),
                        CAST(json_extract(value, '$.expires_at') AS INTEGER)
                   FROM settings WHERE key = ?1",
                [Self::KEY_RESTOCK_LEASE],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let now: i64 = conn.query_row("SELECT CAST(strftime('%s','now') AS INTEGER)", [], |r| r.get(0))?;
        Ok(row.filter(|(_, exp)| *exp > now).map(|(h, _)| h))
    }

    // ───────────────────────── 自动补货:订单 ─────────────────────────

    /// 落库幂等键。**必须在发出购买请求之前调用** —— 进程崩在请求途中时,
    /// 重启后靠这行才知道那个 `client_order_id` 是什么,用原 id 重放才能问出真实结果;
    /// 否则重试就是重复扣款。
    /// `supplier` / `shelf` / `region` 必须**和幂等键一起**落库,而不是等买成了再补:
    /// 订单停在 `pending` 时,对账要靠 `supplier` 才知道该去问哪一家。少了它,
    /// 一张在途订单在多供应商下就是**无主的** —— 谁都不敢重放,钱永远找不回来。
    pub fn restock_create_order(
        &self,
        order_id: &str,
        count: i64,
        max_total_cny: f64,
        supplier: &str,
        shelf: &str,
        region: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO restock_orders
                 (client_order_id, count, max_total_cny, status, supplier, shelf, region)
             VALUES (?1, ?2, ?3, 'pending', ?4, ?5, ?6)",
            rusqlite::params![order_id, count, max_total_cny, supplier, shelf, region],
        )?;
        Ok(())
    }

    /// key 到手后立刻落库,记**实际扣款**并返回该值。
    ///
    /// 记实扣而不是按报价估算:报价字段是 USD、扣款走 CNY,用报价推算会让日预算阀失真。
    ///
    /// `debited` 是**供应商自报的单笔扣款**(kiroapp 的 `total_debit` 就是权威值);
    /// 给不出时传 `None`,回落成余额差(drop 走这条)。两者都拿不到就是 0,
    /// 调用方需要据此告警 —— 账不平比少买一个号严重。
    pub fn restock_mark_purchased(
        &self,
        order_id: &str,
        keys: &[String],
        debited: Option<f64>,
        balance_before: Option<f64>,
        balance_after: f64,
    ) -> anyhow::Result<f64> {
        let spent = debited
            .filter(|d| *d > 0.0)
            .or_else(|| balance_before.map(|b| (b - balance_after).max(0.0)))
            .unwrap_or(0.0);
        let keys_json = serde_json::to_string(keys)?;
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE restock_orders
                SET status='purchased', keys_json=?2, spent_cny=?3,
                    balance_before=?4, balance_after=?5,
                    updated_at=strftime('%s','now')
              WHERE client_order_id=?1",
            rusqlite::params![order_id, keys_json, spent, balance_before, balance_after],
        )?;
        Ok(spent)
    }

    /// 改订单状态(附带错误说明)。`status` 见 [`RestockOrder`] 的状态机注释。
    pub fn restock_mark_status(
        &self,
        order_id: &str,
        status: &str,
        error: &str,
    ) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE restock_orders SET status=?2, error=?3, updated_at=strftime('%s','now')
              WHERE client_order_id=?1",
            rusqlite::params![order_id, status, error],
        )?;
        Ok(())
    }

    /// 停在 `pending` 的订单:请求可能在途也可能已成功,用原幂等键重放确认。
    ///
    /// `min_age_secs` 是**必需的安全边距,不是优化**。对账现在每轮都跑(原先只在进程
    /// 当选后跑一次,于是运行期产生的 pending 永远等不到确认),而「刚落库、请求正在飞」
    /// 的订单本身就是 `pending`。不设年龄下限的话,另一个 router 会把**别人正在途中的
    /// 那一单**拿去重放 —— 对方若还没记录这个 id,就是第二次真实扣款。
    ///
    /// 取值必须**大于单轮外层超时**(120s),这样任何一个还可能在途的订单都不会被碰。
    pub fn restock_pending_orders(&self, min_age_secs: i64) -> anyhow::Result<Vec<RestockOrder>> {
        self.restock_orders_where(
            &format!(
                "status = 'pending' AND created_at <= strftime('%s','now') - {}",
                min_age_secs.max(0)
            ),
            100,
        )
    }

    /// 在途订单数(面板展示)。与孤儿订单(`purchased` = 钱花了号没进系统)分开数:
    /// 两者要人做的事完全不同 —— 在途的等对账,孤儿的要人工上号。
    pub fn restock_pending_count(&self) -> anyhow::Result<i64> {
        let conn = self.conn.lock();
        Ok(conn.query_row(
            "SELECT COUNT(*) FROM restock_orders WHERE status = 'pending'",
            [],
            |r| r.get(0),
        )?)
    }

    /// **孤儿订单:买到了 key 却没能上号。** 钱已经花了,必须人工处理。
    pub fn restock_orphan_orders(&self) -> anyhow::Result<Vec<RestockOrder>> {
        self.restock_orders_where("status = 'purchased'", 100)
    }

    /// 最近的订单(倒序)。
    pub fn restock_recent_orders(&self, limit: i64) -> anyhow::Result<Vec<RestockOrder>> {
        self.restock_orders_where("1=1", limit)
    }

    fn restock_orders_where(&self, cond: &str, limit: i64) -> anyhow::Result<Vec<RestockOrder>> {
        let sql = format!(
            "SELECT client_order_id, count, status, keys_json, spent_cny, error, created_at,
                    supplier, shelf, region, max_total_cny
               FROM restock_orders WHERE {cond} ORDER BY created_at DESC LIMIT ?1"
        );
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([limit.max(1)], |r| {
                let keys_json: String = r.get(3)?;
                Ok(RestockOrder {
                    client_order_id: r.get(0)?,
                    count: r.get(1)?,
                    status: r.get(2)?,
                    keys: serde_json::from_str(&keys_json).unwrap_or_default(),
                    spent_cny: r.get(4)?,
                    error: r.get(5)?,
                    created_at: r.get(6)?,
                    supplier: r.get(7)?,
                    shelf: r.get(8)?,
                    region: r.get(9)?,
                    max_total_cny: r.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 某一家在 `since_ts` 以来的花费。用于每家独立的日上限。
    ///
    /// 口径与 [`Self::restock_spent_since`] **必须一致**(`pending` 按限价计入最坏情况),
    /// 否则会出现「全局账说花了 ¥100、单家账加起来只有 ¥60」这种对不上的面板。
    ///
    /// 历史订单的 `supplier` 是空串,不会被任何一家匹配到 —— 这是有意的:
    /// 它们下单时还没有多供应商概念,归给谁都是编的。
    pub fn restock_spent_since_by_supplier(
        &self,
        since_ts: i64,
        supplier: &str,
    ) -> anyhow::Result<f64> {
        let conn = self.conn.lock();
        Ok(conn.query_row(
            "SELECT COALESCE(SUM(CASE
                        WHEN status IN ('purchased','imported') THEN spent_cny
                        WHEN status = 'pending'                 THEN max_total_cny
                        ELSE 0 END), 0)
               FROM restock_orders
              WHERE created_at >= ?1 AND supplier = ?2",
            rusqlite::params![since_ts, supplier],
            |r| r.get(0),
        )?)
    }

    /// `since_ts` 以来的花费与成功购买数(日预算阀读它)。
    ///
    /// 直接对订单求和而不另设计数器 —— 计数器与订单不一致这类对账问题不值得引入。
    ///
    /// ## `pending` 必须按**最坏情况**计入花费
    ///
    /// `pending` 的语义是「幂等键已落库,请求可能在途、也可能已经成交了但我们没看到」。
    /// 原先只统计 `purchased`/`imported`,于是:单轮 120s 外层超时把 `run_once` 掐断在
    /// 购买途中 → 订单停在 `pending` → **下一轮把它当成没花过钱** → 用一个新的
    /// `client_order_id` 再买一次。幂等键防得住「同一个 id 重放」,防不住「换个 id 再来」。
    ///
    /// 所以这里用 `max_total_cny`(下单时的限价,即这单可能花掉的上限)把它算进去。
    /// 方向必须是 fail-closed:宁可高估当日花费、少买一个号,也不能低估到超预算。
    /// 对账把它落定成 `purchased`(记真实扣款)或 `failed`(记 0)之后,这个高估自动消失。
    ///
    /// 返回的**计数**仍只数真正买成的单 —— 面板上的「今日已补 N 个」不该把在途的算进去。
    pub fn restock_spent_since(&self, since_ts: i64) -> anyhow::Result<(f64, i64)> {
        let conn = self.conn.lock();
        let r = conn.query_row(
            "SELECT COALESCE(SUM(CASE
                        WHEN status IN ('purchased','imported') THEN spent_cny
                        WHEN status = 'pending'                 THEN max_total_cny
                        ELSE 0 END), 0),
                    COUNT(CASE WHEN status IN ('purchased','imported') THEN 1 END)
               FROM restock_orders
              WHERE created_at >= ?1",
            [since_ts],
            |r| Ok((r.get::<_, f64>(0)?, r.get::<_, i64>(1)?)),
        )?;
        Ok(r)
    }

    // ───────────────────────── 自动补货:自购号 ─────────────────────────

    /// 登记本服务买入并上号成功的账号。**回收只动这张表里的号**。
    pub fn restock_record_owned(&self, order_id: &str, account_ids: &[String]) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        for aid in account_ids {
            tx.execute(
                "INSERT OR IGNORE INTO restock_owned (account_id, client_order_id) VALUES (?1, ?2)",
                rusqlite::params![aid, order_id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 尚未回收的自购号 `(account_id, created_at)`。
    pub fn restock_owned_alive(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare("SELECT account_id, created_at FROM restock_owned WHERE reclaimed_at IS NULL")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 标记某个自购号已回收(删号之后调用)。
    pub fn restock_mark_reclaimed(&self, account_id: &str) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE restock_owned SET reclaimed_at = CAST(strftime('%s','now') AS INTEGER) WHERE account_id = ?1",
            [account_id],
        )?;
        Ok(())
    }

    /// 某笔订单买到的账号 id(面板算单号成本用)。
    pub fn restock_accounts_of_order(&self, order_id: &str) -> anyhow::Result<Vec<String>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT account_id FROM restock_owned WHERE client_order_id = ?1")?;
        let rows = stmt
            .query_map([order_id], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ───────────────────────── 自动补货:决策流水 ─────────────────────────

    /// 记一条决策。这是"为什么没补 / 为什么补了"的唯一可查记录。
    pub fn restock_log_decision(&self, d: &RestockDecision) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO restock_decisions
                (ts, action, reason, healthy, stock, price_usd, balance_cny, detail)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8)",
            rusqlite::params![
                d.ts, d.action, d.reason, d.healthy, d.stock, d.price_usd, d.balance_cny, d.detail
            ],
        )?;
        Ok(())
    }

    /// 最近 N 条决策(倒序)。
    pub fn restock_recent_decisions(&self, limit: i64) -> anyhow::Result<Vec<RestockDecision>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT ts, action, reason, healthy, stock, price_usd, balance_cny, detail
               FROM restock_decisions ORDER BY ts DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([limit.max(1)], |r| {
                Ok(RestockDecision {
                    ts: r.get(0)?,
                    action: r.get(1)?,
                    reason: r.get(2)?,
                    healthy: r.get(3)?,
                    stock: r.get(4)?,
                    price_usd: r.get(5)?,
                    balance_cny: r.get(6)?,
                    detail: r.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 清理过期决策流水(保留 `keep_days` 天)。
    pub fn restock_prune_decisions(&self, keep_days: i64) -> anyhow::Result<()> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM restock_decisions
              WHERE ts < CAST(strftime('%s','now') AS INTEGER) - ?1 * 86400",
            [keep_days.max(1)],
        )?;
        Ok(())
    }

    /// 全部 `ksk_`(API Key 凭据)账号的 id。
    ///
    /// 用**凭据字段**判定而不是 id 前缀:人工上的号可能叫别的名字(邮箱等),
    /// 只认前缀会漏。前缀只在「号已被删、日志还在」时作兜底(见 rollup 的 LEFT JOIN)。
    pub fn restock_ksk_account_ids(&self) -> anyhow::Result<Vec<String>> {
        Ok(self.restock_ksk_accounts()?.into_iter().map(|(id, _)| id).collect())
    }

    /// ksk_ 号的 `(account_id, created_at)`。判活要用建号时刻给新号宽限期 ——
    /// 刚上号还没跑过任何请求,不给宽限就会被自己判成僵尸,于是不停买。
    pub fn restock_ksk_accounts(&self) -> anyhow::Result<Vec<(String, i64)>> {
        let conn = self.stats_conn.lock();
        let mut stmt = conn.prepare(
            "SELECT account_id, created_at FROM accounts
              WHERE json_extract(extra, '$.kiro_api_key') IS NOT NULL",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// ksk_ 号清单,**按创建时间倒序**,带终身调用数与积分。
    ///
    /// 用量取自 `usage_records`(**永不裁剪**),所以这里是**号的终身产出**,
    /// 不像只读 `request_logs` 时那样被 1.5 小时的环形窗口截断 ——
    /// 「这个号花了多少钱、换来多少次调用」因此才算得准。
    pub fn restock_account_inventory(&self) -> anyhow::Result<Vec<RestockAccountRow>> {
        let conn = self.stats_conn.lock();
        let mut stmt = conn.prepare(
            "SELECT a.account_id, a.created_at, a.disabled, a.max_concurrency,
                    (SELECT COUNT(*) FROM usage_records u WHERE u.account_id = a.account_id),
                    (SELECT COALESCE(SUM(u.success),0) FROM usage_records u
                      WHERE u.account_id = a.account_id),
                    (SELECT COALESCE(SUM(CASE WHEN u.metering_credit > 0
                                              THEN u.metering_credit ELSE 0 END),0)
                       FROM usage_records u WHERE u.account_id = a.account_id),
                    (SELECT COALESCE(GROUP_CONCAT(g.group_name || '@' || g.priority, ' '), '')
                       FROM account_groups g WHERE g.account_id = a.account_id),
                    (SELECT MIN(u.created_at) FROM usage_records u
                      WHERE u.account_id = a.account_id),
                    (SELECT MAX(u.created_at) FROM usage_records u
                      WHERE u.account_id = a.account_id)
               FROM accounts a
              WHERE json_extract(a.extra, '$.kiro_api_key') IS NOT NULL
              ORDER BY a.created_at DESC, a.account_id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(RestockAccountRow {
                    account_id: r.get(0)?,
                    created_at: r.get(1)?,
                    disabled: r.get::<_, i64>(2)? != 0,
                    max_concurrency: r.get(3)?,
                    calls: r.get(4)?,
                    success: r.get(5)?,
                    credits: r.get(6)?,
                    groups: r.get(7)?,
                    first_used_at: r.get(8)?,
                    last_used_at: r.get(9)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    // ───────────────────────── 自动补货:积分汇总 ─────────────────────────

    /// 把 `usage_records` 增量聚合进 `restock_credit_rollup`,推进一个批次。
    ///
    /// 返回 `(本批消费到的 id, 是否还没追平)`。调用方循环调用直到追平,或按每轮预算收手。
    ///
    /// **为什么源是 `usage_records` 而不是 `request_logs`**:后者是
    /// `REQUEST_LOG_CAP = 10_000` 的硬环形缓冲(实测只覆盖约 1.5 小时),想看跨天规律
    /// 必须自己攒;而 `usage_records` 同样带 `metering_credit` 且**永不裁剪**
    /// (见其建表注释),线上已有 51 天历史 —— 周画像因此上线即成熟,没有冷启动。
    ///
    /// **读写分离**:聚合走 `stats_conn`(只读连接),否则百万行 GROUP BY 会占住写锁,
    /// 让客户请求的计费落库排队(`stats_conn` 存在的理由,见其字段注释);
    /// 只有小结果集的 UPSERT 才拿写锁。
    ///
    /// **累加与游标推进在同一事务**:分开做的话,崩在两者之间会让同一段 id 被搬第二次,
    /// 积分凭空翻倍 —— 而这种错误在图上完全看不出来。
    pub fn restock_rollup_advance(&self, batch: i64) -> anyhow::Result<(i64, bool)> {
        let cursor: i64 = self
            .get_kv(Self::KEY_RESTOCK_CURSOR)?
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        // ① 只读连接上聚合。id 是 INTEGER PRIMARY KEY,区间扫描走主键索引。
        let (rows, max_id, more) = {
            let conn = self.stats_conn.lock();
            let hi: Option<i64> = conn.query_row(
                "SELECT MAX(id) FROM usage_records WHERE id > ?1 AND id <= ?1 + ?2",
                rusqlite::params![cursor, batch.max(1)],
                |r| r.get(0),
            )?;
            let Some(hi) = hi else {
                // 本批区间内没有行。可能是真追平了,也可能是这段 id 恰好空着(不会,
                // usage_records 从不删行),两种情况都直接把游标推到区间末尾。
                let global_max: i64 = conn
                    .query_row("SELECT COALESCE(MAX(id),0) FROM usage_records", [], |r| r.get(0))?;
                return if global_max > cursor {
                    Ok((cursor + batch.max(1), true))
                } else {
                    Ok((cursor, false))
                };
            };
            let mut stmt = conn.prepare(
                "SELECT (u.created_at/3600)*3600 AS hour_ts,
                        u.model,
                        CASE WHEN json_extract(a.extra,'$.kiro_api_key') IS NOT NULL
                                  OR u.account_id LIKE 'kiro-apikey-%'
                             THEN 1 ELSE 0 END AS ksk,
                        COUNT(*),
                        COALESCE(SUM(u.success),0),
                        COALESCE(SUM(CASE WHEN u.metering_credit > 0
                                          THEN u.metering_credit ELSE 0 END),0)
                   FROM usage_records u
                   -- LEFT JOIN:回收掉的号在 accounts 里已不存在,但它的用量还在。
                   -- 那种情况靠 account_id 前缀兜底,否则自购号生命末期的消耗会被
                   -- 算进「其它号」,补货口径直接偏低。
                   LEFT JOIN accounts a ON a.account_id = u.account_id
                  WHERE u.id > ?1 AND u.id <= ?2
                  GROUP BY hour_ts, u.model, ksk",
            )?;
            let rows: Vec<(i64, String, i64, i64, i64, f64)> = stmt
                .query_map(rusqlite::params![cursor, hi], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let global_max: i64 = conn
                .query_row("SELECT COALESCE(MAX(id),0) FROM usage_records", [], |r| r.get(0))?;
            (rows, hi, global_max > hi)
        };

        // ② 写锁只用于小结果集的 UPSERT + 游标,同一事务。
        {
            let mut conn = self.conn.lock();
            let tx = conn.transaction()?;
            for (hour_ts, model, ksk, calls, success, credits) in &rows {
                tx.execute(
                    "INSERT INTO restock_credit_rollup
                        (hour_ts, model, ksk, calls, success, credits)
                     VALUES (?1,?2,?3,?4,?5,?6)
                     ON CONFLICT(hour_ts, model, ksk) DO UPDATE SET
                        calls   = calls   + excluded.calls,
                        success = success + excluded.success,
                        credits = credits + excluded.credits",
                    rusqlite::params![hour_ts, model, ksk, calls, success, credits],
                )?;
            }
            tx.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value,
                                                updated_at = strftime('%s','now')",
                rusqlite::params![Self::KEY_RESTOCK_CURSOR, max_id.to_string()],
            )?;
            tx.commit()?;
        }
        Ok((max_id, more))
    }

    /// 小时聚合的明细行(面板画图与模型分解用)。
    pub fn restock_credit_series(&self, since_ts: i64) -> anyhow::Result<Vec<CreditRollupRow>> {
        let conn = self.stats_conn.lock();
        let mut stmt = conn.prepare(
            "SELECT hour_ts, model, ksk, calls, success, credits
               FROM restock_credit_rollup WHERE hour_ts >= ?1 ORDER BY hour_ts",
        )?;
        let rows = stmt
            .query_map([since_ts], |r| {
                Ok(CreditRollupRow {
                    hour_ts: r.get(0)?,
                    model: r.get(1)?,
                    ksk: r.get::<_, i64>(2)? == 1,
                    calls: r.get(3)?,
                    success: r.get(4)?,
                    credits: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 逐小时的 `(整点, ksk_ 积分, 全部积分)`,升序。**预测用这个** ——
    /// 让 SQLite 先合并掉模型维度,免得在 Rust 里对几万行做分组。
    pub fn restock_credit_hours(&self, since_ts: i64) -> anyhow::Result<Vec<(i64, f64, f64)>> {
        let conn = self.stats_conn.lock();
        let mut stmt = conn.prepare(
            "SELECT hour_ts,
                    SUM(CASE WHEN ksk = 1 THEN credits ELSE 0 END),
                    SUM(credits)
               FROM restock_credit_rollup WHERE hour_ts >= ?1
              GROUP BY hour_ts ORDER BY hour_ts",
        )?;
        let rows = stmt
            .query_map([since_ts], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 最近 `window_secs` 秒的实测消耗速率(**积分/小时**,全池口径)。
    ///
    /// 源用 `usage_records` 而不是 rollup:rollup 是整点聚合,当前这个不完整的小时
    /// 会显示成半根柱子,拿去和阈值比就会在每个整点后误判成「没需求」。
    ///
    /// 口径是**全池**(含贵号池)而非只算 ksk_ —— 我们要问的是「有多少活儿等着干」,
    /// 而不是「便宜号现在干了多少」。后者在断供时恰好是 0,拿它当需求会让补货自锁。
    pub fn restock_recent_credit_rate(&self, window_secs: i64) -> anyhow::Result<f64> {
        let w = window_secs.max(60);
        let conn = self.stats_conn.lock();
        let sum: f64 = conn.query_row(
            "SELECT COALESCE(SUM(CASE WHEN metering_credit > 0 THEN metering_credit ELSE 0 END), 0)
               FROM usage_records
              WHERE created_at >= CAST(strftime('%s','now') AS INTEGER) - ?1",
            [w],
            |r| r.get(0),
        )?;
        Ok(sum * 3600.0 / w as f64)
    }

    /// 窗口内每个 ksk_ 账号的 `(尝试数, 成功数)`。**补货判活用这个。**
    ///
    /// 源必须是 `request_logs` 而不是 `usage_records`:后者**只写成功记录**
    /// (线上近 24h 三万余行全是 `success=1`),分不出「试过但全败」与「压根没被选中」。
    /// 而这两者的处置完全相反 —— 前者是死号必须补货,后者只是没流量不能下结论。
    ///
    /// 环形缓冲(`REQUEST_LOG_CAP`)只覆盖数小时,对分钟级的判活窗口绰绰有余。
    pub fn restock_account_activity(
        &self,
        since_ts: i64,
    ) -> anyhow::Result<std::collections::HashMap<String, (i64, i64)>> {
        let conn = self.stats_conn.lock();
        let mut stmt = conn.prepare(
            "SELECT r.account_id, COUNT(*), COALESCE(SUM(r.success), 0)
               FROM request_logs r
               JOIN accounts a ON a.account_id = r.account_id
              WHERE r.created_at >= ?1
                AND json_extract(a.extra, '$.kiro_api_key') IS NOT NULL
              GROUP BY r.account_id",
        )?;
        let rows = stmt
            .query_map([since_ts], |r| {
                Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?)))
            })?
            .collect::<Result<std::collections::HashMap<_, _>, _>>()?;
        Ok(rows)
    }

    /// 最近 `sample` 个**已经死透**的自购号的实测寿命(秒,首次到末次成功的间隔)。
    ///
    /// 只取「末次成功已超过 `settled_after_secs`」的号 —— 还在服务的号只走完了半程,
    /// 算进去会把中位数一路拖低,进而让「该换号了」提前触发、白白多花钱。
    ///
    /// **只用于展示与人工校准**,不自动改 `expected_lifetime_secs`:寿命估计一旦估短,
    /// 后果是每轮都提前下单,花费直接翻倍,这种旋钮不该自己转。
    pub fn restock_measured_lifetimes(
        &self,
        sample: i64,
        settled_after_secs: i64,
    ) -> anyhow::Result<Vec<i64>> {
        let conn = self.stats_conn.lock();
        let mut stmt = conn.prepare(
            "SELECT MAX(u.created_at) - MIN(u.created_at)
               FROM restock_owned o
               JOIN usage_records u ON u.account_id = o.account_id
              GROUP BY o.account_id
             HAVING COUNT(*) > 1
                AND MAX(u.created_at) < CAST(strftime('%s','now') AS INTEGER) - ?2
              ORDER BY MAX(u.created_at) DESC
              LIMIT ?1",
        )?;
        let rows = stmt
            .query_map([sample.max(1), settled_after_secs.max(0)], |r| r.get(0))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 已攒到的聚合区间 `(最早整点, 最晚整点)`;空表返回 `None`。
    pub fn restock_rollup_span(&self) -> anyhow::Result<Option<(i64, i64)>> {
        let conn = self.stats_conn.lock();
        let r: (Option<i64>, Option<i64>) = conn.query_row(
            "SELECT MIN(hour_ts), MAX(hour_ts) FROM restock_credit_rollup",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        Ok(match r {
            (Some(a), Some(b)) => Some((a, b)),
            _ => None,
        })
    }

    // === 请求日志(调试用,环形保留最新 N 条)===

    /// request_logs 列表列(不含大 payload)。`Self::REQLOG_ROW_COLS` 顺序与
    /// [`Self::row_to_reqlog_row`] 一一对应。
    const REQLOG_ROW_COLS: &'static str = "id, created_at, client_key_id, account_id, model, \
         stream, success, status_code, error_kind, duration_ms, ttfb_ms, input_tokens, \
         output_tokens, cache_read_tokens, cache_creation_tokens, reported_tokens, \
         real_cache_read_tokens, metering_credit";

    /// 追加一条请求日志,并把表裁到**最新 `cap` 条**(cap=0 → 不裁,谨慎)。
    /// id 单调自增(AUTOINCREMENT 不复用),故 `id <= max_id - cap` 即"除最新 cap 条外全删"。
    pub fn insert_request_log(&self, log: &RequestLog, cap: u64) -> anyhow::Result<()> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        tx.execute(
            "INSERT INTO request_logs \
             (client_key_id, account_id, model, stream, success, status_code, error_kind, \
              duration_ms, ttfb_ms, input_tokens, output_tokens, cache_read_tokens, \
              cache_creation_tokens, reported_tokens, real_cache_read_tokens, metering_credit, \
              client_payload, kiro_payload, response_payload) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
            rusqlite::params![
                log.client_key_id,
                log.account_id,
                log.model,
                log.stream as i64,
                log.success as i64,
                log.status_code,
                log.error_kind,
                log.duration_ms,
                log.ttfb_ms,
                clamp_i64(log.input_tokens),
                clamp_i64(log.output_tokens),
                clamp_i64(log.cache_read_tokens),
                clamp_i64(log.cache_creation_tokens),
                clamp_i64(log.reported_tokens),
                clamp_i64(log.real_cache_read_tokens),
                log.metering_credit,
                gzip_text(&log.client_payload),
                gzip_text(&log.kiro_payload),
                gzip_text(&log.response_payload),
            ],
        )?;
        let max_id = tx.last_insert_rowid();
        // 媒体 blob 去重入库:INSERT OR IGNORE 按 hash 去重(同图复用一行),refs 记本条引用。
        for b in &log.blobs {
            tx.execute(
                "INSERT OR IGNORE INTO log_blobs (hash, media_type, data, bytes) VALUES (?1,?2,?3,?4)",
                rusqlite::params![b.hash, b.media_type, b.data, b.bytes],
            )?;
            tx.execute(
                "INSERT OR IGNORE INTO log_blob_refs (log_id, hash) VALUES (?1,?2)",
                rusqlite::params![max_id, b.hash],
            )?;
        }
        if cap > 0 {
            let cutoff = max_id - cap as i64;
            if cutoff > 0 {
                // 环形裁剪:删旧日志 + 其 blob 引用,再清掉已无任何引用的 blob(GC,防无限膨胀)。
                tx.execute("DELETE FROM request_logs WHERE id <= ?1", [cutoff])?;
                tx.execute("DELETE FROM log_blob_refs WHERE log_id <= ?1", [cutoff])?;
                tx.execute(
                    "DELETE FROM log_blobs WHERE hash NOT IN (SELECT hash FROM log_blob_refs)",
                    [],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 账号累计成功/失败计数 +1(监控用,非计费)。在 [`write_request_log`](../../gw_app) 的
    /// blocking 落库任务里调用,**不占热路径**。`account_id` 空(选号前无账号)则跳过;
    /// ghost 账号(已删)UPDATE 影响 0 行,无害。
    ///
    /// 记的是每次上游调用的**终态**结局:成功 → success_count+1,终态失败 → failure_count+1。
    /// 中途换号被禁用的那一次失败不计(该问题号的当前态已由运行态状态徽章暴露),count 只作量级参考。
    pub fn bump_account_counters(&self, account_id: &str, success: bool) -> anyhow::Result<()> {
        if account_id.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock();
        conn.execute(
            "UPDATE accounts SET success_count = success_count + ?1, \
             failure_count = failure_count + ?2 WHERE account_id = ?3",
            rusqlite::params![success as i64, (!success) as i64, account_id],
        )?;
        Ok(())
    }

    /// 列表查询(按筛选,id 降序;不含 payload)。`limit<=0` 时用 `default_limit`;
    /// `filter.offset>0` 时跳过前 N 条(分页)。
    pub fn list_request_logs(
        &self,
        filter: &RequestLogFilter,
        default_limit: i64,
    ) -> anyhow::Result<Vec<RequestLogRow>> {
        let (where_, mut params) = Self::reqlog_filter_where(filter);
        let limit = if filter.limit > 0 {
            filter.limit
        } else {
            default_limit
        };
        params.push(Value::Integer(limit));
        let limit_ph = params.len();
        params.push(Value::Integer(filter.offset.max(0)));
        let sql = format!(
            "SELECT {} FROM request_logs WHERE {} ORDER BY id DESC LIMIT ?{} OFFSET ?{}",
            Self::REQLOG_ROW_COLS,
            where_,
            limit_ph,
            params.len()
        );
        let conn = self.stats_conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), Self::row_to_reqlog_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 同筛选下的总条数(分页用,不受 limit/offset 影响)。
    pub fn count_request_logs(&self, filter: &RequestLogFilter) -> anyhow::Result<i64> {
        let (where_, params) = Self::reqlog_filter_where(filter);
        let sql = format!("SELECT COUNT(*) FROM request_logs WHERE {where_}");
        let conn = self.stats_conn.lock();
        let n = conn.query_row(&sql, rusqlite::params_from_iter(params), |r| r.get(0))?;
        Ok(n)
    }

    /// 取单条详情(含完整 client/kiro payload)。无此 id → `None`。
    pub fn get_request_log(&self, id: i64) -> anyhow::Result<Option<RequestLogDetail>> {
        let sql = format!(
            "SELECT {}, client_payload, kiro_payload, response_payload FROM request_logs WHERE id = ?1",
            Self::REQLOG_ROW_COLS
        );
        let conn = self.stats_conn.lock();
        let mut detail = match conn.query_row(&sql, [id], |r| {
            // 报文以 gzip BLOB 入库(旧明文行兼容),读时按动态 Value 还原。
            Ok(RequestLogDetail {
                row: Self::row_to_reqlog_row(r)?,
                client_payload: value_to_payload(r.get::<_, Value>(18)?),
                kiro_payload: value_to_payload(r.get::<_, Value>(19)?),
                response_payload: value_to_payload(r.get::<_, Value>(20)?),
                blobs: Vec::new(),
            })
        }) {
            Ok(d) => d,
            Err(rusqlite::Error::QueryReturnedNoRows) => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        // 本条日志报文里 blob:<hash> 引用到的去重媒体(图片/文档)。
        let mut stmt = conn.prepare(
            "SELECT b.hash, b.media_type, b.data, b.bytes \
             FROM log_blob_refs r JOIN log_blobs b ON b.hash = r.hash \
             WHERE r.log_id = ?1",
        )?;
        detail.blobs = stmt
            .query_map([id], |r| {
                Ok(LogBlob {
                    hash: r.get(0)?,
                    media_type: r.get(1)?,
                    data: r.get(2)?,
                    bytes: r.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Some(detail))
    }

    /// 测试用:当前去重媒体 blob 行数(校验去重/GC)。
    #[cfg(test)]
    fn count_log_blobs(&self) -> i64 {
        self.stats_conn
            .lock()
            .query_row("SELECT COUNT(*) FROM log_blobs", [], |r| r.get(0))
            .unwrap()
    }

    /// 把一行(`REQLOG_ROW_COLS` 顺序)映射为 [`RequestLogRow`]。
    fn row_to_reqlog_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<RequestLogRow> {
        Ok(RequestLogRow {
            id: r.get(0)?,
            created_at: r.get(1)?,
            client_key_id: r.get(2)?,
            account_id: r.get(3)?,
            model: r.get(4)?,
            stream: r.get::<_, i64>(5)? != 0,
            success: r.get::<_, i64>(6)? != 0,
            status_code: r.get(7)?,
            error_kind: r.get(8)?,
            duration_ms: r.get(9)?,
            ttfb_ms: r.get(10)?,
            input_tokens: r.get(11)?,
            output_tokens: r.get(12)?,
            cache_read_tokens: r.get(13)?,
            cache_creation_tokens: r.get(14)?,
            reported_tokens: r.get(15)?,
            real_cache_read_tokens: r.get(16)?,
            metering_credit: r.get(17)?,
        })
    }

    /// request_logs 的 WHERE 子句 + 参数(占位符从 ?1 起;调用方可继续 push LIMIT)。
    fn reqlog_filter_where(f: &RequestLogFilter) -> (String, Vec<Value>) {
        let since = f.since_unix.unwrap_or(0);
        let until = f.until_unix.unwrap_or(i64::MAX);
        let mut clause = String::from("created_at >= ?1 AND created_at < ?2");
        let mut params = vec![Value::Integer(since), Value::Integer(until)];
        if let Some(a) = &f.account_id {
            params.push(Value::Text(a.clone()));
            clause.push_str(&format!(" AND account_id = ?{}", params.len()));
        }
        if let Some(m) = &f.model {
            params.push(Value::Text(m.clone()));
            clause.push_str(&format!(" AND model = ?{}", params.len()));
        }
        if let Some(s) = f.success {
            params.push(Value::Integer(s as i64));
            clause.push_str(&format!(" AND success = ?{}", params.len()));
        }
        (clause, params)
    }

    /// 分组是否存在(admin 写入 group_name 前的存在性校验,防"幽灵分组")。
    pub fn group_exists(&self, name: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let exists = conn
            .prepare_cached("SELECT 1 FROM groups WHERE name = ?1")?
            .exists([name])?;
        Ok(exists)
    }

    /// 取**归属**于 `owner` 的全部账号并转换为运行时 [`Account`](gw_core::account::Account)
    /// (extra JSON 解码回字段表;含已禁用账号,调度器自行处理 disabled)。
    /// 单行 extra 损坏时跳过该账号并告警,不拖垮整组。
    ///
    /// 这里查的是 `accounts.group_name` = **归属**(哪个 worker 独占管它的运行态),
    /// 不是成员边。可见性/组内排序由 [`Self::load_group_memberships`] 单独给出:
    /// worker 装载它名下**全部**账号的运行态,再按请求所属分组过滤出可见子集。
    pub fn load_owned_accounts(&self, owner: &str) -> anyhow::Result<Vec<Account>> {
        let rows = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {} FROM accounts WHERE group_name = ?1 ORDER BY account_id ASC",
                Self::ACCOUNT_COLS
            ))?;
            let collected = stmt
                .query_map([owner], Self::row_to_account)?
                .collect::<Result<Vec<_>, _>>()?;
            collected
        };
        let mut accounts = Vec::with_capacity(rows.len());
        for row in rows {
            let extra: std::collections::BTreeMap<String, serde_json::Value> =
                match serde_json::from_str(&row.extra) {
                    Ok(m) => m,
                    Err(e) => {
                        tracing::error!(account = %row.account_id, "extra JSON 损坏,跳过该账号: {e}");
                        continue;
                    }
                };
            accounts.push(Account {
                account_id: row.account_id,
                provider: row.provider,
                max_concurrency: row.max_concurrency.clamp(1, u32::MAX as i64) as u32,
                disabled: row.disabled,
                // 暖机号龄的事实源(accounts.created_at,对补货号≈上游激活时间)。
                created_at: row.created_at,
                extra,
            });
        }
        Ok(accounts)
    }

    /// 取 `owner` 名下账号参与的**全部成员边**,按组聚成视图:`组名 → {账号 → 组内优先级}`。
    ///
    /// worker 每轮同步一次,选号时按请求所属分组现取一张视图 —— 不在视图里的号对该
    /// 请求**不存在**,在视图里的号按 priority 分层(数值越小越优先)。
    ///
    /// 只回本 owner 名下的边:跨 owner 的成员边由对应 worker 各自持有,谁也不越界管
    /// 别人的号(单一持有者约束)。
    pub fn load_group_memberships(
        &self,
        owner: &str,
    ) -> anyhow::Result<HashMap<String, HashMap<String, i64>>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT m.group_name, m.account_id, m.priority \
             FROM account_groups m JOIN accounts a ON a.account_id = m.account_id \
             WHERE a.group_name = ?1",
        )?;
        let mut out: HashMap<String, HashMap<String, i64>> = HashMap::new();
        let rows = stmt.query_map([owner], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
        })?;
        for row in rows {
            let (group, account, priority) = row?;
            out.entry(group).or_default().insert(account, priority);
        }
        Ok(out)
    }

    /// 账号列表 + 每个账号的成员边,**同一把锁下读完**(单连接 = 同一快照)。
    ///
    /// 给 admin 账号列表页用 —— 一次查完,免得前端按组 N 次拉成员再自己做反向索引。
    /// 与 [`Self::load_group_memberships`] 的区别:那个按 owner 过滤、给 worker 选号用;
    /// 这个不过滤,是**配置态全景**。
    ///
    /// 必须一把锁读完两张表:分两次调用的话,导入正好插在中间就会返回"有账号但没有边"
    /// 的组合 —— 而 `create_account` 是原子建号+建边的,那个状态从未真实存在过。
    /// 前端拿这份响应当"无分组"告警依据和编辑差集基线,喂它一个假快照会直接误导运维。
    ///
    /// 组名升序只为展示稳定(表格 chip 不跳动);差集比较按组名做 Map 查找,不依赖顺序。
    pub fn list_accounts_with_memberships(
        &self,
    ) -> anyhow::Result<Vec<(AccountRow, Vec<(String, i64)>)>> {
        let conn = self.conn.lock();
        let mut edges: HashMap<String, Vec<(String, i64)>> = HashMap::new();
        {
            let mut stmt = conn.prepare_cached(
                "SELECT account_id, group_name, priority FROM account_groups \
                 ORDER BY account_id ASC, group_name ASC",
            )?;
            let rows = stmt.query_map([], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?))
            })?;
            for row in rows {
                let (account, group, priority) = row?;
                edges.entry(account).or_default().push((group, priority));
            }
        }
        let mut stmt = conn.prepare_cached(&format!(
            "SELECT {} FROM accounts ORDER BY group_name ASC, account_id ASC",
            Self::ACCOUNT_COLS
        ))?;
        let accounts = stmt
            .query_map([], Self::row_to_account)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(accounts
            .into_iter()
            .map(|row| {
                let e = edges.remove(&row.account_id).unwrap_or_default();
                (row, e)
            })
            .collect())
    }

    /// 组名 → **持有该组成员的 owner 集合**(即请求该组时可以派发到哪些 worker)。
    ///
    /// router 据此选 worker:旧模型里「组」与「worker」一一对应,现在一个组的成员可以
    /// 分散在多个 owner 名下,router 在其中做亲和/负载即可。没有成员的组不会出现在结果
    /// 里 —— 那种组本来也服务不了任何请求,让它 503 比静默回落到主组安全得多。
    pub fn group_owners(&self) -> anyhow::Result<HashMap<String, Vec<String>>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT m.group_name, a.group_name FROM account_groups m \
             JOIN accounts a ON a.account_id = m.account_id \
             WHERE a.group_name <> '' ORDER BY 1, 2",
        )?;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        for row in rows {
            let (group, owner) = row?;
            out.entry(group).or_default().push(owner);
        }
        Ok(out)
    }

    /// 建/改一条成员边(存在则改组内优先级)。
    ///
    /// 外键在写入侧校验而非靠 SQLite FK:悬空边不会报错,只会让该组静默少一个号 ——
    /// 比起立即失败,那种"配了但不生效"更难排查。
    ///
    /// **一个组的成员必须同属一个 owner**(对抗审查 Architect#2)。跨 owner 时组内
    /// priority 不再是全局排序:router 只按会话数选 worker,被选中的 worker 只看得见
    /// 自己那部分成员,于是可能直接用兜底层,而另一个 owner 的主力号正闲着 ——
    /// "小号优先、压满才溢出"当场失效。与其假装支持跨 owner,不如在写入侧明确拒绝。
    pub fn upsert_membership(
        &self,
        account_id: &str,
        group_name: &str,
        priority: i64,
    ) -> anyhow::Result<MembershipOutcome> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let owner: Option<String> = tx
            .prepare_cached("SELECT group_name FROM accounts WHERE account_id = ?1")?
            .query_row([account_id], |r| r.get(0))
            .optional()?;
        let Some(owner) = owner else {
            return Ok(MembershipOutcome::MissingAccountOrGroup);
        };
        if !tx.prepare_cached("SELECT 1 FROM groups WHERE name = ?1")?.exists([group_name])? {
            return Ok(MembershipOutcome::MissingAccountOrGroup);
        }
        // 本组现有成员的 owner(取任意一个:同 owner 是不变量,不同即违规)。
        let existing: Option<String> = tx
            .prepare_cached(
                "SELECT a.group_name FROM account_groups m JOIN accounts a \
                 ON a.account_id = m.account_id WHERE m.group_name = ?1 \
                 AND m.account_id <> ?2 LIMIT 1",
            )?
            .query_row(rusqlite::params![group_name, account_id], |r| r.get(0))
            .optional()?;
        if let Some(existing) = existing {
            if existing != owner {
                return Ok(MembershipOutcome::CrossOwner { existing, incoming: owner });
            }
        }
        tx.execute(
            "INSERT INTO account_groups (account_id, group_name, priority) VALUES (?1, ?2, ?3) \
             ON CONFLICT(account_id, group_name) DO UPDATE SET priority = excluded.priority",
            rusqlite::params![account_id, group_name, priority],
        )?;
        tx.commit()?;
        Ok(MembershipOutcome::Ok)
    }

    /// 删一条成员边;`false` = 边不存在。
    pub fn remove_membership(&self, account_id: &str, group_name: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "DELETE FROM account_groups WHERE account_id = ?1 AND group_name = ?2",
            [account_id, group_name],
        )?;
        Ok(changed == 1)
    }

    /// 列一个组的成员边(账号 id → 组内优先级),按优先级再按 id 排序,便于前端分层展示。
    pub fn list_group_members(&self, group_name: &str) -> anyhow::Result<Vec<(String, i64)>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT account_id, priority FROM account_groups WHERE group_name = ?1 \
             ORDER BY priority ASC, account_id ASC",
        )?;
        let rows = stmt
            .query_map([group_name], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// **批量**按条件加成员:220 个号手工点不现实。
    ///
    /// `subscription_title` / `owner` 任一为 `None` = 该维度不筛。返回新建或改动的边数。
    /// 与 `upsert_membership` 同语义(已存在的边会被改成新的组内优先级),**同样受
    /// "一个组的成员必须同属一个 owner" 约束** —— 筛出的号跨了 owner 就整批拒绝,
    /// 不做部分写入(半批成功比失败更难排查)。
    pub fn bulk_add_members(
        &self,
        group_name: &str,
        owner: Option<&str>,
        subscription_title: Option<&str>,
        priority: i64,
    ) -> anyhow::Result<Result<usize, MembershipOutcome>> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        if !tx.prepare_cached("SELECT 1 FROM groups WHERE name = ?1")?.exists([group_name])? {
            return Ok(Err(MembershipOutcome::MissingAccountOrGroup));
        }
        const FILTER: &str = "FROM accounts a WHERE a.group_name <> '' \
             AND (?2 IS NULL OR a.group_name = ?2) \
             AND (?3 IS NULL OR json_extract(a.extra, '$.subscription_title') = ?3)";
        // 本批 + 组内既有成员合起来必须只有一个 owner。
        let mut stmt = tx.prepare(&format!(
            "SELECT DISTINCT a.group_name {FILTER} \
             UNION SELECT DISTINCT a2.group_name FROM account_groups m \
             JOIN accounts a2 ON a2.account_id = m.account_id WHERE m.group_name = ?1"
        ))?;
        let owners: Vec<String> = stmt
            .query_map(rusqlite::params![group_name, owner, subscription_title], |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        if owners.len() > 1 {
            let mut sorted = owners;
            sorted.sort();
            return Ok(Err(MembershipOutcome::CrossOwner {
                existing: sorted[0].clone(),
                incoming: sorted[1].clone(),
            }));
        }
        let n = tx.execute(
            &format!(
                "INSERT INTO account_groups (account_id, group_name, priority) \
                 SELECT a.account_id, ?1, ?4 {FILTER} \
                 ON CONFLICT(account_id, group_name) DO UPDATE SET priority = excluded.priority"
            ),
            rusqlite::params![group_name, owner, subscription_title, priority],
        )?;
        tx.commit()?;
        Ok(Ok(n))
    }

    /// 幂等导入 accounts.yaml(组 + 账号,INSERT OR IGNORE,已有行不覆盖——
    /// DB 是事实源,yaml 只做首次播种,绝不回滚已 roll 的 token);
    /// 返回本次新插入的账号数。worker 启动时调用,完成 yaml → SQLite 的迁移。
    pub fn import_accounts(&self, cfg: &AccountsConfig) -> anyhow::Result<usize> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let mut inserted = 0usize;
        for (gname, group) in &cfg.groups {
            tx.execute("INSERT OR IGNORE INTO groups (name) VALUES (?1)", [gname])?;
            for acc in &group.accounts {
                let provider = if acc.provider.is_empty() {
                    group.provider.as_str()
                } else {
                    acc.provider.as_str()
                };
                let extra_json = serde_json::to_string(&acc.extra)?;
                inserted += tx.execute(
                    "INSERT OR IGNORE INTO accounts \
                     (account_id, group_name, provider, max_concurrency, disabled, extra) \
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    rusqlite::params![
                        acc.account_id,
                        gname,
                        provider,
                        acc.max_concurrency.max(1) as i64,
                        acc.disabled as i64,
                        extra_json,
                    ],
                )?;
                // 与 create_account 同理:没有成员边的号对所有客户不可见。
                tx.execute(
                    "INSERT OR IGNORE INTO account_groups (account_id, group_name, priority) \
                     VALUES (?1, ?2, COALESCE(CAST(json_extract(?3, '$.priority') AS INTEGER), 100))",
                    rusqlite::params![acc.account_id, gname, extra_json],
                )?;
            }
        }
        tx.commit()?;
        Ok(inserted)
    }

    // ───────── 用量统计查询(admin 看板;按 [`UsageFilter`] 时间窗 + key 筛选) ─────────

    /// 把筛选条件编译成 SQL WHERE 子句 + 位置参数(?1=since, ?2=until, ?3=key 可选)。
    fn filter_where(f: &UsageFilter) -> (String, Vec<Value>) {
        let since = f.since_unix.unwrap_or(0);
        let until = f.until_unix.unwrap_or(i64::MAX);
        let mut clause = String::from("created_at >= ?1 AND created_at < ?2");
        let mut params = vec![Value::Integer(since), Value::Integer(until)];
        if let Some(k) = &f.client_key_id {
            clause.push_str(" AND client_key_id = ?3");
            params.push(Value::Text(k.clone()));
        }
        (clause, params)
    }

    /// 用量总览(按筛选)。
    pub fn usage_summary(&self, filter: &UsageFilter) -> anyhow::Result<UsageSummary> {
        let (where_, params) = Self::filter_where(filter);
        let sql = format!(
            "SELECT COUNT(*), COALESCE(SUM(success),0), COALESCE(SUM(input_tokens),0), \
             COALESCE(SUM(output_tokens),0), COALESCE(SUM(cache_read_tokens),0), \
             COALESCE(SUM(cache_creation_tokens),0), COALESCE(SUM(real_cache_read_tokens),0), \
             COALESCE(SUM(CASE WHEN metering_credit > 0 THEN metering_credit ELSE 0 END),0) FROM usage_records WHERE {where_}"
        );
        let conn = self.stats_conn.lock();
        let s = conn.query_row(&sql, rusqlite::params_from_iter(params), |r| {
            Ok(UsageSummary {
                requests: r.get::<_, i64>(0)? as u64,
                success_requests: r.get::<_, i64>(1)? as u64,
                input_tokens: r.get::<_, i64>(2)? as u64,
                output_tokens: r.get::<_, i64>(3)? as u64,
                cache_read_tokens: r.get::<_, i64>(4)? as u64,
                cache_creation_tokens: r.get::<_, i64>(5)? as u64,
                real_cache_read_tokens: r.get::<_, i64>(6)? as u64,
                metering_credit: r.get::<_, f64>(7)?,
            })
        })?;
        Ok(s)
    }

    /// 按模型聚合(请求数降序,按筛选)。
    pub fn usage_by_model(&self, filter: &UsageFilter) -> anyhow::Result<Vec<UsageByModel>> {
        let (where_, params) = Self::filter_where(filter);
        let sql = format!(
            "SELECT model, COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), \
             COALESCE(SUM(cache_read_tokens),0), COALESCE(SUM(cache_creation_tokens),0), \
             COALESCE(SUM(real_cache_read_tokens),0), COALESCE(SUM(CASE WHEN metering_credit > 0 THEN metering_credit ELSE 0 END),0) \
             FROM usage_records WHERE {where_} GROUP BY model ORDER BY COUNT(*) DESC"
        );
        let conn = self.stats_conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |r| {
                Ok(UsageByModel {
                    model: r.get(0)?,
                    requests: r.get::<_, i64>(1)? as u64,
                    input_tokens: r.get::<_, i64>(2)? as u64,
                    output_tokens: r.get::<_, i64>(3)? as u64,
                    cache_read_tokens: r.get::<_, i64>(4)? as u64,
                    cache_creation_tokens: r.get::<_, i64>(5)? as u64,
                    real_cache_read_tokens: r.get::<_, i64>(6)? as u64,
                    metering_credit: r.get::<_, f64>(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 按客户 apikey(client_key_id)聚合(请求数降序,按筛选——通常不传 key)。
    pub fn usage_by_key(&self, filter: &UsageFilter) -> anyhow::Result<Vec<UsageByKey>> {
        let (where_, params) = Self::filter_where(filter);
        let sql = format!(
            "SELECT client_key_id, COUNT(*), COALESCE(SUM(success),0), \
             COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), \
             COALESCE(SUM(cache_read_tokens),0), COALESCE(SUM(cache_creation_tokens),0), \
             COALESCE(SUM(real_cache_read_tokens),0), COALESCE(SUM(CASE WHEN metering_credit > 0 THEN metering_credit ELSE 0 END),0) \
             FROM usage_records WHERE {where_} GROUP BY client_key_id ORDER BY COUNT(*) DESC"
        );
        let conn = self.stats_conn.lock();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(params), |r| {
                Ok(UsageByKey {
                    client_key_id: r.get(0)?,
                    requests: r.get::<_, i64>(1)? as u64,
                    success_requests: r.get::<_, i64>(2)? as u64,
                    input_tokens: r.get::<_, i64>(3)? as u64,
                    output_tokens: r.get::<_, i64>(4)? as u64,
                    cache_read_tokens: r.get::<_, i64>(5)? as u64,
                    cache_creation_tokens: r.get::<_, i64>(6)? as u64,
                    real_cache_read_tokens: r.get::<_, i64>(7)? as u64,
                    metering_credit: r.get::<_, f64>(8)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[async_trait]
impl ControlStore for SqliteStore {
    async fn authenticate(&self, api_key: &str) -> anyhow::Result<Option<AuthenticatedKey>> {
        let conn = self.conn.lock();
        // over_quota 在 SQL 内算好(quota_tokens NULL = 不限),鉴权路径零额外查询。
        // 影子组策略同样 LEFT JOIN 带出:仍是一次查询,且策略每请求现读 —— admin 改完
        // 下一个请求即生效(回退路径 `PATCH shadow_of=''` 因此是秒级的,无需重启)。
        // LEFT JOIN 而非 JOIN:key 未分组('')或分组行已不存在时,g.* 全为 NULL,
        // tier 落 None,**绝不能让鉴权失败**。
        let mut stmt = conn.prepare_cached(
            "SELECT k.key, k.disabled, \
             (k.quota_tokens IS NOT NULL AND k.used_tokens >= k.quota_tokens), \
             k.group_name FROM api_keys k WHERE k.key = ?1",
        )?;
        let row = stmt
            .query_row([api_key], |r| {
                Ok(AuthenticatedKey {
                    key_id: r.get::<_, String>(0)?,
                    disabled: r.get::<_, i64>(1)? != 0,
                    over_quota: r.get::<_, i64>(2)? != 0,
                    group_name: r.get::<_, String>(3)?,
                })
            })
            .ok();
        Ok(row)
    }
}

/// u64 token 数 → SQLite i64,超 i64::MAX 饱和封顶(不回绕)。
fn clamp_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

#[async_trait]
impl UsageSink for SqliteStore {
    async fn record(&self, usage: UsageRecord) -> anyhow::Result<()> {
        // u64 → i64(SQLite INTEGER):token 计数实际远小于 i64::MAX;万一上游/解析异常
        // 给出超大值,饱和到 i64::MAX 而非静默回绕成负数污染计费(审查 Skeptic#4)。
        let conn = self.conn.lock();
        conn.execute(
            "INSERT INTO usage_records \
             (client_key_id, account_id, model, input_tokens, output_tokens, \
              cache_read_tokens, cache_creation_tokens, real_cache_read_tokens, \
              metering_credit, success) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            rusqlite::params![
                usage.client_key_id,
                usage.account_id,
                usage.model,
                clamp_i64(usage.input_tokens),
                clamp_i64(usage.output_tokens),
                clamp_i64(usage.cache_read_tokens),
                clamp_i64(usage.cache_creation_tokens),
                clamp_i64(usage.real_cache_read_tokens),
                // credit 非负兜底:异常负值不污染"每积分成本"分母。
                usage.metering_credit.max(0.0),
                usage.success as i64,
            ],
        )?;
        // 限额计量:把本次消耗累加到 key 的 used_tokens(v1 口径 = input+output;
        // cache 两类暂不计,后续换加权成本时一并调整)。锁内两条语句,无并发缝隙。
        if !usage.client_key_id.is_empty() {
            let consumed =
                clamp_i64(usage.input_tokens).saturating_add(clamp_i64(usage.output_tokens));
            if consumed > 0 {
                conn.execute(
                    "UPDATE api_keys SET used_tokens = used_tokens + ?1 WHERE key = ?2",
                    rusqlite::params![consumed, usage.client_key_id],
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod restock_tests {
    use super::*;

    /// **这是本次改动的红线测试。** 生产上有两个以上 `--mode router` 共用同一个
    /// control.db,补货循环若不互斥就是各买各的、重复扣款。
    #[test]
    fn 租约同一时刻只有一个持有者() {
        let s = SqliteStore::open_in_memory().unwrap();
        assert!(s.try_acquire_restock_lease("router-A", 60).unwrap(), "首个应当选");
        assert!(
            !s.try_acquire_restock_lease("router-B", 60).unwrap(),
            "租约未过期时第二个进程必须抢不到 —— 抢到就是重复扣款"
        );
        // 持有者可以续租(每轮都会调一次)。
        assert!(s.try_acquire_restock_lease("router-A", 60).unwrap());
        assert_eq!(s.restock_lease_holder().unwrap().as_deref(), Some("router-A"));
    }

    #[test]
    fn 租约过期后由别的进程接手() {
        let s = SqliteStore::open_in_memory().unwrap();
        // TTL 传 0 会被 max(1) 钳成 1 秒;这里直接写一条已过期的租约来验接管路径,
        // 免得测试真去 sleep。
        s.upsert_kv(
            SqliteStore::KEY_RESTOCK_LEASE,
            r#"{"holder":"router-A","expires_at":1}"#,
        )
        .unwrap();
        assert_eq!(s.restock_lease_holder().unwrap(), None, "过期的不算持有");
        assert!(s.try_acquire_restock_lease("router-B", 60).unwrap(), "过期后应能接手");
        assert_eq!(s.restock_lease_holder().unwrap().as_deref(), Some("router-B"));
    }

    #[test]
    fn 只有持有者本人能让出租约() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.try_acquire_restock_lease("router-A", 60).unwrap();
        s.release_restock_lease("router-B").unwrap();
        assert_eq!(
            s.restock_lease_holder().unwrap().as_deref(),
            Some("router-A"),
            "别人不能把我的租约释放掉,否则等于绕过互斥"
        );
        s.release_restock_lease("router-A").unwrap();
        assert_eq!(s.restock_lease_holder().unwrap(), None);
    }

    #[test]
    fn 花钱锁与leader租约互不干扰且能互斥两个执行体() {
        let s = SqliteStore::open_in_memory().unwrap();
        const LOCK: &str = SqliteStore::KEY_RESTOCK_PURCHASE_LOCK;

        // 后台循环长期持有 leader 租约。
        assert!(s.try_acquire_restock_lease("loop-A", 60).unwrap());

        // 管理面(另一个 holder)照样能拿到**花钱锁** —— 这正是两把锁分开的意义:
        // 若让 buy-now 去抢 leader 租约,它会永远拿不到,变成点了没反应的按钮。
        assert!(s.try_acquire_lock(LOCK, "admin-1", 90).unwrap());
        assert!(s.holds_lock(LOCK, "admin-1").unwrap());

        // 此时后台循环**买不了** —— 手动购买正在临界区内。
        assert!(
            !s.try_acquire_lock(LOCK, "loop-A", 90).unwrap(),
            "花钱锁必须真的互斥,否则两个执行体会读到同一个 spent 再各自下单"
        );
        assert!(!s.holds_lock(LOCK, "loop-A").unwrap());

        // 同一个 holder 可重入(续期),不会把自己锁死。
        assert!(s.try_acquire_lock(LOCK, "admin-1", 90).unwrap());

        // 释放后别人立刻能进。
        s.release_lock(LOCK, "admin-1").unwrap();
        assert!(s.try_acquire_lock(LOCK, "loop-A", 90).unwrap());

        // 非持有者释放不掉(避免误伤接任者)。
        s.release_lock(LOCK, "admin-1").unwrap();
        assert!(s.holds_lock(LOCK, "loop-A").unwrap(), "别人不能释放我的锁");
    }

    #[test]
    fn 过期的花钱锁可被接管() {
        let s = SqliteStore::open_in_memory().unwrap();
        const LOCK: &str = SqliteStore::KEY_RESTOCK_PURCHASE_LOCK;
        // 直接构造一个「持有者已死、租期已过」的锁。不能靠传负 TTL ——
        // `try_acquire_lock` 里有 `ttl_secs.max(1)`,负数会被拉成 1 秒而不是立即过期;
        // 也不能靠 sleep,那会让这条用例变慢且不稳。
        s.upsert_kv(LOCK, r#"{"holder":"dead","expires_at":1}"#).unwrap();

        assert!(!s.holds_lock(LOCK, "dead").unwrap(), "已过期的锁不算持有");
        // 持有者被 SIGKILL(docker stop 10s 后就是)时锁必须能自己过期,
        // 否则补货会被一个已经不存在的进程永久锁死。
        assert!(s.try_acquire_lock(LOCK, "alive", 90).unwrap(), "过期后必须能被接管");
        assert!(s.holds_lock(LOCK, "alive").unwrap());
        // 而未过期时不能被抢。
        assert!(!s.try_acquire_lock(LOCK, "other", 90).unwrap());
    }

    #[test]
    fn 在途订单按限价计入日预算否则会重复购买() {
        let s = SqliteStore::open_in_memory().unwrap();
        // 场景复现:单轮 120s 外层超时把购买掐断,订单停在 pending。
        s.restock_create_order("stuck", 1, 21.24, "drop", "", "").unwrap();

        let (spent, bought) = s.restock_spent_since(0).unwrap();
        assert!(
            (spent - 21.24).abs() < 1e-9,
            "在途单必须按限价(最坏情况)计入花费,实际 {spent} —— \
             算成 0 就会让下一轮以为没花过钱,换个 order_id 再买一次"
        );
        assert_eq!(bought, 0, "在途的不该算进「今日已补 N 个」");

        // 对账落定成真实扣款后,那个高估自动消失。
        s.restock_mark_purchased("stuck", &["ksk_a".into()], None, Some(200.0), 179.94).unwrap();
        let (spent, bought) = s.restock_spent_since(0).unwrap();
        assert!((spent - 20.06).abs() < 1e-9, "落定后按实扣记,实际 {spent}");
        assert_eq!(bought, 1);

        // 判死的单记 0(确定没扣款)。
        s.restock_create_order("dead", 1, 21.24, "drop", "", "").unwrap();
        s.restock_mark_status("dead", "failed", "400 参数错").unwrap();
        let (spent, _) = s.restock_spent_since(0).unwrap();
        assert!((spent - 20.06).abs() < 1e-9, "failed 不该计入花费,实际 {spent}");
    }

    #[test]
    fn 幂等键先落库且实扣按余额差记() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.restock_create_order("oid1", 1, 21.24, "drop", "", "").unwrap();
        // 崩在这里也能查到这个 id —— 这正是先落库的意义。
        // 年龄门槛传 0:刚落库的订单也要能查到。生产上对账用 300s,见其文档。
        let pending = s.restock_pending_orders(0).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].client_order_id, "oid1");
        assert_eq!(s.restock_pending_count().unwrap(), 1);
        // 但**刚落库的单不该被对账碰**:它可能正在飞,重放就是第二次扣款。
        assert!(
            s.restock_pending_orders(300).unwrap().is_empty(),
            "未够龄的在途单必须被年龄门槛挡住"
        );

        let spent = s
            .restock_mark_purchased("oid1", &["ksk_a".into()], None, Some(200.0), 179.94)
            .unwrap();
        assert!((spent - 20.06).abs() < 1e-9, "实扣必须按余额差,实际 {spent}");
        assert!(s.restock_pending_orders(0).unwrap().is_empty());
        assert_eq!(s.restock_pending_count().unwrap(), 0);
        // 停在 purchased = 钱花了号没上,必须能被单独查出来。
        assert_eq!(s.restock_orphan_orders().unwrap().len(), 1);
    }

    #[test]
    fn 拿不到购买前余额时实扣记零而不是负数() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.restock_create_order("oid2", 1, 21.24, "drop", "", "").unwrap();
        let spent = s.restock_mark_purchased("oid2", &["ksk_b".into()], None, None, 100.0).unwrap();
        assert_eq!(spent, 0.0, "没有基准就记 0,由调用方告警,绝不能算出负数污染日预算");
    }

    #[test]
    fn 日花费只统计成功购买的单() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.restock_create_order("ok1", 1, 21.0, "drop", "", "").unwrap();
        s.restock_mark_purchased("ok1", &["ksk_a".into()], None, Some(100.0), 80.0).unwrap();
        s.restock_create_order("bad1", 1, 21.0, "drop", "", "").unwrap();
        s.restock_mark_status("bad1", "failed", "网络中断").unwrap();
        let (spent, n) = s.restock_spent_since(0).unwrap();
        assert!((spent - 20.0).abs() < 1e-9, "失败单不该计入花费,实际 {spent}");
        assert_eq!(n, 1);
    }

    #[test]
    fn 逐家花费与全局花费同口径() {
        let s = SqliteStore::open_in_memory().unwrap();
        // drop 成交 ¥20
        s.restock_create_order("d1", 1, 21.0, "drop", "", "").unwrap();
        s.restock_mark_purchased("d1", &["ksk_a".into()], None, Some(100.0), 80.0).unwrap();
        // kiroapp 成交 ¥15.43(适配器自报扣款,不走余额差)
        s.restock_create_order("k1", 1, 16.0, "kiroapp", "eu", "eu-central-1").unwrap();
        s.restock_mark_purchased("k1", &["ksk_b".into()], Some(15.43), None, 0.0).unwrap();
        // kiroapp 还有一张在途:必须按**限价**计入,否则下一轮会当它没花过钱再买一次。
        s.restock_create_order("k2", 1, 16.0, "kiroapp", "eu", "eu-central-1").unwrap();
        // 失败单两边都不计。
        s.restock_create_order("k3", 1, 16.0, "kiroapp", "eu", "eu-central-1").unwrap();
        s.restock_mark_status("k3", "failed", "余额不足").unwrap();

        let drop_spent = s.restock_spent_since_by_supplier(0, "drop").unwrap();
        let kiro_spent = s.restock_spent_since_by_supplier(0, "kiroapp").unwrap();
        assert!((drop_spent - 20.0).abs() < 1e-9, "实际 {drop_spent}");
        assert!((kiro_spent - (15.43 + 16.0)).abs() < 1e-9, "在途要按限价计入,实际 {kiro_spent}");

        // 逐家之和必须等于全局 —— 对不上的面板会让人以为系统在骗自己。
        let (total, _) = s.restock_spent_since(0).unwrap();
        assert!((total - (drop_spent + kiro_spent)).abs() < 1e-9, "全局 {total}");

        // 历史订单(多供应商之前下的,supplier 为空)不归任何一家,但仍进全局账。
        s.restock_create_order("old", 1, 21.0, "", "", "").unwrap();
        s.restock_mark_purchased("old", &["ksk_c".into()], None, Some(50.0), 30.0).unwrap();
        assert_eq!(s.restock_spent_since_by_supplier(0, "drop").unwrap(), drop_spent);
        let (total2, _) = s.restock_spent_since(0).unwrap();
        assert!((total2 - total - 20.0).abs() < 1e-9);
    }

    #[test]
    fn 订单记得住货源与区域否则在途单无人认领() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.restock_create_order("k9", 2, 32.0, "kiroapp", "eu", "eu-central-1").unwrap();
        let o = s.restock_pending_orders(0).unwrap().pop().unwrap();
        assert_eq!(o.supplier, "kiroapp", "对账要靠它才知道该问哪一家");
        assert_eq!(o.shelf, "eu");
        assert_eq!(o.region, "eu-central-1");
        // 对账要用**下单时的限价**重放,所以它必须读得回来。
        assert!((o.max_total_cny - 32.0).abs() < 1e-9);
    }

    #[test]
    fn 汇总累加而非覆盖且游标随之前进() {
        let s = SqliteStore::open_in_memory().unwrap();
        {
            let c = s.conn.lock();
            c.execute_batch(
                "INSERT INTO accounts (account_id, extra) VALUES
                    ('kiro-apikey-a', '{\"kiro_api_key\":\"ksk_x\"}'),
                    ('manual-1', '{\"access_token\":\"t\"}');
                 INSERT INTO usage_records (id, account_id, model, metering_credit, success, created_at)
                 VALUES (1,'kiro-apikey-a','opus',1.5,1,3600),
                        (2,'kiro-apikey-a','opus',2.5,1,3600),
                        (3,'manual-1','opus',9.0,1,3600);",
            )
            .unwrap();
        }
        let (cur, more) = s.restock_rollup_advance(1000).unwrap();
        assert_eq!(cur, 3);
        assert!(!more, "已追平");
        let rows = s.restock_credit_series(0).unwrap();
        let ksk: f64 = rows.iter().filter(|r| r.ksk).map(|r| r.credits).sum();
        let other: f64 = rows.iter().filter(|r| !r.ksk).map(|r| r.credits).sum();
        assert!((ksk - 4.0).abs() < 1e-9, "ksk_ 号 1.5+2.5,实际 {ksk}");
        assert!((other - 9.0).abs() < 1e-9, "人工号归入非 ksk_,实际 {other}");

        // 再跑一次不该重复累加 —— 游标已经推到底了。
        s.restock_rollup_advance(1000).unwrap();
        let again: f64 = s.restock_credit_series(0).unwrap().iter().map(|r| r.credits).sum();
        assert!((again - 13.0).abs() < 1e-9, "重复搬运会让积分凭空翻倍,实际 {again}");
    }

    #[test]
    fn 负积分不污染汇总() {
        let s = SqliteStore::open_in_memory().unwrap();
        {
            let c = s.conn.lock();
            c.execute_batch(
                "INSERT INTO usage_records (id, account_id, model, metering_credit, success, created_at)
                 VALUES (1,'x','opus',-5.0,1,3600), (2,'x','opus',3.0,1,3600);",
            )
            .unwrap();
        }
        s.restock_rollup_advance(1000).unwrap();
        let total: f64 = s.restock_credit_series(0).unwrap().iter().map(|r| r.credits).sum();
        assert!((total - 3.0).abs() < 1e-9, "负值必须被 CASE WHEN>0 挡住,实际 {total}");
    }

    #[test]
    fn 只回收登记过的自购号() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.restock_record_owned("oid", &["a".into(), "b".into()]).unwrap();
        assert_eq!(s.restock_owned_alive().unwrap().len(), 2);
        s.restock_mark_reclaimed("a").unwrap();
        let alive = s.restock_owned_alive().unwrap();
        assert_eq!(alive.len(), 1);
        assert_eq!(alive[0].0, "b");
        assert_eq!(s.restock_accounts_of_order("oid").unwrap().len(), 2);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_absent_then_roundtrip_then_overwrite() {
        let store = SqliteStore::open_in_memory().unwrap();
        // 空库:无 overlay。
        assert_eq!(store.get_settings().unwrap(), None);
        // 写读往返。
        store.upsert_settings(r#"{"default_proxy":"socks5://h:1080"}"#).unwrap();
        assert_eq!(
            store.get_settings().unwrap().as_deref(),
            Some(r#"{"default_proxy":"socks5://h:1080"}"#)
        );
        // 二次写覆盖(不新增行)。
        store.upsert_settings(r#"{"max_failures":1}"#).unwrap();
        assert_eq!(
            store.get_settings().unwrap().as_deref(),
            Some(r#"{"max_failures":1}"#)
        );
    }

    /// 模型目录与系统设置同表不同键,**互不影响** —— 复用 settings 表是为了不动 schema,
    /// 但两个键必须完全隔离,否则写目录会把热调设置抹掉。
    #[test]
    fn model_catalog_kv_is_isolated_from_system_settings() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.upsert_settings(r#"{"max_failures":3}"#).unwrap();
        assert_eq!(store.get_kv(SqliteStore::KEY_MODEL_CATALOG).unwrap(), None);

        store
            .upsert_kv(SqliteStore::KEY_MODEL_CATALOG, r#"{"models":[]}"#)
            .unwrap();
        assert_eq!(
            store.get_kv(SqliteStore::KEY_MODEL_CATALOG).unwrap().as_deref(),
            Some(r#"{"models":[]}"#)
        );
        // 反向:写目录不得影响 system 键。
        assert_eq!(
            store.get_settings().unwrap().as_deref(),
            Some(r#"{"max_failures":3}"#),
            "写模型目录把系统设置冲掉了"
        );
        // 未知键读回 None,不是空串。
        assert_eq!(store.get_kv("nonexistent").unwrap(), None);
    }

    #[tokio::test]
    async fn authenticate_known_and_unknown_key() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.add_api_key("sk-test", Some("default")).unwrap();

        let ok = store.authenticate("sk-test").await.unwrap();
        assert!(ok.is_some());
        let ok = ok.unwrap();
        assert_eq!(ok.key_id, "sk-test");
        assert_eq!(ok.group_name, "", "新建 key 默认未分组");

        let bad = store.authenticate("sk-nope").await.unwrap();
        assert!(bad.is_none());
    }

    #[tokio::test]
    async fn authenticate_carries_group_name() {
        // router 的按组路由依赖鉴权返回 group_name:G0→kiro / DARIO→dario。
        let store = SqliteStore::open_in_memory().unwrap();
        store.add_api_key("sk-dario", Some("ccmax")).unwrap();
        let patch = ApiKeyPatch { group_name: Some("DARIO".into()), ..Default::default() };
        assert!(store.update_api_key("sk-dario", &patch).unwrap());

        let auth = store.authenticate("sk-dario").await.unwrap().unwrap();
        assert_eq!(auth.group_name, "DARIO", "鉴权必须带出 key 的分组用于路由");
    }

    #[tokio::test]
    async fn disabled_flag_reported() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.add_api_key("sk-x", None).unwrap();
        {
            let conn = store.conn.lock();
            conn.execute("UPDATE api_keys SET disabled=1 WHERE key='sk-x'", [])
                .unwrap();
        }
        let auth = store.authenticate("sk-x").await.unwrap().unwrap();
        assert!(auth.disabled);
    }

    #[tokio::test]
    async fn usage_record_persists_and_reads_back() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .record(UsageRecord {
                client_key_id: "sk-cust".into(),
                account_id: "acct-7".into(),
                model: "claude-sonnet-4-5".into(),
                input_tokens: 1200,
                output_tokens: 345,
                cache_read_tokens: 900,
                cache_creation_tokens: 64,
                real_cache_read_tokens: 0,
                metering_credit: 0.0,
                success: true,
            })
            .await
            .unwrap();

        let conn = store.conn.lock();
        let (acct, model, in_t, out_t, cache_t, cache_w, ok): (
            String,
            String,
            i64,
            i64,
            i64,
            i64,
            i64,
        ) = conn
            .query_row(
                "SELECT account_id, model, input_tokens, output_tokens, cache_read_tokens, \
                 cache_creation_tokens, success \
                 FROM usage_records WHERE client_key_id = 'sk-cust'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(acct, "acct-7");
        assert_eq!(model, "claude-sonnet-4-5");
        assert_eq!(in_t, 1200);
        assert_eq!(out_t, 345);
        assert_eq!(cache_t, 900);
        assert_eq!(cache_w, 64, "cache_creation 不得在落库时丢失");
        assert_eq!(ok, 1);
    }

    #[tokio::test]
    async fn usage_credit_and_real_cache_persist_and_aggregate() {
        let store = SqliteStore::open_in_memory().unwrap();
        for (cr, rcr, credit) in [(900u64, 100u64, 1.5f64), (300, 50, 0.5)] {
            store
                .record(UsageRecord {
                    client_key_id: "sk-cust".into(),
                    account_id: "a".into(),
                    model: "claude-opus-4-8".into(),
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: cr,
                    cache_creation_tokens: 0,
                    real_cache_read_tokens: rcr,
                    metering_credit: credit,
                    success: true,
                })
                .await
                .unwrap();
        }
        let f = UsageFilter::default();
        let s = store.usage_summary(&f).unwrap();
        assert_eq!(s.cache_read_tokens, 1200);
        assert_eq!(s.real_cache_read_tokens, 150, "真实口径缓存读须独立累计");
        assert!((s.metering_credit - 2.0).abs() < 1e-9, "积分须求和: {}", s.metering_credit);

        let by_model = store.usage_by_model(&f).unwrap();
        assert_eq!(by_model.len(), 1);
        assert!((by_model[0].metering_credit - 2.0).abs() < 1e-9);
        assert_eq!(by_model[0].real_cache_read_tokens, 150);

        let by_key = store.usage_by_key(&f).unwrap();
        assert_eq!(by_key[0].real_cache_read_tokens, 150);
        assert!((by_key[0].metering_credit - 2.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn usage_negative_credit_clamped_to_zero() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .record(UsageRecord {
                client_key_id: String::new(),
                account_id: "a".into(),
                model: "m".into(),
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                real_cache_read_tokens: 0,
                metering_credit: -3.0,
                success: true,
            })
            .await
            .unwrap();
        let s = store.usage_summary(&UsageFilter::default()).unwrap();
        assert_eq!(s.metering_credit, 0.0, "负 credit 须兜底为 0,不污染分母");
    }

    #[tokio::test]
    async fn usage_record_persists_failure_flag() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .record(UsageRecord {
                client_key_id: String::new(),
                account_id: "acct-fail".into(),
                model: "m".into(),
                input_tokens: 0,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                real_cache_read_tokens: 0,
                metering_credit: 0.0,
                success: false,
            })
            .await
            .unwrap();

        let conn = store.conn.lock();
        let ok: i64 = conn
            .query_row(
                "SELECT success FROM usage_records WHERE account_id = 'acct-fail'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(ok, 0, "失败请求的 usage 行 success 应为 0");
    }

    async fn rec(store: &SqliteStore, key: &str, model: &str, inp: u64, out: u64, ok: bool) {
        store
            .record(UsageRecord {
                client_key_id: key.into(),
                account_id: "a".into(),
                model: model.into(),
                input_tokens: inp,
                output_tokens: out,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                real_cache_read_tokens: 0,
                metering_credit: 0.0,
                success: ok,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn usage_summary_aggregates_all() {
        let store = SqliteStore::open_in_memory().unwrap();
        rec(&store, "k1", "m1", 100, 10, true).await;
        rec(&store, "k2", "m1", 200, 20, true).await;
        rec(&store, "k1", "m2", 50, 5, false).await;
        let s = store.usage_summary(&UsageFilter::default()).unwrap();
        assert_eq!(s.requests, 3);
        assert_eq!(s.success_requests, 2);
        assert_eq!(s.input_tokens, 350);
        assert_eq!(s.output_tokens, 35);
    }

    #[tokio::test]
    async fn usage_summary_filters_by_key() {
        let store = SqliteStore::open_in_memory().unwrap();
        rec(&store, "k1", "m1", 100, 10, true).await;
        rec(&store, "k1", "m2", 50, 5, true).await;
        rec(&store, "k2", "m1", 200, 20, true).await;
        let only_k1 = UsageFilter {
            client_key_id: Some("k1".into()),
            ..Default::default()
        };
        let s = store.usage_summary(&only_k1).unwrap();
        assert_eq!(s.requests, 2, "只统计 k1");
        assert_eq!(s.input_tokens, 150);
        // by-model 也应只含 k1 的模型
        let models = store.usage_by_model(&only_k1).unwrap();
        assert_eq!(models.len(), 2);
        assert!(models.iter().all(|m| m.model == "m1" || m.model == "m2"));
    }

    #[tokio::test]
    async fn usage_by_model_groups() {
        let store = SqliteStore::open_in_memory().unwrap();
        rec(&store, "k1", "m1", 100, 10, true).await;
        rec(&store, "k2", "m1", 200, 20, true).await;
        rec(&store, "k1", "m2", 50, 5, true).await;
        let rows = store.usage_by_model(&UsageFilter::default()).unwrap();
        let m1 = rows.iter().find(|r| r.model == "m1").unwrap();
        assert_eq!(m1.requests, 2);
        assert_eq!(m1.input_tokens, 300);
        let m2 = rows.iter().find(|r| r.model == "m2").unwrap();
        assert_eq!(m2.requests, 1);
    }

    #[tokio::test]
    async fn usage_by_key_groups() {
        let store = SqliteStore::open_in_memory().unwrap();
        rec(&store, "k1", "m1", 100, 10, true).await;
        rec(&store, "k1", "m2", 50, 5, false).await;
        rec(&store, "k2", "m1", 200, 20, true).await;
        let rows = store.usage_by_key(&UsageFilter::default()).unwrap();
        let k1 = rows.iter().find(|r| r.client_key_id == "k1").unwrap();
        assert_eq!(k1.requests, 2);
        assert_eq!(k1.success_requests, 1);
        assert_eq!(k1.input_tokens, 150);
        let k2 = rows.iter().find(|r| r.client_key_id == "k2").unwrap();
        assert_eq!(k2.requests, 1);
    }

    #[tokio::test]
    async fn usage_summary_respects_since_filter() {
        let store = SqliteStore::open_in_memory().unwrap();
        rec(&store, "k1", "m1", 100, 10, true).await;
        // since 在未来 → 过滤掉刚写入的行。
        let future = 9_999_999_999i64;
        let s = store
            .usage_summary(&UsageFilter {
                since_unix: Some(future),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(s.requests, 0, "since 在未来应过滤掉所有行");
    }

    // ───────── API key CRUD ─────────

    #[test]
    fn list_api_keys_returns_fields_newest_first() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.create_api_key("sk-old", Some("老客户"), None).unwrap());
        assert!(store.create_api_key("sk-new", None, None).unwrap());
        {
            // created_at 默认同秒,手动拉开以测排序。
            let conn = store.conn.lock();
            conn.execute("UPDATE api_keys SET created_at=100 WHERE key='sk-old'", [])
                .unwrap();
            conn.execute("UPDATE api_keys SET created_at=200 WHERE key='sk-new'", [])
                .unwrap();
        }
        let rows = store.list_api_keys().unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "sk-new", "新建的排前面");
        assert_eq!(rows[0].label, None);
        assert_eq!(rows[0].created_at, 200);
        assert!(!rows[0].disabled);
        assert_eq!(rows[1].key, "sk-old");
        assert_eq!(rows[1].label.as_deref(), Some("老客户"));
    }

    #[test]
    fn list_api_keys_same_second_newest_insert_first() {
        // created_at 秒级精度:同秒创建时按插入序(rowid)倒序兜底,
        // 保证"新建项在顶部"(对抗审查 Skeptic#3)。
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_api_key("sk-aa-tiebreak", None, None).unwrap();
        store.create_api_key("sk-zz-tiebreak", None, None).unwrap();
        {
            let conn = store.conn.lock();
            conn.execute("UPDATE api_keys SET created_at=500", []).unwrap();
        }
        let rows = store.list_api_keys().unwrap();
        assert_eq!(rows[0].key, "sk-zz-tiebreak", "同秒时后插入的排前面");
        assert_eq!(rows[1].key, "sk-aa-tiebreak");
    }

    #[test]
    fn create_api_key_rejects_duplicate() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.create_api_key("sk-dup", None, None).unwrap(), "首次创建成功");
        assert!(!store.create_api_key("sk-dup", Some("again"), None).unwrap(), "重复创建返回 false");
        // 重复创建不得覆盖已有行。
        let row = store.get_api_key("sk-dup").unwrap().unwrap();
        assert_eq!(row.label, None, "重复创建不能改写已有 label");
    }

    #[test]
    fn get_api_key_none_for_unknown() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.get_api_key("sk-nope").unwrap().is_none());
    }

    #[tokio::test]
    async fn update_api_key_partial_fields() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_api_key("sk-u", Some("初始"), None).unwrap();

        // 只改 disabled,label 不动。
        let only_disable = ApiKeyPatch { disabled: Some(true), ..Default::default() };
        assert!(store.update_api_key("sk-u", &only_disable).unwrap());
        let row = store.get_api_key("sk-u").unwrap().unwrap();
        assert!(row.disabled);
        assert_eq!(row.label.as_deref(), Some("初始"));
        // 禁用后鉴权应报告 disabled(router 据此拒绝)。
        assert!(store.authenticate("sk-u").await.unwrap().unwrap().disabled);

        // 只改 label,disabled 不动;空串=清空备注 → NULL(与创建路径一致,
        // 避免 ''/NULL 双态,对抗审查 Architect#5)。
        let clear_label = ApiKeyPatch { label: Some(String::new()), ..Default::default() };
        assert!(store.update_api_key("sk-u", &clear_label).unwrap());
        let row = store.get_api_key("sk-u").unwrap().unwrap();
        assert_eq!(row.label, None, "清空备注应落 NULL 而非空串");
        assert!(row.disabled, "改 label 不得动 disabled");

        // 不存在的 key 返回 false。
        let set_label = ApiKeyPatch { label: Some("x".into()), ..Default::default() };
        assert!(!store.update_api_key("sk-ghost", &set_label).unwrap());
        // 全缺省 = 存在性检查。
        assert!(store.update_api_key("sk-u", &ApiKeyPatch::default()).unwrap());
        assert!(!store.update_api_key("sk-ghost", &ApiKeyPatch::default()).unwrap());
    }

    #[tokio::test]
    async fn delete_api_key_removes_only_target() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_api_key("sk-del", None, None).unwrap();
        store.create_api_key("sk-keep", None, None).unwrap();
        // 删除前先留一条 usage,验证历史归属不被联动清除。
        rec(&store, "sk-del", "m1", 10, 1, true).await;

        assert!(store.delete_api_key("sk-del").unwrap());
        assert!(!store.delete_api_key("sk-del").unwrap(), "二次删除返回 false");
        assert!(store.authenticate("sk-del").await.unwrap().is_none(), "删除后鉴权失效");
        assert!(store.get_api_key("sk-keep").unwrap().is_some());
        // usage 历史保留(按 key 统计仍可见)。
        let by_key = store.usage_by_key(&UsageFilter::default()).unwrap();
        assert!(by_key.iter().any(|r| r.client_key_id == "sk-del"));
    }

    // ───────── 分组 CRUD ─────────

    #[test]
    fn groups_crud_lifecycle() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store.create_group("G0", "#7c6cf6", "主组").unwrap());
        assert!(!store.create_group("G0", "", "").unwrap(), "重名返回 false");
        assert!(store.create_group("G1", "", "").unwrap());

        // 计数:G0 挂 1 账号 + 2 key;G1 空。
        store
            .create_account("kiro-01", "G0", "kiro", 1, r#"{"refresh_token":"rt-1"}"#)
            .unwrap();
        store.create_api_key("sk-g0-a", None, Some("G0")).unwrap();
        store.create_api_key("sk-g0-b", None, Some("G0")).unwrap();

        let groups = store.list_groups().unwrap();
        assert_eq!(groups.len(), 2);
        let g0 = groups.iter().find(|g| g.name == "G0").unwrap();
        assert_eq!(g0.color, "#7c6cf6");
        assert_eq!(g0.note, "主组");
        assert_eq!(g0.account_count, 1);
        assert_eq!(g0.key_count, 2);
        let g1 = groups.iter().find(|g| g.name == "G1").unwrap();
        assert_eq!((g1.account_count, g1.key_count), (0, 0));

        // 部分更新:只改 color,note 不动。
        assert!(store.update_group("G0", Some("#ff0000"), None).unwrap());
        let g0 = store.list_groups().unwrap().into_iter().find(|g| g.name == "G0").unwrap();
        assert_eq!(g0.color, "#ff0000");
        assert_eq!(g0.note, "主组");
        assert!(!store.update_group("GX", Some("#000"), None).unwrap());

        // 仍有 key 绑定时**必须拒绝**:删组会把 key 的 group_name 清空,而 router 把空组名
        // 回落到主组 —— 等于把这些客户静默提权成主组的不受限访问。
        assert_eq!(store.delete_group("G0").unwrap(), DeleteGroupOutcome::HasKeys(2));
        assert!(store.get_api_key("sk-g0-a").unwrap().unwrap().group_name == "G0", "被拒的删除不得留痕");

        for k in ["sk-g0-a", "sk-g0-b"] {
            store
                .update_api_key(k, &ApiKeyPatch { group_name: Some("G1".into()), ..Default::default() })
                .unwrap();
        }
        // 仍是账号的 owner 时**也必须拒绝**:删组若顺手把归属清空,这些号会变成没有任何
        // worker 加载的孤儿,而借用它们的别的组会当场全量 503,且删的人看不出因果。
        assert_eq!(store.delete_group("G0").unwrap(), DeleteGroupOutcome::IsOwner(1));
        assert_eq!(
            store.get_account("kiro-01").unwrap().unwrap().group_name,
            "G0",
            "被拒的删除绝不能动账号归属"
        );

        // 账号迁到别的 owner 之后才允许删;成员边一并清理(不留悬空边)。
        store
            .update_account(
                "kiro-01",
                &AccountPatch { group_name: Some("G1".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!(store.delete_group("G0").unwrap(), DeleteGroupOutcome::Deleted);
        assert_eq!(store.delete_group("G0").unwrap(), DeleteGroupOutcome::NotFound, "二次删除报 NotFound");
        assert!(store.list_group_members("G0").unwrap().is_empty(), "成员边必须一并删掉");
    }

    /// 下线一个组的正确姿势:清空成员边。组还在、key 还绑着,但选不出账号 → 立即 503,
    /// 客户不会被静默提权到主组。这必须是**一步**动作,而不是 GET 一遍再发 N 次 DELETE。
    #[test]
    fn clear_members_is_the_one_step_offline_path() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_group("LOW", "", "").unwrap();
        store.create_group("G0", "", "").unwrap();
        for id in ["a", "b", "c"] {
            store.create_account(id, "G0", "kiro", 2, "{}").unwrap();
            store.upsert_membership(id, "LOW", 0).unwrap();
        }
        store.create_api_key("sk-low", None, Some("LOW")).unwrap();

        assert_eq!(store.clear_group_members("LOW").unwrap(), 3);
        assert!(store.list_group_members("LOW").unwrap().is_empty());
        // key 仍绑在组上(没有被打回未分组 → 不会回落主组拿到全部账号)。
        assert_eq!(store.get_api_key("sk-low").unwrap().unwrap().group_name, "LOW");
        // 归属不受影响:这些号还归 G0 管,G0 的客户照常用。
        assert_eq!(store.list_group_members("G0").unwrap().len(), 3);
    }

    // ───────── 账号 CRUD ─────────

    #[test]
    fn accounts_crud_lifecycle() {
        let store = SqliteStore::open_in_memory().unwrap();
        assert!(store
            .create_account("kiro-01", "G0", "kiro", 2, r#"{"refresh_token":"rt-1","priority":10}"#)
            .unwrap());
        assert!(!store
            .create_account("kiro-01", "G1", "kiro", 1, "{}")
            .unwrap(), "重复 account_id 返回 false");

        let a = store.get_account("kiro-01").unwrap().unwrap();
        assert_eq!(a.group_name, "G0");
        assert_eq!(a.provider, "kiro");
        assert_eq!(a.max_concurrency, 2);
        assert!(!a.disabled);
        assert!(a.extra.contains("rt-1"));

        // 部分更新:换组 + 禁用,extra 不动。
        let patch = AccountPatch {
            group_name: Some("G1".into()),
            disabled: Some(true),
            ..Default::default()
        };
        assert_eq!(store.update_account("kiro-01", &patch).unwrap(), UpdateAccountOutcome::Ok);
        let a = store.get_account("kiro-01").unwrap().unwrap();
        assert_eq!(a.group_name, "G1");
        assert!(a.disabled);
        assert!(a.extra.contains("rt-1"), "未指定 extra 不得改动");

        // 刷新回写:整体替换 extra(rolling refresh_token 持久化)。
        assert!(store
            .update_account_extra("kiro-01", r#"{"refresh_token":"rt-2-rolled"}"#)
            .unwrap());
        let a = store.get_account("kiro-01").unwrap().unwrap();
        assert!(a.extra.contains("rt-2-rolled"));
        assert!(!store.update_account_extra("ghost", "{}").unwrap());

        // 列表 + 删除。
        store.create_account("kiro-02", "G0", "kiro", 1, "{}").unwrap();
        assert_eq!(store.list_accounts().unwrap().len(), 2);
        assert!(store.delete_account("kiro-02").unwrap());
        assert!(!store.delete_account("kiro-02").unwrap());
        assert!(store.get_account("kiro-02").unwrap().is_none());
    }

    #[test]
    fn bump_account_counters_tallies_success_and_failure() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_account("kiro-c", "G0", "kiro", 2, "{}").unwrap();
        // 新号从 0 起算(additive 默认列)。
        let a = store.get_account("kiro-c").unwrap().unwrap();
        assert_eq!(a.success_count, 0);
        assert_eq!(a.failure_count, 0);
        // 3 成功 + 2 失败,独立累加。
        for _ in 0..3 {
            store.bump_account_counters("kiro-c", true).unwrap();
        }
        store.bump_account_counters("kiro-c", false).unwrap();
        store.bump_account_counters("kiro-c", false).unwrap();
        let a = store.get_account("kiro-c").unwrap().unwrap();
        assert_eq!(a.success_count, 3);
        assert_eq!(a.failure_count, 2);
        // list_accounts 也带上计数(前端读取路径)。
        let row = store
            .list_accounts()
            .unwrap()
            .into_iter()
            .find(|r| r.account_id == "kiro-c")
            .unwrap();
        assert_eq!(row.success_count, 3);
        assert_eq!(row.failure_count, 2);
        // 空 account_id 跳过;ghost 账号 UPDATE 影响 0 行——不报错、不建行。
        store.bump_account_counters("", true).unwrap();
        store.bump_account_counters("ghost", false).unwrap();
        assert!(store.get_account("ghost").unwrap().is_none());
    }

    #[test]
    fn merge_account_extra_keeps_unrelated_fields() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .create_account(
                "kiro-m",
                "G0",
                "kiro",
                1,
                r#"{"refresh_token":"rt-old","priority":5,"region":"us-east-1"}"#,
            )
            .unwrap();
        // 模拟刷新回写:只带变化的 token 字段。
        assert!(store
            .merge_account_extra("kiro-m", r#"{"refresh_token":"rt-new","access_token":"at-1"}"#)
            .unwrap());
        let a = store.get_account("kiro-m").unwrap().unwrap();
        assert!(a.extra.contains("rt-new"));
        assert!(a.extra.contains("at-1"));
        assert!(a.extra.contains(r#""priority":5"#), "未触及字段必须保留");
        assert!(a.extra.contains("us-east-1"), "未触及字段必须保留");
        assert!(!store.merge_account_extra("ghost", "{}").unwrap());
    }

    #[test]
    fn group_exists_checks() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_group("G0", "", "").unwrap();
        assert!(store.group_exists("G0").unwrap());
        assert!(!store.group_exists("G0-typo").unwrap());
    }

    #[test]
    fn load_owned_accounts_converts_to_runtime_account() {
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .create_account(
                "kiro-01",
                "G0",
                "kiro",
                3,
                r#"{"refresh_token":"rt-1","priority":5}"#,
            )
            .unwrap();
        store.create_account("kiro-02", "G1", "kiro", 1, "{}").unwrap();
        // 禁用的账号也要返回(调度器自己看 disabled 字段)。
        store.create_account("kiro-03", "G0", "kiro", 1, "{}").unwrap();
        store
            .update_account("kiro-03", &AccountPatch { disabled: Some(true), ..Default::default() })
            .unwrap();

        let accounts = store.load_owned_accounts("G0").unwrap();
        assert_eq!(accounts.len(), 2, "只取 G0,含禁用账号");
        let a1 = accounts.iter().find(|a| a.account_id == "kiro-01").unwrap();
        assert_eq!(a1.provider, "kiro");
        assert_eq!(a1.max_concurrency, 3);
        assert_eq!(a1.extra_str("refresh_token"), Some("rt-1"));
        assert_eq!(a1.extra.get("priority").and_then(|v| v.as_i64()), Some(5));
        let a3 = accounts.iter().find(|a| a.account_id == "kiro-03").unwrap();
        assert!(a3.disabled);
    }

    /// 暖机号龄事实源:accounts.created_at 必须原样带进运行时 Account,
    /// 丢了它(=0)暖机会对所有号失效(fail-open 到不限速)。
    #[test]
    fn load_owned_accounts_carries_created_at_for_warmup() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_account("k1", "G0", "kiro", 1, "{}").unwrap();
        let row = store.get_account("k1").unwrap().unwrap();
        let accounts = store.load_owned_accounts("G0").unwrap();
        assert_eq!(accounts.len(), 1);
        assert!(row.created_at > 0, "建库即应有建档时刻");
        assert_eq!(accounts[0].created_at, row.created_at, "created_at 必须直通");
    }

    #[test]
    fn import_accounts_yaml_idempotent() {
        let yaml = r#"
groups:
  G0:
    provider: kiro
    accounts:
      - account_id: kiro-01
        refresh_token: rt-1
      - account_id: kiro-02
        refresh_token: rt-2
  G1:
    provider: kiro
    accounts:
      - account_id: kiro-03
        refresh_token: rt-3
"#;
        let cfg: AccountsConfig = serde_yaml::from_str(yaml).unwrap();
        let store = SqliteStore::open_in_memory().unwrap();

        let n = store.import_accounts(&cfg).unwrap();
        assert_eq!(n, 3, "首次导入 3 账号");
        assert_eq!(store.list_groups().unwrap().len(), 2, "组同步建出");
        let a = store.get_account("kiro-01").unwrap().unwrap();
        assert_eq!(a.group_name, "G0");
        assert!(a.extra.contains("rt-1"), "extra 字段(refresh_token)进库");

        // 二次导入:已有行不覆盖(DB 是事实源,yaml 只是首次播种)。
        store.update_account_extra("kiro-01", r#"{"refresh_token":"rt-1-rolled"}"#).unwrap();
        let n = store.import_accounts(&cfg).unwrap();
        assert_eq!(n, 0, "幂等:无新增");
        let a = store.get_account("kiro-01").unwrap().unwrap();
        assert!(a.extra.contains("rt-1-rolled"), "导入不得回滚已 roll 的 token");
    }

    // ───────── per-key 限额 ─────────

    #[tokio::test]
    async fn quota_set_clear_and_over_quota_in_auth() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_api_key("sk-q", None, None).unwrap();

        // 无限额:over_quota = false。
        assert!(!store.authenticate("sk-q").await.unwrap().unwrap().over_quota);

        // 设限额 100:未用 → false。
        let set = ApiKeyPatch { quota_tokens: Some(100), ..Default::default() };
        assert!(store.update_api_key("sk-q", &set).unwrap());
        let row = store.get_api_key("sk-q").unwrap().unwrap();
        assert_eq!(row.quota_tokens, Some(100));
        assert!(!store.authenticate("sk-q").await.unwrap().unwrap().over_quota);

        // 消耗 120(input 100 + output 20)→ 超限。
        rec(&store, "sk-q", "m1", 100, 20, true).await;
        let row = store.get_api_key("sk-q").unwrap().unwrap();
        assert_eq!(row.used_tokens, 120, "used = input+output");
        assert!(store.authenticate("sk-q").await.unwrap().unwrap().over_quota);

        // 重置已用 → 恢复。
        let reset = ApiKeyPatch { reset_used: true, ..Default::default() };
        assert!(store.update_api_key("sk-q", &reset).unwrap());
        assert!(!store.authenticate("sk-q").await.unwrap().unwrap().over_quota);

        // 清除限额(<=0)→ NULL,不限。
        rec(&store, "sk-q", "m1", 500, 0, true).await;
        let clear = ApiKeyPatch { quota_tokens: Some(0), ..Default::default() };
        assert!(store.update_api_key("sk-q", &clear).unwrap());
        let row = store.get_api_key("sk-q").unwrap().unwrap();
        assert_eq!(row.quota_tokens, None);
        assert!(!store.authenticate("sk-q").await.unwrap().unwrap().over_quota);
    }

    #[tokio::test]
    async fn record_skips_quota_bump_for_unattributed() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_api_key("sk-other", None, None).unwrap();
        // 未归属 usage(client_key_id 空)不影响任何 key 的 used_tokens。
        rec(&store, "", "m1", 999, 1, true).await;
        assert_eq!(store.get_api_key("sk-other").unwrap().unwrap().used_tokens, 0);
    }

    #[tokio::test]
    async fn file_backed_stats_see_writes_from_main_connection() {
        // 守卫:统计查询走独立读连接后,必须仍能看到主连接的已提交写入
        // (WAL 跨连接可见性),否则 admin 看板会展示陈旧/空数据。
        let path = std::env::temp_dir().join(format!(
            "gw-store-stats-test-{}.db",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        {
            let store = SqliteStore::open(&path).unwrap();
            rec(&store, "k1", "m1", 100, 10, true).await;
            let s = store.usage_summary(&UsageFilter::default()).unwrap();
            assert_eq!(s.requests, 1, "统计读连接必须看到主连接的写入");
            assert!(store.create_api_key("sk-file-guard-1", None, None).unwrap());
            assert_eq!(store.list_api_keys().unwrap().len(), 1);
        }
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{}", path.display(), suffix));
        }
    }

    #[tokio::test]
    async fn usage_record_saturates_huge_token_count() {
        // u64 超 i64::MAX 时应饱和(不静默回绕成负数,污染计费)。
        let store = SqliteStore::open_in_memory().unwrap();
        store
            .record(UsageRecord {
                client_key_id: String::new(),
                account_id: "big".into(),
                model: "m".into(),
                input_tokens: u64::MAX,
                output_tokens: 0,
                cache_read_tokens: 0,
                cache_creation_tokens: 0,
                real_cache_read_tokens: 0,
                metering_credit: 0.0,
                success: true,
            })
            .await
            .unwrap();
        let conn = store.conn.lock();
        let in_t: i64 = conn
            .query_row(
                "SELECT input_tokens FROM usage_records WHERE account_id='big'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(in_t, i64::MAX, "超 i64 的 token 数应饱和而非回绕成负数");
    }

    fn mk_log(account: &str, model: &str, success: bool) -> RequestLog {
        RequestLog {
            client_key_id: "k1".into(),
            account_id: account.into(),
            model: model.into(),
            stream: true,
            success,
            status_code: Some(if success { 200 } else { 429 }),
            error_kind: if success { None } else { Some("rate_limited".into()) },
            duration_ms: Some(1234),
            ttfb_ms: Some(200),
            input_tokens: 10,
            output_tokens: 20,
            cache_read_tokens: 5,
            cache_creation_tokens: 0,
            reported_tokens: 999,
            real_cache_read_tokens: 3,
            metering_credit: 0.4321,
            client_payload: format!(r#"{{"model":"{model}","orig":true}}"#),
            kiro_payload: format!(r#"{{"conversationState":"for-{account}"}}"#),
            response_payload: format!(r#"{{"type":"message","role":"assistant","model":"{model}"}}"#),
            blobs: Vec::new(),
        }
    }

    fn mk_blob(hash: &str) -> LogBlob {
        LogBlob {
            hash: hash.into(),
            media_type: "image/png".into(),
            data: format!("data-of-{hash}"),
            bytes: 10,
        }
    }

    #[test]
    fn request_log_roundtrip_and_detail() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.insert_request_log(&mk_log("kiro-1", "claude-opus-4-8", true), 2000).unwrap();

        let rows = store
            .list_request_logs(&RequestLogFilter::default(), 100)
            .unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.account_id, "kiro-1");
        assert_eq!(row.model, "claude-opus-4-8");
        assert!(row.stream && row.success);
        assert_eq!(row.reported_tokens, 999);
        // 真/credit 列往返:真实命中 + Kiro 原生计费(f64)正确存读。
        assert_eq!(row.real_cache_read_tokens, 3);
        assert!((row.metering_credit - 0.4321).abs() < 1e-9);

        // 详情含完整 payload。
        let detail = store.get_request_log(row.id).unwrap().expect("应存在");
        assert!(detail.kiro_payload.contains("for-kiro-1"));
        assert!(detail.client_payload.contains("\"orig\":true"));
        // 模型回复(response_payload)gzip 往返无损。
        assert!(detail.response_payload.contains("\"role\":\"assistant\""));
        // 不存在的 id → None。
        assert!(store.get_request_log(999_999).unwrap().is_none());
    }

    #[test]
    fn request_log_blobs_dedup_and_returned_in_detail() {
        let store = SqliteStore::open_in_memory().unwrap();
        // 两条日志各引用同一张图(同 hash)→ log_blobs 只存一份(去重)。
        let mut l1 = mk_log("a", "opus", true);
        l1.blobs = vec![mk_blob("hashX"), mk_blob("hashY")];
        store.insert_request_log(&l1, 2000).unwrap();
        let mut l2 = mk_log("b", "opus", true);
        l2.blobs = vec![mk_blob("hashX")]; // 与 l1 共享 hashX
        store.insert_request_log(&l2, 2000).unwrap();

        assert_eq!(store.count_log_blobs(), 2, "hashX 去重 → 共 2 个唯一 blob");
        // 详情按 log 取回各自引用的 blob。
        let rows = store
            .list_request_logs(&RequestLogFilter::default(), 100)
            .unwrap();
        let id_l2 = rows[0].id; // 最新
        let id_l1 = rows[1].id;
        let d1 = store.get_request_log(id_l1).unwrap().unwrap();
        let mut hashes1: Vec<_> = d1.blobs.iter().map(|b| b.hash.clone()).collect();
        hashes1.sort();
        assert_eq!(hashes1, vec!["hashX", "hashY"]);
        let d2 = store.get_request_log(id_l2).unwrap().unwrap();
        assert_eq!(d2.blobs.len(), 1);
        assert_eq!(d2.blobs[0].hash, "hashX");
    }

    #[test]
    fn request_log_blob_gc_removes_orphans_on_trim() {
        let store = SqliteStore::open_in_memory().unwrap();
        // cap=1:每插一条就把更早的全裁掉,其独有 blob 应被 GC。
        let mut l1 = mk_log("a", "opus", true);
        l1.blobs = vec![mk_blob("only-in-1")];
        store.insert_request_log(&l1, 1).unwrap();
        assert_eq!(store.count_log_blobs(), 1);

        let mut l2 = mk_log("b", "opus", true);
        l2.blobs = vec![mk_blob("only-in-2")];
        store.insert_request_log(&l2, 1).unwrap(); // l1 被裁,only-in-1 应被 GC
        assert_eq!(store.count_log_blobs(), 1, "孤儿 blob 应随裁剪清掉");
        let rows = store
            .list_request_logs(&RequestLogFilter::default(), 100)
            .unwrap();
        assert_eq!(rows.len(), 1);
        let d = store.get_request_log(rows[0].id).unwrap().unwrap();
        assert_eq!(d.blobs.len(), 1);
        assert_eq!(d.blobs[0].hash, "only-in-2");
    }

    #[test]
    fn request_log_payload_gzip_roundtrip_full_text() {
        let store = SqliteStore::open_in_memory().unwrap();
        // 超过旧 512KiB 截断阈值的大报文:gzip 全文存储,详情应原样取回(不截断)。
        let big_text = "中文长正文 abc ".repeat(60_000); // ~ 数百 KB
        let mut log = mk_log("a", "opus", true);
        log.client_payload = format!(r#"{{"messages":[{{"role":"user","content":"{big_text}"}}]}}"#);
        store.insert_request_log(&log, 2000).unwrap();
        let rows = store.list_request_logs(&RequestLogFilter::default(), 50).unwrap();
        let d = store.get_request_log(rows[0].id).unwrap().unwrap();
        assert_eq!(d.client_payload, log.client_payload, "大报文应 gzip 全文无损取回");
        assert!(d.client_payload.len() > 512 * 1024, "确实超过旧截断阈值");
    }

    #[test]
    fn ungzip_handles_legacy_plaintext_and_gzip() {
        // 旧明文(无 gzip magic)原样返回;gzip 字节正确解压。
        assert_eq!(ungzip_text(b"{\"plain\":true}".to_vec()), "{\"plain\":true}");
        let gz = gzip_text("hello gzip");
        assert!(gz.len() >= 2 && gz[0] == 0x1f && gz[1] == 0x8b, "应是 gzip 字节");
        assert_eq!(ungzip_text(gz), "hello gzip");
    }

    #[test]
    fn request_log_pagination_offset_and_count() {
        let store = SqliteStore::open_in_memory().unwrap();
        for i in 0..5 {
            store
                .insert_request_log(&mk_log(&format!("acc{i}"), "m", true), 2000)
                .unwrap();
        }
        // 总数不受 limit/offset 影响。
        assert_eq!(store.count_request_logs(&RequestLogFilter::default()).unwrap(), 5);
        // 第一页(limit 2, offset 0):最新两条 acc4/acc3(id 降序)。
        let p1 = store
            .list_request_logs(&RequestLogFilter { limit: 2, offset: 0, ..Default::default() }, 50)
            .unwrap();
        assert_eq!(p1.iter().map(|r| r.account_id.as_str()).collect::<Vec<_>>(), vec!["acc4", "acc3"]);
        // 第二页(offset 2):acc2/acc1。
        let p2 = store
            .list_request_logs(&RequestLogFilter { limit: 2, offset: 2, ..Default::default() }, 50)
            .unwrap();
        assert_eq!(p2.iter().map(|r| r.account_id.as_str()).collect::<Vec<_>>(), vec!["acc2", "acc1"]);
        // 末页越界(offset 4, limit 2):仅 acc0 一条。
        let p3 = store
            .list_request_logs(&RequestLogFilter { limit: 2, offset: 4, ..Default::default() }, 50)
            .unwrap();
        assert_eq!(p3.len(), 1);
        assert_eq!(p3[0].account_id, "acc0");
    }

    #[test]
    fn request_log_ring_trim_keeps_latest_cap() {
        let store = SqliteStore::open_in_memory().unwrap();
        for i in 0..5 {
            store
                .insert_request_log(&mk_log(&format!("acc{i}"), "m", true), 3)
                .unwrap();
        }
        let rows = store
            .list_request_logs(&RequestLogFilter::default(), 100)
            .unwrap();
        // 只保留最新 3 条(acc2/acc3/acc4),id 降序。
        assert_eq!(rows.len(), 3);
        let accounts: Vec<_> = rows.iter().map(|r| r.account_id.as_str()).collect();
        assert_eq!(accounts, vec!["acc4", "acc3", "acc2"]);
    }

    #[test]
    fn request_log_filters_by_account_model_success() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.insert_request_log(&mk_log("a", "opus", true), 2000).unwrap();
        store.insert_request_log(&mk_log("a", "sonnet", false), 2000).unwrap();
        store.insert_request_log(&mk_log("b", "opus", true), 2000).unwrap();

        let by_account = store
            .list_request_logs(
                &RequestLogFilter { account_id: Some("a".into()), ..Default::default() },
                100,
            )
            .unwrap();
        assert_eq!(by_account.len(), 2);

        let by_model = store
            .list_request_logs(
                &RequestLogFilter { model: Some("opus".into()), ..Default::default() },
                100,
            )
            .unwrap();
        assert_eq!(by_model.len(), 2);

        let failures = store
            .list_request_logs(
                &RequestLogFilter { success: Some(false), ..Default::default() },
                100,
            )
            .unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].model, "sonnet");
        assert_eq!(failures[0].error_kind.as_deref(), Some("rate_limited"));

        // limit 生效。
        let limited = store
            .list_request_logs(&RequestLogFilter { limit: 1, ..Default::default() }, 100)
            .unwrap();
        assert_eq!(limited.len(), 1);
    }

    // ───────── 分组归属与鉴权 ─────────

    /// 存量库热升级:旧 schema(无 account_groups 表)+ 已有账号 → 建表流程补表并回填,
    /// 且 groups 元数据原样保留。生产库是老库,这条挂了就等于上线即炸。
    #[test]
    fn schema_upgrade_creates_membership_table_on_legacy_db() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE groups (name TEXT PRIMARY KEY, color TEXT NOT NULL DEFAULT '', \
             note TEXT NOT NULL DEFAULT '', created_at INTEGER NOT NULL DEFAULT 0);
             INSERT INTO groups (name, color, note) VALUES ('G0', '#111', '主组');
             CREATE TABLE accounts (account_id TEXT PRIMARY KEY, group_name TEXT NOT NULL DEFAULT '', \
             provider TEXT NOT NULL DEFAULT 'kiro', max_concurrency INTEGER NOT NULL DEFAULT 2, \
             disabled INTEGER NOT NULL DEFAULT 0, extra TEXT NOT NULL DEFAULT '{}', \
             created_at INTEGER NOT NULL DEFAULT 0);
             INSERT INTO accounts (account_id, group_name, extra) \
             VALUES ('power', 'G0', '{\"priority\":0}'), ('promax', 'G0', '{\"priority\":100}');",
        )
        .unwrap();
        SqliteStore::setup_schema(&conn).unwrap();
        let color: String = conn
            .query_row("SELECT color FROM groups WHERE name='G0'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(color, "#111", "升级不得动既有组元数据");
        let mut stmt = conn
            .prepare("SELECT account_id, priority FROM account_groups WHERE group_name='G0' ORDER BY 1")
            .unwrap();

        let rows: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows,
            vec![("power".to_string(), 0), ("promax".to_string(), 100)],
            "回填后组内排序必须与升级前的 extra.priority 一致"
        );
    }

    /// suspend 生命周期往返:条件写(含退役置 disabled)→ 读回 → 人工恢复
    /// (epoch 递增)→ 旧 epoch / 同 epoch 旧 revision 的写入被拒(竞态不落旧状态)。
    #[test]
    fn suspend_lifecycle_roundtrip_and_restore() {
        let s = SqliteStore::open_in_memory().unwrap();
        s.create_account("a", "G0", "kiro", 2, "{}").unwrap();
        let lc = gw_core::store::SuspendLifecycle {
            suspend_streak: 2,
            reason: Some("temporarily_suspended".into()),
            retry_at: Some(1_800_000_000),
            epoch: 0,
            revision: 1,
        };
        assert!(s.persist_suspend_lifecycle("a", &lc, false).unwrap());
        let m = s.load_suspend_lifecycles().unwrap();
        assert_eq!(m.get("a"), Some(&lc));
        // 同 epoch 的**乱序**写:rev 3 先落,rev 2 后到必须落败(二轮阻断#1)。
        let newer = gw_core::store::SuspendLifecycle { revision: 3, ..lc.clone() };
        assert!(s.persist_suspend_lifecycle("a", &newer, false).unwrap());
        let stale = gw_core::store::SuspendLifecycle { revision: 2, ..lc.clone() };
        assert!(!s.persist_suspend_lifecycle("a", &stale, false).unwrap());
        assert_eq!(
            s.load_suspend_lifecycles().unwrap().get("a").unwrap().revision,
            3,
            "同 epoch 旧 revision 不得覆盖新状态"
        );
        // 退役:覆盖为 retired 且同事务置 disabled=1。
        let retired = gw_core::store::SuspendLifecycle {
            suspend_streak: 3,
            reason: Some("suspended_retired".into()),
            retry_at: None,
            epoch: 0,
            revision: 4,
        };
        assert!(s.persist_suspend_lifecycle("a", &retired, true).unwrap());
        assert!(s.get_account("a").unwrap().unwrap().disabled, "退役必须置 disabled=1");
        // 人工恢复:disabled=0 + 行清零 + epoch 递增、revision 归零。
        assert!(s.restore_account("a").unwrap());
        assert!(!s.get_account("a").unwrap().unwrap().disabled);
        let m = s.load_suspend_lifecycles().unwrap();
        let row = m.get("a").expect("恢复写清零行而不是删行");
        assert_eq!(row.suspend_streak, 0);
        assert_eq!(row.reason, None);
        assert_eq!(row.retry_at, None);
        assert_eq!(row.epoch, 1, "恢复必须递增 epoch");
        assert_eq!(row.revision, 0, "恢复必须归零 revision");
        // 竞态:worker 队列里的旧状态写不回(且不会重新置 disabled)。
        assert!(!s.persist_suspend_lifecycle("a", &retired, true).unwrap());
        assert!(!s.get_account("a").unwrap().unwrap().disabled, "落败的退役写不得反写 disabled");
        assert_eq!(s.load_suspend_lifecycles().unwrap().get("a").unwrap().epoch, 1);
        // 恢复后的新转换(epoch=1, rev=1)正常生效,且读回 epoch 确实前进(三轮阻断:
        // SET 漏 epoch=excluded.epoch 时,这里读回的仍是旧 epoch)。
        let next = gw_core::store::SuspendLifecycle { epoch: 1, revision: 1, ..lc.clone() };
        assert!(s.persist_suspend_lifecycle("a", &next, false).unwrap());
        let row = s.load_suspend_lifecycles().unwrap().remove("a").unwrap();
        assert_eq!((row.epoch, row.revision), (1, 1), "更大 epoch 写后读回必须前进");
        // 旧 epoch 的高 revision 写在 epoch 前进后必败(含 set_disabled)。
        let stale_hi_rev = gw_core::store::SuspendLifecycle { epoch: 0, revision: 99, ..retired.clone() };
        assert!(!s.persist_suspend_lifecycle("a", &stale_hi_rev, true).unwrap());
        assert!(!s.get_account("a").unwrap().unwrap().disabled);
        // 删除后 restore 不得重插生命周期行(三轮中#2)。
        // 删号:生命周期行同事务删除;之后迟到的写不得重插孤儿行。
        assert!(s.delete_account("a").unwrap());
        assert!(s.load_suspend_lifecycles().unwrap().get("a").is_none());
        assert!(!s.persist_suspend_lifecycle("a", &next, false).unwrap(),
            "账号已删,迟到写不得重插孤儿行");
        assert!(!s.restore_account("a").unwrap(), "账号已删,恢复必须 false");
        assert!(s.load_suspend_lifecycles().unwrap().get("a").is_none(),
            "删除后 restore 不得重插生命周期行");
    }


    /// key 未分组 / 分组行不存在时**鉴权不得失败**(写成 JOIN 会让这类 key 直接 401,
    /// 是最容易踩的回归)。
    #[tokio::test]
    async fn authenticate_tolerates_missing_group() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.add_api_key("sk-nogroup", None).unwrap();
        let a = store.authenticate("sk-nogroup").await.unwrap().unwrap();
        assert_eq!(a.group_name, "", "未分组 key 照常通过鉴权");

        // 分组名指向一个并不存在的组行(历史脏数据):同样不得报错。
        store
            .update_api_key(
                "sk-nogroup",
                &ApiKeyPatch { group_name: Some("GHOST".into()), ..Default::default() },
            )
            .unwrap();
        let a = store.authenticate("sk-nogroup").await.unwrap().unwrap();
        assert_eq!(a.group_name, "GHOST", "悬空组名不得让鉴权失败");
    }

    // ───────── 成员边(账号↔分组 N:M) ─────────

    /// 建号即建边。**没有成员边的号对所有客户都不可见** —— 号在 accounts 表里躺着、
    /// 导入日志一切正常,却永远不会被选中,是最难查的一类"配了没生效"。
    #[test]
    fn creating_account_also_creates_owner_membership() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_group("G0", "", "").unwrap();
        store
            .create_account("a", "G0", "kiro", 2, r#"{"priority":0}"#)
            .unwrap();
        store.create_account("b", "G0", "kiro", 2, "{}").unwrap();
        assert_eq!(
            store.list_group_members("G0").unwrap(),
            vec![("a".to_string(), 0), ("b".to_string(), 100)],
            "组内优先级取自 extra.priority,缺省 100(与调度器历史默认一致)"
        );
    }

    /// 同一个号在两个组里可以排到**不同的层** —— 这正是旧模型(priority 挂在账号上)
    /// 做不到、进而逼出影子组和档位区间的那件事。
    #[test]
    fn same_account_ranks_differently_per_group() {
        let store = SqliteStore::open_in_memory().unwrap();
        for g in ["G0", "LOW"] {
            store.create_group(g, "", "").unwrap();
        }
        store
            .create_account("power", "G0", "kiro", 2, r#"{"priority":0}"#)
            .unwrap();
        store
            .create_account("promax", "G0", "kiro", 2, r#"{"priority":100}"#)
            .unwrap();
        // 低价组:小号当主力(0)、主力号当兜底(100)—— 与 G0 完全相反。
        assert_eq!(store.upsert_membership("promax", "LOW", 0).unwrap(), MembershipOutcome::Ok);
        assert_eq!(store.upsert_membership("power", "LOW", 100).unwrap(), MembershipOutcome::Ok);

        let views = store.load_group_memberships("G0").unwrap();
        assert_eq!(views["G0"], HashMap::from([("power".into(), 0), ("promax".into(), 100)]));
        assert_eq!(views["LOW"], HashMap::from([("promax".into(), 0), ("power".into(), 100)]));
        // 反向:两张视图的排序必须真的相反,而不是恰好都一样。
        assert_ne!(views["G0"]["power"], views["LOW"]["power"]);
    }

    /// upsert 是"存在即改优先级";悬空边(账号或组不存在)必须写不进去 ——
    /// 悬空边不报错,只会让该组静默少一个号,比立即失败难查得多。
    #[test]
    fn membership_upsert_updates_and_rejects_dangling() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_group("G0", "", "").unwrap();
        store.create_account("a", "G0", "kiro", 2, "{}").unwrap();

        assert_eq!(store.upsert_membership("a", "G0", 7).unwrap(), MembershipOutcome::Ok);
        assert_eq!(store.list_group_members("G0").unwrap(), vec![("a".to_string(), 7)]);

        assert_eq!(store.upsert_membership("ghost", "G0", 0).unwrap(), MembershipOutcome::MissingAccountOrGroup, "账号不存在不得建边");
        assert_eq!(store.upsert_membership("a", "NOPE", 0).unwrap(), MembershipOutcome::MissingAccountOrGroup, "分组不存在不得建边");
        assert_eq!(store.list_group_members("G0").unwrap().len(), 1, "被拒的写入不得留痕");

        assert!(store.remove_membership("a", "G0").unwrap());
        assert!(!store.remove_membership("a", "G0").unwrap(), "重复删返回 false");
        assert!(store.list_group_members("G0").unwrap().is_empty());
    }

    /// admin 账号列表页用的**配置态全景**:账号与成员边同一快照,组名升序保证展示稳定。
    #[test]
    fn list_accounts_with_memberships_pairs_every_account_with_its_edges() {
        let store = SqliteStore::open_in_memory().unwrap();
        // 建组顺序故意与字典序相反,证明返回顺序来自 ORDER BY 而不是插入顺序。
        for g in ["G0", "ZED", "GLOW", "GECO"] {
            store.create_group(g, "", "").unwrap();
        }
        store.create_account("multi", "G0", "kiro", 2, "{}").unwrap();
        store.create_account("lonely", "G0", "kiro", 2, "{}").unwrap();
        // 建边同样按反字典序写入。
        store.upsert_membership("multi", "ZED", 100).unwrap();
        store.upsert_membership("multi", "GLOW", 0).unwrap();
        store.upsert_membership("multi", "GECO", 100).unwrap();

        let edges_of = |store: &SqliteStore, id: &str| {
            store
                .list_accounts_with_memberships()
                .unwrap()
                .into_iter()
                .find(|(row, _)| row.account_id == id)
                .map(|(_, edges)| edges)
        };

        assert_eq!(
            edges_of(&store, "multi").unwrap(),
            vec![
                ("G0".to_string(), 100), // create_account 自动建的归属边
                ("GECO".to_string(), 100),
                ("GLOW".to_string(), 0),
                ("ZED".to_string(), 100),
            ],
            "同一账号的边按组名升序,且组内优先级逐条对上"
        );

        // 归属边是 create_account 自动建的,所以"没有额外成员边"的号仍有 1 条。
        assert_eq!(edges_of(&store, "lonely").unwrap(), vec![("G0".to_string(), 100)]);

        // 反向断言:删掉一条边后,那个组不得再出现在该账号的列表里。
        assert!(store.remove_membership("multi", "GLOW").unwrap());
        let after = edges_of(&store, "multi").unwrap();
        assert!(
            !after.iter().any(|(g, _)| g == "GLOW"),
            "删边后 GLOW 不得再出现,否则前端会显示一个已经不存在的成员关系"
        );
        assert_eq!(after.len(), 3);

        // 一条边都没有的账号:**行仍在**,边是空 vec —— 前端据此显示红色"无分组"。
        // 这正是 2026-07-29 那种"号在库里、不在任何组、谁也用不到"的形态。
        store.create_account("orphan", "", "kiro", 2, "{}").unwrap();
        assert_eq!(
            edges_of(&store, "orphan"),
            Some(vec![]),
            "未分组账号必须照样出现在列表里,只是边为空;漏掉这行运维就永远看不见这个号"
        );
    }

    /// 改**归属**是从边的另一头破坏"一组一 owner":边一条没动,却把边另一端的 owner
    /// 换了。`upsert_membership` 拦不住这条路径,必须在 `update_account` 里拦。
    #[test]
    fn changing_owner_that_would_split_a_group_is_rejected() {
        let store = SqliteStore::open_in_memory().unwrap();
        // 归属组叫 OWN(而不是复用 G0),这样"共享的成员组"与"归属组"是两个东西,
        // 测的就是纯粹的成员边冲突。冲突组按组名升序取第一个 → LOW < OWN。
        for g in ["OWN", "G1", "LOW"] {
            store.create_group(g, "", "").unwrap();
        }
        store.create_account("a", "OWN", "kiro", 2, "{}").unwrap();
        store.create_account("b", "OWN", "kiro", 2, "{}").unwrap();
        // a 与 b 同属 LOW,且都归属 OWN —— 合法。
        store.upsert_membership("a", "LOW", 0).unwrap();
        store.upsert_membership("b", "LOW", 100).unwrap();

        // 只改 a 的归属、一条边都不动 → 必须被拒,否则 LOW 立刻横跨 OWN/G1 两个 owner。
        let outcome = store
            .update_account(
                "a",
                &AccountPatch { group_name: Some("G1".into()), ..Default::default() },
            )
            .unwrap();
        assert_eq!(
            outcome,
            UpdateAccountOutcome::CrossOwner {
                group: "LOW".into(),
                existing: "OWN".into(),
                incoming: "G1".into(),
            }
        );
        assert_eq!(
            store.get_account("a").unwrap().unwrap().group_name,
            "OWN",
            "被拒的更新绝不能留下痕迹"
        );

        // 反向断言一:把 a 的共享边全部拆掉之后,改归属就该放行。
        assert!(store.remove_membership("a", "LOW").unwrap());
        assert!(store.remove_membership("a", "OWN").unwrap());
        assert_eq!(
            store
                .update_account(
                    "a",
                    &AccountPatch { group_name: Some("G1".into()), ..Default::default() }
                )
                .unwrap(),
            UpdateAccountOutcome::Ok
        );

        // 反向断言二:不碰归属的 patch 不受这条校验影响。
        assert_eq!(
            store
                .update_account("b", &AccountPatch { disabled: Some(true), ..Default::default() })
                .unwrap(),
            UpdateAccountOutcome::Ok
        );
    }

    /// worker 只拿**自己名下**账号的边:跨 owner 的号由对应 worker 各自持有,
    /// 谁也不越界管别人的号(单一持有者约束)。
    #[test]
    fn memberships_are_scoped_to_owner() {
        let store = SqliteStore::open_in_memory().unwrap();
        for g in ["G0", "G1", "LOW", "OTHER"] {
            store.create_group(g, "", "").unwrap();
        }
        store.create_account("mine", "G0", "kiro", 2, "{}").unwrap();
        store.create_account("theirs", "G1", "kiro", 2, "{}").unwrap();
        store.upsert_membership("mine", "LOW", 0).unwrap();
        store.upsert_membership("theirs", "OTHER", 0).unwrap();

        // 每个 worker 只拿自己名下账号的边:别人的组连出现都不该出现。
        let g0 = store.load_group_memberships("G0").unwrap();
        assert_eq!(g0["LOW"], HashMap::from([("mine".into(), 0)]));
        assert!(!g0.contains_key("OTHER"), "不得看到别的 owner 的组");
        let g1 = store.load_group_memberships("G1").unwrap();
        assert_eq!(g1["OTHER"], HashMap::from([("theirs".into(), 0)]));
        assert!(!g1.contains_key("LOW"));
    }

    /// **一个组的成员必须同属一个 owner**。跨 owner 时组内 priority 不再是全局排序:
    /// router 只按会话数选 worker,被选中的 worker 只看得见自己那部分成员,可能直接用
    /// 兜底层而另一 owner 的主力号闲着 —— "小号优先、压满才溢出"当场失效。
    #[test]
    fn group_members_must_share_one_owner() {
        let store = SqliteStore::open_in_memory().unwrap();
        for g in ["G0", "G1", "LOW"] {
            store.create_group(g, "", "").unwrap();
        }
        store.create_account("a", "G0", "kiro", 2, "{}").unwrap();
        store.create_account("b", "G1", "kiro", 2, "{}").unwrap();

        assert_eq!(store.upsert_membership("a", "LOW", 0).unwrap(), MembershipOutcome::Ok);
        assert_eq!(
            store.upsert_membership("b", "LOW", 100).unwrap(),
            MembershipOutcome::CrossOwner { existing: "G0".into(), incoming: "G1".into() },
            "第二个 owner 的号必须被拒"
        );
        assert_eq!(store.list_group_members("LOW").unwrap(), vec![("a".to_string(), 0)],
            "被拒的写入不得留痕");

        // 同 owner 的第二个号照常可加 —— 护栏不是把组锁成单成员。
        store.create_account("a2", "G0", "kiro", 2, "{}").unwrap();
        assert_eq!(store.upsert_membership("a2", "LOW", 100).unwrap(), MembershipOutcome::Ok);

        // 批量同理:筛出的号跨了 owner 就**整批**拒绝,不做部分写入。
        assert_eq!(
            store.bulk_add_members("LOW", None, None, 0).unwrap(),
            Err(MembershipOutcome::CrossOwner { existing: "G0".into(), incoming: "G1".into() })
        );
        assert_eq!(store.list_group_members("LOW").unwrap().len(), 2, "整批拒绝不得半写");
    }

    /// 220 个号手工点不现实,批量按条件加成员必须能用;且筛选维度要真的起作用。
    #[test]
    fn bulk_add_members_filters_by_subscription_and_owner() {
        let store = SqliteStore::open_in_memory().unwrap();
        for g in ["G0", "G1", "LOW"] {
            store.create_group(g, "", "").unwrap();
        }
        store
            .create_account("p1", "G0", "kiro", 2, r#"{"subscription_title":"KIRO POWER"}"#)
            .unwrap();
        store
            .create_account("m1", "G0", "kiro", 2, r#"{"subscription_title":"KIRO PRO MAX"}"#)
            .unwrap();
        store
            .create_account("m2", "G1", "kiro", 2, r#"{"subscription_title":"KIRO PRO MAX"}"#)
            .unwrap();

        // 只要 G0 名下的 PRO MAX:订阅维度和 owner 维度都必须生效。
        let n = store
            .bulk_add_members("LOW", Some("G0"), Some("KIRO PRO MAX"), 0)
            .unwrap();
        assert_eq!(n, Ok(1));
        assert_eq!(store.list_group_members("LOW").unwrap(), vec![("m1".to_string(), 0)]);

        // 不筛订阅 → G0 名下两个号都进来,且已存在的边被改成新优先级。
        store.bulk_add_members("LOW", Some("G0"), None, 55).unwrap().unwrap();
        assert_eq!(
            store.list_group_members("LOW").unwrap(),
            vec![("m1".to_string(), 55), ("p1".to_string(), 55)]
        );
        assert_eq!(
            store.bulk_add_members("NOPE", None, None, 0).unwrap(),
            Err(MembershipOutcome::MissingAccountOrGroup),
            "组不存在应明确报错,而不是静默 0 边"
        );
    }

    /// 老库升级:回填必须让新模型的起点与旧行为**逐条等价**(每个号在原组、
    /// 优先级还是 extra.priority),且**幂等 + 只补不覆盖** —— 否则运维手工调过的
    /// 组内优先级会在下次重启时被账号上的旧值悄悄冲掉。
    #[test]
    fn backfill_reproduces_legacy_layout_and_never_overwrites() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_group("G0", "", "").unwrap();
        {
            // 模拟老库:账号在,但成员边表是空的。
            let conn = store.conn.lock();
            conn.execute("DELETE FROM account_groups", []).unwrap();
            conn.execute(
                "INSERT INTO accounts (account_id, group_name, extra) VALUES \
                 ('power','G0','{\"priority\":0}'), ('promax','G0','{\"priority\":100}'), \
                 ('nopri','G0','{}'), ('orphan','','{}')",
                [],
            )
            .unwrap();
            SqliteStore::backfill_account_groups(&conn).unwrap();
        }
        assert_eq!(
            store.list_group_members("G0").unwrap(),
            vec![("power".to_string(), 0), ("nopri".to_string(), 100), ("promax".to_string(), 100)],
            "回填后每个号在原组、优先级沿用 extra.priority(缺省 100)"
        );
        assert!(
            !store.list_group_members("G0").unwrap().iter().any(|(id, _)| id == "orphan"),
            "未分组的号不该被凭空塞进任何组"
        );

        // 运维手工把 promax 提成主力,再触发一次回填(重启)——不得被冲回 100。
        store.upsert_membership("promax", "G0", 0).unwrap();
        {
            let conn = store.conn.lock();
            SqliteStore::backfill_account_groups(&conn).unwrap();
        }
        assert_eq!(
            store.list_group_members("G0").unwrap().iter().find(|(id, _)| id == "promax"),
            Some(&("promax".to_string(), 0)),
            "回填只补不覆盖:重启不得冲掉手工调整"
        );
    }

}
