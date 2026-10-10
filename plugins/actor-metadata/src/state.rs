//! 每位演员的有限重试状态与一次性资料来源迁移。
//!
//! # 上游对应：`state.py`
//!
//! 逐条照搬：`actors` 表结构、`PRAGMA user_version` 迁移（`minnanoav_ref`
//! 列 + 未补齐演员重新入队）、`get` / `save`。
//!
//! # 一处与上游不同
//!
//! **连接串行化**。上游用 `sqlite3.connect(path, timeout=10)` 每次开新连接；
//! 这里 `State` 持有一个 `Mutex<Connection>` —— 任务是单线程顺序跑的，
//! 串行连接足够，且省掉反复开关文件的开销。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use rusqlite::{params, Connection};

/// 上游 `State`。
pub struct State {
    db: Mutex<Connection>,
    /// 资料来源升级后重新入队的演员数（上游 `requeued_count`）。
    pub requeued_count: i64,
}

/// 一位演员的一行状态（上游 `state.get` 返回的 dict）。
#[derive(Debug, Clone)]
pub struct ActorRow {
    pub actor_id: i64,
    pub javdb_id: String,
    pub status: String,
    pub attempts: i64,
    pub last_attempt_at: Option<f64>,
    pub next_attempt_at: Option<f64>,
    pub missing_fields: Vec<String>,
    pub reason: String,
    pub minnanoav_ref: String,
    pub sources: BTreeMap<String, Provenance>,
}

/// 字段的来源审计（上游 `row["sources"][key] = {"url": ..., "fetched_at": ...}`）。
#[derive(Debug, Clone, PartialEq)]
pub struct Provenance {
    pub url: String,
    pub fetched_at: f64,
}

impl State {
    pub fn open(path: &Path) -> Result<Self, StateError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| StateError(format!("state:create_dir:{e}")))?;
        }
        let db = Connection::open(path).map_err(|e| StateError(format!("state:open:{e}")))?;
        // 上游 `BEGIN IMMEDIATE`：写事务一开始就拿锁。
        db.execute_batch("PRAGMA busy_timeout = 10000")
            .map_err(|e| StateError(format!("state:pragma:{e}")))?;
        let mut state = Self {
            db: Mutex::new(db),
            requeued_count: 0,
        };
        state.requeued_count = state.init()?;
        Ok(state)
    }

    fn init(&self) -> Result<i64, StateError> {
        let db = self.db.lock().unwrap();
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS actors (
                actor_id INTEGER PRIMARY KEY,
                javdb_id TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                attempts INTEGER NOT NULL DEFAULT 0,
                last_attempt_at REAL,
                next_attempt_at REAL,
                missing_fields TEXT NOT NULL DEFAULT '[]',
                reason TEXT NOT NULL DEFAULT '',
                minnanoav_ref TEXT NOT NULL DEFAULT '',
                sources TEXT NOT NULL DEFAULT '{}'
            )",
        )
        .map_err(|e| StateError(format!("state:create_table:{e}")))?;
        // user_version 记录资料来源迁移版本，与插件发版号独立。
        let version: i64 = db
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .map_err(|e| StateError(format!("state:user_version:{e}")))?;
        if version < 1 {
            let columns: Vec<String> = db
                .prepare("PRAGMA table_info(actors)")
                .map_err(|e| StateError(format!("state:table_info:{e}")))?
                .query_map([], |row| row.get::<_, String>(1))
                .map_err(|e| StateError(format!("state:table_info:{e}")))?
                .collect::<Result<_, _>>()
                .map_err(|e| StateError(format!("state:table_info:{e}")))?;
            if !columns.iter().any(|c| c == "minnanoav_ref") {
                db.execute(
                    "ALTER TABLE actors ADD COLUMN minnanoav_ref TEXT NOT NULL DEFAULT ''",
                    [],
                )
                .map_err(|e| StateError(format!("state:migrate:{e}")))?;
            }
            // 换源后给尚未补齐的旧来源记录重新尝试的机会，保留资料来源审计。
            let requeued = db
                .execute(
                    "UPDATE actors SET status='pending', attempts=0, last_attempt_at=NULL,
                        next_attempt_at=NULL, reason='source_upgraded'
                    WHERE status IN ('waiting', 'stopped') AND (
                        reason LIKE '%javbus:%' OR reason LIKE '%javdb:%'
                        OR reason IN ('no_data', 'interrupted', 'attempt_limit')
                    )",
                    [],
                )
                .map_err(|e| StateError(format!("state:requeue:{e}")))?;
            db.execute("PRAGMA user_version = 1", [])
                .map_err(|e| StateError(format!("state:user_version:{e}")))?;
            return Ok(requeued as i64);
        }
        Ok(0)
    }

    /// 上游 `State.get`：取一行，没有就插入；`javdb_id` 变了就删了重建
    /// （宿主恢复/重建后复用数字 ID 时，不能复用其他演员的终态和来源映射）。
    pub fn get(&self, actor_id: i64, javdb_id: &str) -> Result<ActorRow, StateError> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO actors (actor_id, javdb_id) VALUES (?, ?)
             ON CONFLICT(actor_id) DO NOTHING",
            params![actor_id, javdb_id],
        )
        .map_err(|e| StateError(format!("state:get:insert:{e}")))?;
        let stored_javdb_id: String = db
            .query_row(
                "SELECT javdb_id FROM actors WHERE actor_id = ?",
                params![actor_id],
                |row| row.get(0),
            )
            .map_err(|e| StateError(format!("state:get:select:{e}")))?;
        if stored_javdb_id != javdb_id {
            db.execute("DELETE FROM actors WHERE actor_id = ?", params![actor_id])
                .map_err(|e| StateError(format!("state:get:delete:{e}")))?;
            db.execute(
                "INSERT INTO actors (actor_id, javdb_id) VALUES (?, ?)",
                params![actor_id, javdb_id],
            )
            .map_err(|e| StateError(format!("state:get:reinsert:{e}")))?;
        }
        Self::read_row(&db, actor_id)
    }

    fn read_row(db: &Connection, actor_id: i64) -> Result<ActorRow, StateError> {
        db.query_row(
            "SELECT actor_id, javdb_id, status, attempts, last_attempt_at,
                    next_attempt_at, missing_fields, reason, minnanoav_ref, sources
             FROM actors WHERE actor_id = ?",
            params![actor_id],
            |row| {
                let missing_fields: String = row.get(6)?;
                let sources: String = row.get(9)?;
                Ok(ActorRow {
                    actor_id: row.get(0)?,
                    javdb_id: row.get(1)?,
                    status: row.get(2)?,
                    attempts: row.get(3)?,
                    last_attempt_at: row.get(4)?,
                    next_attempt_at: row.get(5)?,
                    missing_fields: serde_json::from_str(&missing_fields).unwrap_or_default(),
                    reason: row.get(7)?,
                    minnanoav_ref: row.get(8)?,
                    sources: parse_provenance(&sources),
                })
            },
        )
        .map_err(|e| StateError(format!("state:read:{e}")))
    }

    /// 上游 `State.save`。
    pub fn save(&self, row: &ActorRow) -> Result<(), StateError> {
        let db = self.db.lock().unwrap();
        let missing_fields =
            serde_json::to_string(&row.missing_fields).unwrap_or_else(|_| "[]".to_owned());
        let sources = serde_json::to_string(
            &row.sources
                .iter()
                .map(|(k, p)| {
                    (
                        k,
                        serde_json::json!({"url": p.url, "fetched_at": p.fetched_at}),
                    )
                })
                .collect::<BTreeMap<_, _>>(),
        )
        .unwrap_or_else(|_| "{}".to_owned());
        db.execute(
            "UPDATE actors SET status=?, attempts=?, last_attempt_at=?, next_attempt_at=?,
                missing_fields=?, reason=?, minnanoav_ref=?, sources=? WHERE actor_id=?",
            params![
                row.status,
                row.attempts,
                row.last_attempt_at,
                row.next_attempt_at,
                missing_fields,
                row.reason,
                row.minnanoav_ref,
                sources,
                row.actor_id,
            ],
        )
        .map_err(|e| StateError(format!("state:save:{e}")))?;
        Ok(())
    }
}

