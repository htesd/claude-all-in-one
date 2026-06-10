//! gw-store —— SQLite(WAL) 持久化。
//!
//! Phase 0:控制面最小实现(api_keys 表 + 鉴权)。WAL 模式以支持
//! 多进程(router + 多 worker)并发读、控制面写少。
//! usage / 状态机 / request_cache 在 P2/P4 按 IMPROVEMENTS.md 补。

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use gw_core::account::Account;
use gw_core::config::AccountsConfig;
use gw_core::store::{
    AccountPatch, AccountRow, ApiKeyPatch, ApiKeyRow, AuthenticatedKey, ControlStore, GroupRow,
    UsageByKey, UsageByModel, UsageFilter, UsageRecord, UsageSink, UsageSummary,
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

-- 分组(账号组,同时用于 key 归类)。name 即 accounts.yaml 的组名(G0/G1...)。
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
    max_concurrency INTEGER NOT NULL DEFAULT 1,
    disabled        INTEGER NOT NULL DEFAULT 0,
    extra           TEXT NOT NULL DEFAULT '{}',
    created_at      INTEGER NOT NULL DEFAULT (strftime('%s','now'))
);
CREATE INDEX IF NOT EXISTS idx_accounts_group ON accounts(group_name);
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
        Ok(())
    }

    /// 列不存在则 ADD COLUMN(表名/DDL 为代码常量,无注入面)。
    fn ensure_column(conn: &Connection, table: &str, col: &str, ddl: &str) -> anyhow::Result<()> {
        let exists: bool = conn
            .prepare(&format!(
                "SELECT 1 FROM pragma_table_info('{table}') WHERE name = ?1"
            ))?
            .exists([col])?;
        if !exists {
            conn.execute(&format!("ALTER TABLE {table} ADD COLUMN {ddl}"), [])?;
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
             (SELECT COUNT(*) FROM api_keys k WHERE k.group_name = g.name) \
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
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 严格新增分组;已存在返回 `false`。
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
    /// `false` = 组不存在。三条语句包成事务,避免删组成功但引用残留。
    pub fn delete_group(&self, name: &str) -> anyhow::Result<bool> {
        let mut conn = self.conn.lock();
        let tx = conn.transaction()?;
        let changed = tx.execute("DELETE FROM groups WHERE name = ?1", [name])?;
        if changed == 1 {
            tx.execute("UPDATE accounts SET group_name = '' WHERE group_name = ?1", [name])?;
            tx.execute("UPDATE api_keys SET group_name = '' WHERE group_name = ?1", [name])?;
        }
        tx.commit()?;
        Ok(changed == 1)
    }

    // ───────── 账号 CRUD(配置态;运行态见 worker /status) ─────────

    const ACCOUNT_COLS: &'static str =
        "account_id, group_name, provider, max_concurrency, disabled, extra, created_at";

    fn row_to_account(r: &rusqlite::Row<'_>) -> rusqlite::Result<AccountRow> {
        Ok(AccountRow {
            account_id: r.get(0)?,
            group_name: r.get(1)?,
            provider: r.get(2)?,
            max_concurrency: r.get(3)?,
            disabled: r.get::<_, i64>(4)? != 0,
            extra: r.get(5)?,
            created_at: r.get(6)?,
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
        Ok(changed == 1)
    }

    /// 部分更新(见 [`AccountPatch`]);`false` = 账号不存在。
    pub fn update_account(&self, account_id: &str, patch: &AccountPatch) -> anyhow::Result<bool> {
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
        let conn = self.conn.lock();
        if sets.is_empty() {
            let exists: bool = conn
                .prepare_cached("SELECT 1 FROM accounts WHERE account_id = ?1")?
                .exists([account_id])?;
            return Ok(exists);
        }
        params.push(Value::Text(account_id.to_string()));
        let sql = format!("UPDATE accounts SET {} WHERE account_id = ?", sets.join(", "));
        let changed = conn.execute(&sql, rusqlite::params_from_iter(params))?;
        Ok(changed == 1)
    }

    /// 删除账号;`false` = 不存在。usage_records 历史归属不动。
    pub fn delete_account(&self, account_id: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let changed = conn.execute("DELETE FROM accounts WHERE account_id = ?1", [account_id])?;
        Ok(changed == 1)
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

    /// 分组是否存在(admin 写入 group_name 前的存在性校验,防"幽灵分组")。
    pub fn group_exists(&self, name: &str) -> anyhow::Result<bool> {
        let conn = self.conn.lock();
        let exists = conn
            .prepare_cached("SELECT 1 FROM groups WHERE name = ?1")?
            .exists([name])?;
        Ok(exists)
    }

    /// 取某组账号并转换为运行时 [`Account`](gw_core::account::Account)
    /// (extra JSON 解码回字段表;含已禁用账号,调度器自行处理 disabled)。
    /// 单行 extra 损坏时跳过该账号并告警,不拖垮整组。
    pub fn load_group_accounts(&self, group: &str) -> anyhow::Result<Vec<Account>> {
        let rows = {
            let conn = self.conn.lock();
            let mut stmt = conn.prepare_cached(&format!(
                "SELECT {} FROM accounts WHERE group_name = ?1 ORDER BY account_id ASC",
                Self::ACCOUNT_COLS
            ))?;
            let collected = stmt
                .query_map([group], Self::row_to_account)?
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
                extra,
            });
        }
        Ok(accounts)
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
        // over_quota 在 SQL 内算好(quota_tokens NULL = 不限),鉴权路径零额外查询。
        let mut stmt = conn.prepare_cached(
            "SELECT key, disabled, \
             (quota_tokens IS NOT NULL AND used_tokens >= quota_tokens) \
             FROM api_keys WHERE key = ?1",
        )?;
        let row = stmt
            .query_row([api_key], |r| {
                Ok(AuthenticatedKey {
                    key_id: r.get::<_, String>(0)?,
                    disabled: r.get::<_, i64>(1)? != 0,
                    over_quota: r.get::<_, i64>(2)? != 0,
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

        // 删除:引用方 group_name 清空,而非级联删除。
        assert!(store.delete_group("G0").unwrap());
        assert!(!store.delete_group("G0").unwrap(), "二次删除 false");
        assert_eq!(store.get_account("kiro-01").unwrap().unwrap().group_name, "");
        assert_eq!(store.get_api_key("sk-g0-a").unwrap().unwrap().group_name, "");
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
        assert!(store.update_account("kiro-01", &patch).unwrap());
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
    fn load_group_accounts_converts_to_runtime_account() {
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

        let accounts = store.load_group_accounts("G0").unwrap();
        assert_eq!(accounts.len(), 2, "只取 G0,含禁用账号");
        let a1 = accounts.iter().find(|a| a.account_id == "kiro-01").unwrap();
        assert_eq!(a1.provider, "kiro");
        assert_eq!(a1.max_concurrency, 3);
        assert_eq!(a1.extra_str("refresh_token"), Some("rt-1"));
        assert_eq!(a1.extra.get("priority").and_then(|v| v.as_i64()), Some(5));
        let a3 = accounts.iter().find(|a| a.account_id == "kiro-03").unwrap();
        assert!(a3.disabled);
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
