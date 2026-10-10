//! 影片级抓取状态（上游 `state.py`）。
//!
//! 只做一件事：记住「这部片已经完整抓过一轮 SubtitleCat」，让定时任务对
//! 「老片 + 已抓过」的组合跳过重复访问来源。
//!
//! # 上游对应
//!
//! | 上游 | 这里 |
//! |---|---|
//! | `state.py:SubtitleCatFetchState.__init__` | [`FetchState::open`] |
//! | `state.py:has_fetched` | [`FetchState::has_fetched`] |
//! | `state.py:mark_fetched` | [`FetchState::mark_fetched`] |
//! | `state.py:_key` | [`state_key`] |
//!
//! 表结构、`ON CONFLICT ... DO UPDATE`、`_key` 的「去空白后不能为空」都
//! 逐条照抄。上游选 SQLite 的理由同样成立：手动任务与定时任务可能并行，
//! SQLite 自带事务与 `busy_timeout` 处理这种偶尔的碰撞。
//!
//! # 一处与上游不同
//!
//! **连接串行化**。上游每次调用开一个新连接（`sqlite3.connect(timeout=30)`）；
//! 这里持有一个 `Mutex<Connection>` —— 任务本身是顺序跑的，串行连接足够，
//! 且省掉反复开关文件的开销（与 `plugin-actor-metadata/src/state.rs` 同一取舍）。

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};

/// 上游 `SubtitleCatFetchState`。
pub struct FetchState {
    db: Mutex<Connection>,
}

impl FetchState {
    /// 打开（必要时建目录、建表）。
    ///
    /// 上游在 `__init__` 里就 `mkdir(parents=True, exist_ok=True)` +
    /// `CREATE TABLE IF NOT EXISTS` —— 这里是**任务运行时**调用，不是注册期
    /// （注册期不许创建外部目录，见 `service.rs` 的模块文档）。
    pub fn open(path: &Path) -> Result<Self, StateError> {
        // 只看非空 parent：`Path::new("x.sqlite3").parent()` 是 `Some("")`，
        // 对空路径 `create_dir_all` 的表现不由 std 保证。
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|e| StateError(format!("state:create_dir:{e}")))?;
        }
        let db = Connection::open(path).map_err(|e| StateError(format!("state:open:{e}")))?;
        // 上游 `sqlite3.connect(..., timeout=30)` + `PRAGMA busy_timeout = 30000`。
        db.execute_batch("PRAGMA busy_timeout = 30000")
            .map_err(|e| StateError(format!("state:pragma:{e}")))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS movie_fetch_state (
                movie_number TEXT PRIMARY KEY,
                last_fetched_at TEXT NOT NULL
            )",
        )
        .map_err(|e| StateError(format!("state:create_table:{e}")))?;
        Ok(Self {
            db: Mutex::new(db),
        })
    }

    /// 上游 `has_fetched`。
    pub fn has_fetched(&self, movie_number: &str) -> Result<bool, StateError> {
        let key = state_key(movie_number)?;
        let db = self.db.lock().unwrap();
        let row: Option<i64> = db
            .query_row(
                "SELECT 1 FROM movie_fetch_state WHERE movie_number = ? LIMIT 1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| StateError(format!("state:has_fetched:{e}")))?;
        Ok(row.is_some())
    }

    /// 上游 `mark_fetched`（重复标记是**更新**，不是错误）。
    pub fn mark_fetched(
        &self,
        movie_number: &str,
        fetched_at: DateTime<Utc>,
    ) -> Result<(), StateError> {
        let key = state_key(movie_number)?;
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO movie_fetch_state (movie_number, last_fetched_at)
             VALUES (?, ?)
             ON CONFLICT(movie_number) DO UPDATE SET
                 last_fetched_at = excluded.last_fetched_at",
            params![key, fetched_at.to_rfc3339()],
        )
        .map_err(|e| StateError(format!("state:mark_fetched:{e}")))?;
        Ok(())
    }
}

/// 上游 `_key`：番号去空白后不能为空。
fn state_key(movie_number: &str) -> Result<String, StateError> {
    let normalized = movie_number.trim();
    if normalized.is_empty() {
        return Err(StateError("movie_number 不能为空".to_owned()));
    }
    Ok(normalized.to_owned())
}

/// 状态打不开 / 写不进：任务直接失败（上游没有吞掉这类错误）。
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
    use chrono::TimeZone;

    fn temp_state() -> (FetchState, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let state = FetchState::open(&dir.path().join("nested/fetch_state.sqlite3")).unwrap();
        (state, dir)
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).unwrap()
    }

    #[test]
    fn mark_then_has_fetched() {
        let (state, _dir) = temp_state();
        assert!(!state.has_fetched("SSNI-888").unwrap());
        state.mark_fetched("SSNI-888", at(1_700_000_000)).unwrap();
        assert!(state.has_fetched("SSNI-888").unwrap());
        // 别的番号不受影响。
        assert!(!state.has_fetched("SSNI-889").unwrap());
    }

    #[test]
    fn marking_again_updates_instead_of_failing() {
        let (state, _dir) = temp_state();
        state.mark_fetched("SSNI-888", at(1)).unwrap();
        state.mark_fetched("SSNI-888", at(2)).unwrap();
        assert!(state.has_fetched("SSNI-888").unwrap());
    }

    #[test]
    fn state_survives_a_reopen() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("fetch_state.sqlite3");
        FetchState::open(&path)
            .unwrap()
            .mark_fetched("SSNI-888", at(1))
            .unwrap();
        // 下一轮任务重开同一个文件，该记得的还是记得。
        let reopened = FetchState::open(&path).unwrap();
        assert!(reopened.has_fetched("SSNI-888").unwrap());
    }

    #[test]
    fn blank_numbers_are_rejected() {
        let (state, _dir) = temp_state();
        assert!(state.has_fetched("   ").is_err());
        assert!(state.mark_fetched("", at(1)).is_err());
    }
}