fn parse_provenance(raw: &str) -> BTreeMap<String, Provenance> {
    let mut out = BTreeMap::new();
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return out;
    };
    let Some(map) = value.as_object() else {
        return out;
    };
    for (key, item) in map {
        let url = item
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_owned();
        let fetched_at = item
            .get("fetched_at")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        out.insert(key.clone(), Provenance { url, fetched_at });
    }
    out
}

/// 状态文件打不开 / 写不进：任务直接失败（上游没有吞掉这类错误）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateError(pub String);

impl std::fmt::Display for StateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StateError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_state() -> (State, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let state = State::open(&dir.path().join("actor_metadata.sqlite3")).unwrap();
        (state, dir)
    }

    #[test]
    fn get_inserts_and_save_round_trips() {
        let (state, _dir) = temp_state();
        let mut row = state.get(7, "abc123").unwrap();
        assert_eq!(row.status, "pending");
        assert_eq!(row.attempts, 0);
        row.status = "waiting".to_owned();
        row.attempts = 2;
        row.missing_fields = vec!["birthday".to_owned()];
        row.sources.insert(
            "birthday".to_owned(),
            Provenance {
                url: "https://x/".to_owned(),
                fetched_at: 1.5,
            },
        );
        state.save(&row).unwrap();
        let again = state.get(7, "abc123").unwrap();
        assert_eq!(again.status, "waiting");
        assert_eq!(again.attempts, 2);
        assert_eq!(again.missing_fields, vec!["birthday".to_owned()]);
        assert_eq!(again.sources["birthday"].url, "https://x/");
    }

    #[test]
    fn javdb_id_change_resets_the_row() {
        let (state, _dir) = temp_state();
        let mut row = state.get(7, "old").unwrap();
        row.status = "completed".to_owned();
        state.save(&row).unwrap();
        // 宿主复用了数字 ID：旧演员的终态不能留给新演员。
        let fresh = state.get(7, "new").unwrap();
        assert_eq!(fresh.status, "pending");
        assert_eq!(fresh.javdb_id, "new");
    }
}
