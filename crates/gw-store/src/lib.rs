//! gw-store —— SQLite(WAL) 持久化。
//!
//! Phase 0:控制面最小实现(api_keys 表 + 鉴权)。WAL 模式以支持
//! 多进程(router + 多 worker)并发读、控制面写少。
//! usage / 状态机 / request_cache 在 P2/P4 按 IMPROVEMENTS.md 补。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use gw_core::store::{
    AuthenticatedKey, ControlStore, UsageByKey, UsageByModel, UsageRecord, UsageSink, UsageSummary,
};
use parking_lot::Mutex;
use rusqlite::Connection;

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
    success       INTEGER NOT NULL DEFAULT 1,
    created_at    INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
-- 时间序(全局/按模型时间窗聚合的基础);account/client 维度各带 created_at 便于钉维度后排序。
CREATE INDEX IF NOT EXISTS idx_usage_created  ON usage_records(created_at);
CREATE INDEX IF NOT EXISTS idx_usage_account ON usage_records(account_id, created_at);
CREATE INDEX IF NOT EXISTS idx_usage_client  ON usage_records(client_key_id, created_at);
"#;

/// SQLite 控制面存储。
///
/// 连接用 Mutex 包裹:Phase 0 控制面访问量极低,单连接足够;
/// P4 若热点化再换连接池。WAL 模式允许跨进程并发读。
pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

impl SqliteStore {
    /// 打开(或创建)数据库,启用 WAL,建表。
    pub fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        // WAL:多进程并发读友好;NORMAL 同步在 WAL 下安全且快。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    /// 内存库(测试用)。
    pub fn open_in_memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
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

    // ───────── 用量统计查询(admin 看板;`since_unix`=None 表示全部) ─────────

    /// 用量总览。`since_unix` = 仅统计 created_at >= 该 Unix 秒的行(None=全部)。
    pub fn usage_summary(&self, since_unix: Option<i64>) -> anyhow::Result<UsageSummary> {
        let since = since_unix.unwrap_or(0);
        let conn = self.conn.lock();
        let s = conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(success),0), COALESCE(SUM(input_tokens),0), \
             COALESCE(SUM(output_tokens),0), COALESCE(SUM(cache_read_tokens),0), \
             COALESCE(SUM(cache_creation_tokens),0) \
             FROM usage_records WHERE created_at >= ?1",
            [since],
            |r| {
                Ok(UsageSummary {
                    requests: r.get::<_, i64>(0)? as u64,
                    success_requests: r.get::<_, i64>(1)? as u64,
                    input_tokens: r.get::<_, i64>(2)? as u64,
                    output_tokens: r.get::<_, i64>(3)? as u64,
                    cache_read_tokens: r.get::<_, i64>(4)? as u64,
                    cache_creation_tokens: r.get::<_, i64>(5)? as u64,
                })
            },
        )?;
        Ok(s)
    }

    /// 按模型聚合(请求数降序)。
    pub fn usage_by_model(&self, since_unix: Option<i64>) -> anyhow::Result<Vec<UsageByModel>> {
        let since = since_unix.unwrap_or(0);
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT model, COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), \
             COALESCE(SUM(cache_read_tokens),0), COALESCE(SUM(cache_creation_tokens),0) \
             FROM usage_records WHERE created_at >= ?1 GROUP BY model ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt
            .query_map([since], |r| {
                Ok(UsageByModel {
                    model: r.get(0)?,
                    requests: r.get::<_, i64>(1)? as u64,
                    input_tokens: r.get::<_, i64>(2)? as u64,
                    output_tokens: r.get::<_, i64>(3)? as u64,
                    cache_read_tokens: r.get::<_, i64>(4)? as u64,
                    cache_creation_tokens: r.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 按客户 apikey(client_key_id)聚合(请求数降序)。
    pub fn usage_by_key(&self, since_unix: Option<i64>) -> anyhow::Result<Vec<UsageByKey>> {
        let since = since_unix.unwrap_or(0);
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT client_key_id, COUNT(*), COALESCE(SUM(success),0), \
             COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), \
             COALESCE(SUM(cache_read_tokens),0), COALESCE(SUM(cache_creation_tokens),0) \
             FROM usage_records WHERE created_at >= ?1 GROUP BY client_key_id ORDER BY COUNT(*) DESC",
        )?;
        let rows = stmt
            .query_map([since], |r| {
                Ok(UsageByKey {
                    client_key_id: r.get(0)?,
                    requests: r.get::<_, i64>(1)? as u64,
                    success_requests: r.get::<_, i64>(2)? as u64,
                    input_tokens: r.get::<_, i64>(3)? as u64,
                    output_tokens: r.get::<_, i64>(4)? as u64,
                    cache_read_tokens: r.get::<_, i64>(5)? as u64,
                    cache_creation_tokens: r.get::<_, i64>(6)? as u64,
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
        let mut stmt = conn.prepare_cached("SELECT key, disabled FROM api_keys WHERE key = ?1")?;
        let row = stmt
            .query_row([api_key], |r| {
                Ok(AuthenticatedKey {
                    key_id: r.get::<_, String>(0)?,
                    disabled: r.get::<_, i64>(1)? != 0,
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
              cache_read_tokens, cache_creation_tokens, success) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                usage.client_key_id,
                usage.account_id,
                usage.model,
                clamp_i64(usage.input_tokens),
                clamp_i64(usage.output_tokens),
                clamp_i64(usage.cache_read_tokens),
                clamp_i64(usage.cache_creation_tokens),
                usage.success as i64,
            ],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn authenticate_known_and_unknown_key() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.add_api_key("sk-test", Some("default")).unwrap();

        let ok = store.authenticate("sk-test").await.unwrap();
        assert!(ok.is_some());
        assert_eq!(ok.unwrap().key_id, "sk-test");

        let bad = store.authenticate("sk-nope").await.unwrap();
        assert!(bad.is_none());
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
        let s = store.usage_summary(None).unwrap();
        assert_eq!(s.requests, 3);
        assert_eq!(s.success_requests, 2);
        assert_eq!(s.input_tokens, 350);
        assert_eq!(s.output_tokens, 35);
    }

    #[tokio::test]
    async fn usage_by_model_groups() {
        let store = SqliteStore::open_in_memory().unwrap();
        rec(&store, "k1", "m1", 100, 10, true).await;
        rec(&store, "k2", "m1", 200, 20, true).await;
        rec(&store, "k1", "m2", 50, 5, true).await;
        let rows = store.usage_by_model(None).unwrap();
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
        let rows = store.usage_by_key(None).unwrap();
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
        let s = store.usage_summary(Some(future)).unwrap();
        assert_eq!(s.requests, 0, "since 在未来应过滤掉所有行");
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
}
