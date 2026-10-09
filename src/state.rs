//! 一部影片一条记录：抓取结果、译文缓存和失败次数。
//!
//! # 上游对应：`state.py` 的 `DmmState`
//!
//! 表结构逐字照搬（`movie_cache`，16 列）。`MAX_ATTEMPTS = 5` 不变。
//!
//! # 与上游不同的地方
//!
//! 1. **同步 API**：上游是同步 `sqlite3`；这里用 `rusqlite`（bundled），
//!    同样同步 —— 任务里用 `tokio::task::spawn_blocking` 包一层即可。
//! 2. **列名白名单**：`save` 的列名全部由本模块的常量传入，不接受外部列名
//!    （上游靠「调用方都是内部代码」保证；这里编译期保证）。

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{params, Connection, Row};

/// 上游 `MAX_ATTEMPTS`：一个番号 / 一个字段最多试这么多次。
pub const MAX_ATTEMPTS: i64 = 5;

/// 一条缓存记录（`movie_cache` 的一行）。
#[derive(Debug, Clone, Default)]
pub struct MovieCache {
    pub movie_number: String,
    pub fetch_status: String,
    pub raw_title: String,
    pub raw_desc: String,
    pub source_url: String,
    pub source_id: String,
    pub fetch_attempts: i64,
    pub fetch_error: String,
    pub title_zh: Option<String>,
    pub desc_zh: Option<String>,
    pub title_attempts: i64,
    pub desc_attempts: i64,
    pub title_error: String,
    pub desc_error: String,
}

impl MovieCache {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            movie_number: row.get("movie_number")?,
            fetch_status: row.get("fetch_status")?,
            raw_title: row.get("raw_title")?,
            raw_desc: row.get("raw_desc")?,
            source_url: row.get("source_url")?,
            source_id: row.get("source_id")?,
            fetch_attempts: row.get("fetch_attempts")?,
            fetch_error: row.get("fetch_error")?,
            title_zh: row.get("title_zh")?,
            desc_zh: row.get("desc_zh")?,
            title_attempts: row.get("title_attempts")?,
            desc_attempts: row.get("desc_attempts")?,
            title_error: row.get("title_error")?,
            desc_error: row.get("desc_error")?,
        })
    }

    /// 精简版（`load_all` 用）：只取后续流程用到的列。
    fn from_row_light(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            movie_number: row.get("movie_number")?,
            fetch_status: row.get("fetch_status")?,
            raw_title: row.get("raw_title")?,
            raw_desc: row.get("raw_desc")?,
            title_zh: row.get("title_zh")?,
            desc_zh: row.get("desc_zh")?,
            fetch_attempts: row.get("fetch_attempts")?,
            title_attempts: row.get("title_attempts")?,
            desc_attempts: row.get("desc_attempts")?,
            ..Default::default()
        })
    }
}

/// DMM 抓取状态（上游 `DmmState`）。
pub struct DmmState {
    conn: Connection,
}

