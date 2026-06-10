//! gw-store —— SQLite(WAL) 持久化。
//!
//! Phase 0:控制面最小实现(api_keys 表 + 鉴权)。WAL 模式以支持
//! 多进程(router + 多 worker)并发读、控制面写少。
//! usage / 状态机 / request_cache 在 P2/P4 按 IMPROVEMENTS.md 补。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use gw_core::store::{
    ApiKeyRow, AuthenticatedKey, ControlStore, UsageByKey, UsageByModel, UsageFilter, UsageRecord,
    UsageSink, UsageSummary,
};
use rusqlite::types::Value;
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
        // WAL:多进程并发读友好;NORMAL 同步在 WAL 下安全且快。
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(SCHEMA)?;
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
        conn.execute_batch(SCHEMA)?;
        let conn = Arc::new(Mutex::new(conn));
        Ok(Self {
            stats_conn: conn.clone(),
            conn,
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

    // ───────── API key CRUD(admin 管理页) ─────────

    /// 列出全部客户端 API key(created_at 倒序;同秒按 rowid 倒序兜底,
    /// 保证"新建项在顶部"不被秒级精度破坏)。
    pub fn list_api_keys(&self) -> anyhow::Result<Vec<ApiKeyRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT key, label, disabled, created_at FROM api_keys \
             ORDER BY created_at DESC, rowid DESC",
        )?;
        let rows = stmt
            .query_map([], Self::row_to_api_key)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 读取单个 key 的元数据(POST/PATCH 响应体用)。
    pub fn get_api_key(&self, key: &str) -> anyhow::Result<Option<ApiKeyRow>> {
        let conn = self.conn.lock();
        let mut stmt = conn
            .prepare_cached("SELECT key, label, disabled, created_at FROM api_keys WHERE key = ?1")?;
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
            created_at: r.get(3)?,
        })
    }

    /// 严格新增:已存在返回 `false`(admin 创建需要感知冲突,区别于播种用的
    /// [`Self::add_api_key`] 静默忽略)。
    pub fn create_api_key(&self, key: &str, label: Option<&str>) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute(
            "INSERT OR IGNORE INTO api_keys (key, label) VALUES (?1, ?2)",
            (key, label),
        )?;
        Ok(changed == 1)
    }

    /// 部分更新:`None` 字段不动;返回 `false` = key 不存在。
    /// 两个字段都为 `None` 时只做存在性检查(no-op)。
    pub fn update_api_key(
        &self,
        key: &str,
        label: Option<&str>,
        disabled: Option<bool>,
    ) -> anyhow::Result<bool> {
        // 动态拼 SET 子句(列名是代码常量,值全部参数化,无注入面)。
        let mut sets: Vec<&str> = Vec::new();
        let mut params: Vec<Value> = Vec::new();
        if let Some(l) = label {
            sets.push("label = ?");
            // 空串=清空 → 统一落 NULL,与创建路径一致(避免 ''/NULL 双态)。
            params.push(if l.is_empty() {
                Value::Null
            } else {
                Value::Text(l.to_string())
            });
        }
        if let Some(d) = disabled {
            sets.push("disabled = ?");
            params.push(Value::Integer(d as i64));
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
             COALESCE(SUM(cache_creation_tokens),0) FROM usage_records WHERE {where_}"
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
            })
        })?;
        Ok(s)
    }

    /// 按模型聚合(请求数降序,按筛选)。
    pub fn usage_by_model(&self, filter: &UsageFilter) -> anyhow::Result<Vec<UsageByModel>> {
        let (where_, params) = Self::filter_where(filter);
        let sql = format!(
            "SELECT model, COUNT(*), COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), \
             COALESCE(SUM(cache_read_tokens),0), COALESCE(SUM(cache_creation_tokens),0) \
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
             COALESCE(SUM(cache_read_tokens),0), COALESCE(SUM(cache_creation_tokens),0) \
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
        assert!(store.create_api_key("sk-old", Some("老客户")).unwrap());
        assert!(store.create_api_key("sk-new", None).unwrap());
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
        store.create_api_key("sk-aa-tiebreak", None).unwrap();
        store.create_api_key("sk-zz-tiebreak", None).unwrap();
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
        assert!(store.create_api_key("sk-dup", None).unwrap(), "首次创建成功");
        assert!(!store.create_api_key("sk-dup", Some("again")).unwrap(), "重复创建返回 false");
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
        store.create_api_key("sk-u", Some("初始")).unwrap();

        // 只改 disabled,label 不动。
        assert!(store.update_api_key("sk-u", None, Some(true)).unwrap());
        let row = store.get_api_key("sk-u").unwrap().unwrap();
        assert!(row.disabled);
        assert_eq!(row.label.as_deref(), Some("初始"));
        // 禁用后鉴权应报告 disabled(router 据此拒绝)。
        assert!(store.authenticate("sk-u").await.unwrap().unwrap().disabled);

        // 只改 label,disabled 不动;空串=清空备注 → NULL(与创建路径一致,
        // 避免 ''/NULL 双态,对抗审查 Architect#5)。
        assert!(store.update_api_key("sk-u", Some(""), None).unwrap());
        let row = store.get_api_key("sk-u").unwrap().unwrap();
        assert_eq!(row.label, None, "清空备注应落 NULL 而非空串");
        assert!(row.disabled, "改 label 不得动 disabled");

        // 不存在的 key 返回 false。
        assert!(!store.update_api_key("sk-ghost", Some("x"), None).unwrap());
        // 双 None = 存在性检查。
        assert!(store.update_api_key("sk-u", None, None).unwrap());
        assert!(!store.update_api_key("sk-ghost", None, None).unwrap());
    }

    #[tokio::test]
    async fn delete_api_key_removes_only_target() {
        let store = SqliteStore::open_in_memory().unwrap();
        store.create_api_key("sk-del", None).unwrap();
        store.create_api_key("sk-keep", None).unwrap();
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
            assert!(store.create_api_key("sk-file-guard-1", None).unwrap());
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