impl DmmState {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(e))
            })?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS movie_cache (
                movie_number TEXT PRIMARY KEY,
                fetch_status TEXT NOT NULL DEFAULT 'pending',
                raw_title TEXT NOT NULL DEFAULT '',
                raw_desc TEXT NOT NULL DEFAULT '',
                source_url TEXT NOT NULL DEFAULT '',
                source_id TEXT NOT NULL DEFAULT '',
                fetch_attempts INTEGER NOT NULL DEFAULT 0,
                fetch_error TEXT NOT NULL DEFAULT '',
                title_zh TEXT,
                desc_zh TEXT,
                title_attempts INTEGER NOT NULL DEFAULT 0,
                desc_attempts INTEGER NOT NULL DEFAULT 0,
                title_error TEXT NOT NULL DEFAULT '',
                desc_error TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )?;
        Ok(Self { conn })
    }

    /// 内存库（测试用）。
    pub fn in_memory() -> rusqlite::Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(
            "CREATE TABLE movie_cache (
                movie_number TEXT PRIMARY KEY,
                fetch_status TEXT NOT NULL DEFAULT 'pending',
                raw_title TEXT NOT NULL DEFAULT '',
                raw_desc TEXT NOT NULL DEFAULT '',
                source_url TEXT NOT NULL DEFAULT '',
                source_id TEXT NOT NULL DEFAULT '',
                fetch_attempts INTEGER NOT NULL DEFAULT 0,
                fetch_error TEXT NOT NULL DEFAULT '',
                title_zh TEXT,
                desc_zh TEXT,
                title_attempts INTEGER NOT NULL DEFAULT 0,
                desc_attempts INTEGER NOT NULL DEFAULT 0,
                title_error TEXT NOT NULL DEFAULT '',
                desc_error TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
            )",
        )?;
        Ok(Self { conn })
    }

    pub fn load(&self, movie_number: &str) -> rusqlite::Result<Option<MovieCache>> {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM movie_cache WHERE movie_number = ?1")?;
        let mut rows = stmt.query(params![movie_number])?;
        rows.next()?
            .map(MovieCache::from_row)
            .transpose()
    }

    pub fn load_many(
        &self,
        movie_numbers: &[&str],
    ) -> rusqlite::Result<HashMap<String, MovieCache>> {
        let mut cached = HashMap::new();
        for chunk in movie_numbers.chunks(500) {
            let placeholders = chunk
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!("SELECT * FROM movie_cache WHERE movie_number IN ({placeholders})");
            let mut stmt = self.conn.prepare(&sql)?;
            let params: Vec<&dyn rusqlite::ToSql> =
                chunk.iter().map(|s| s as &dyn rusqlite::ToSql).collect();
            let mut rows = stmt.query(params.as_slice())?;
            while let Some(row) = rows.next()? {
                let cache = MovieCache::from_row(row)?;
                cached.insert(cache.movie_number.clone(), cache);
            }
        }
        Ok(cached)
    }

    /// 全量扫描时一次性顺序读取（上游 `load_all`）。
    pub fn load_all(&self) -> rusqlite::Result<HashMap<String, MovieCache>> {
        let mut stmt = self.conn.prepare(
            "SELECT movie_number, fetch_status, raw_title, raw_desc,
                    title_zh, desc_zh, fetch_attempts, title_attempts, desc_attempts
             FROM movie_cache",
        )?;
        let mut rows = stmt.query([])?;
        let mut cached = HashMap::new();
        while let Some(row) = rows.next()? {
            let cache = MovieCache::from_row_light(row)?;
            cached.insert(cache.movie_number.clone(), cache);
        }
        Ok(cached)
    }

    /// 保存字段（上游 `save`）。列名由调用方传 `&str` 常量，不接受外部输入。
    pub fn save(&self, movie_number: &str, fields: &[(&str, FieldValue)]) -> rusqlite::Result<()> {
        if fields.is_empty() {
            return Ok(());
        }
        let columns: Vec<&str> = fields.iter().map(|(name, _)| *name).collect();
        // 列名白名单：只允许 movie_cache 的真实列。
        for name in &columns {
            assert!(
                ALLOWED_COLUMNS.contains(name),
                "state.save 不接受未知列: {name}"
            );
        }
        let placeholders = (0..=fields.len())
            .map(|_| "?")
            .collect::<Vec<_>>()
            .join(", ");
        let updates = columns
            .iter()
            .map(|name| format!("{name} = excluded.{name}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "INSERT INTO movie_cache (movie_number, {}) VALUES ({placeholders})
             ON CONFLICT(movie_number) DO UPDATE SET {updates}, updated_at = CURRENT_TIMESTAMP",
            columns.join(", ")
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let mut values: Vec<Value> = vec![Value::Text(movie_number.to_owned())];
        for (_, v) in fields {
            values.push(v.clone().into());
        }
        let params: Vec<&dyn rusqlite::ToSql> =
            values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
        stmt.execute(params.as_slice())?;
        Ok(())
    }

    /// 记录一次抓取结果（上游 `record_fetch_result`）。
    ///
    /// 原文变了的字段，其译文缓存清掉（`title_zh = None`），下次重新翻译。
    pub fn record_fetch_result(
        &self,
        movie_number: &str,
        result: &crate::dmm::DmmFetchResult,
    ) -> rusqlite::Result<()> {
        let previous = self.load(movie_number)?;
        let mut fields = vec![
            ("fetch_status", FieldValue::text(&result.status)),
            ("raw_title", FieldValue::text(&result.title)),
            ("raw_desc", FieldValue::text(&result.desc)),
            ("source_url", FieldValue::text(&result.source_url)),
            ("source_id", FieldValue::text(&result.source_id)),
            ("fetch_attempts", FieldValue::int(0)),
            ("fetch_error", FieldValue::text("")),
        ];
        for (field, source) in [("title", &result.title), ("desc", &result.desc)] {
            let changed = previous
                .as_ref()
                .map(|p| {
                    let prev = match field {
                        "title" => &p.raw_title,
                        _ => &p.raw_desc,
                    };
                    prev != source
                })
                .unwrap_or(true);
            if changed {
                // 原文变了：译文缓存清掉，下次重新翻译。
                let (zh_key, attempts_key, error_key) = match field {
                    "title" => ("title_zh", "title_attempts", "title_error"),
                    _ => ("desc_zh", "desc_attempts", "desc_error"),
                };
                fields.push((
                    zh_key,
                    if source.is_empty() {
                        FieldValue::text("")
                    } else {
                        FieldValue::null()
                    },
                ));
                fields.push((attempts_key, FieldValue::int(0)));
                fields.push((error_key, FieldValue::text("")));
            }
        }
        self.save(movie_number, &fields)
    }
}

/// `movie_cache` 的合法列（`save` 的白名单）。
const ALLOWED_COLUMNS: &[&str] = &[
    "fetch_status",
    "raw_title",
    "raw_desc",
    "source_url",
    "source_id",
    "fetch_attempts",
    "fetch_error",
    "title_zh",
    "desc_zh",
    "title_attempts",
    "desc_attempts",
    "title_error",
    "desc_error",
];

/// 字段值。
#[derive(Debug, Clone)]
pub enum FieldValue {
    Text(String),
    Int(i64),
    Null,
}

impl FieldValue {
    pub fn text(s: &str) -> Self {
        Self::Text(s.to_owned())
    }
    pub fn int(n: i64) -> Self {
        Self::Int(n)
    }
    pub fn null() -> Self {
        Self::Null
    }
}

#[derive(Debug, Clone)]
enum Value {
    Text(String),
    Int(i64),
    Null,
}

impl From<FieldValue> for Value {
    fn from(v: FieldValue) -> Self {
        match v {
            FieldValue::Text(s) => Value::Text(s),
            FieldValue::Int(n) => Value::Int(n),
            FieldValue::Null => Value::Null,
        }
    }
}

impl rusqlite::ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
        match self {
            Value::Text(s) => Ok(rusqlite::types::ToSqlOutput::from(s.as_str())),
            Value::Int(n) => Ok(rusqlite::types::ToSqlOutput::from(*n)),
            Value::Null => Ok(rusqlite::types::ToSqlOutput::from(rusqlite::types::Null)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_load_roundtrip() {
        let state = DmmState::in_memory().unwrap();
        state
            .save(
                "ABC-123",
                &[
                    ("fetch_status", FieldValue::text("success")),
                    ("raw_title", FieldValue::text("タイトル")),
                ],
            )
            .unwrap();
        let cache = state.load("ABC-123").unwrap().unwrap();
        assert_eq!(cache.fetch_status, "success");
        assert_eq!(cache.raw_title, "タイトル");
        // upsert：第二次写覆盖。
        state
            .save("ABC-123", &[("fetch_status", FieldValue::text("error"))])
            .unwrap();
        let cache = state.load("ABC-123").unwrap().unwrap();
        assert_eq!(cache.fetch_status, "error");
        assert_eq!(cache.raw_title, "タイトル");
    }

    #[test]
    fn missing_number_returns_none() {
        let state = DmmState::in_memory().unwrap();
        assert!(state.load("NOPE-1").unwrap().is_none());
    }

    #[test]
    fn record_fetch_result_clears_stale_translations() {
        use crate::dmm::DmmFetchResult;
        let state = DmmState::in_memory().unwrap();
        // 先有一条带译文的记录。
        state
            .save(
                "ABC-123",
                &[
                    ("fetch_status", FieldValue::text("success")),
                    ("raw_title", FieldValue::text("旧标题")),
                    ("title_zh", FieldValue::text("旧译文")),
                ],
            )
            .unwrap();
        // 重新抓取，标题变了：译文缓存清掉。
        state
            .record_fetch_result(
                "ABC-123",
                &DmmFetchResult {
                    status: "success".to_owned(),
                    title: "新标题".to_owned(),
                    desc: String::new(),
                    source_url: String::new(),
                    source_id: String::new(),
                },
            )
            .unwrap();
        let cache = state.load("ABC-123").unwrap().unwrap();
        assert_eq!(cache.raw_title, "新标题");
        assert_eq!(cache.title_zh, None);
        assert_eq!(cache.title_attempts, 0);
    }

    #[test]
    fn record_fetch_result_keeps_translations_when_source_unchanged() {
        use crate::dmm::DmmFetchResult;
        let state = DmmState::in_memory().unwrap();
        state
            .save(
                "ABC-123",
                &[
                    ("fetch_status", FieldValue::text("success")),
                    ("raw_title", FieldValue::text("同标题")),
                    ("title_zh", FieldValue::text("译文")),
                ],
            )
            .unwrap();
        state
            .record_fetch_result(
                "ABC-123",
                &DmmFetchResult {
                    status: "success".to_owned(),
                    title: "同标题".to_owned(),
                    desc: String::new(),
                    source_url: String::new(),
                    source_id: String::new(),
                },
            )
            .unwrap();
        let cache = state.load("ABC-123").unwrap().unwrap();
        assert_eq!(cache.title_zh.as_deref(), Some("译文"));
    }
}
