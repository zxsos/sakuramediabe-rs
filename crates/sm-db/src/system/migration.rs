//! `SchemaMigration` 表：迁移台账。
//!
//! 对应 `src/model/system/schema_migration.py`。
//!
//! **这张表没有 `TimestampedMixin`** —— 全库唯一的例外。
//! 所以它**没有 `created_at` / `updated_at`**，只有一个 `applied_at`。
//! 迁移记录是只追加的，语义上不需要「创建时间」与「更新时间」之分：
//! 写入时刻本身就是 `applied_at`。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// `schema_migration` 表。
///
/// 继承 `BaseModel` 而非 `TimestampedMixin`，因此无时间戳列。
#[derive(Debug, Clone, FromRow)]
pub struct SchemaMigration {
    pub id: i64,
    /// 迁移名称，全局唯一且带索引。启动时按此判定是否已应用。
    pub name: String,
    /// 应用时刻，默认取当前 UTC。
    pub applied_at: NaiveDateTime,
}

impl SchemaMigration {
    /// 该迁移是否已应用。
    ///
    /// 启动时把已应用的记录与代码里的迁移列表比对，命中即跳过。
    pub fn is_applied(&self, migration_name: &str) -> bool {
        self.name == migration_name
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_migration_by_exact_name() {
        let m = SchemaMigration {
            id: 1,
            name: "0001_initial".to_owned(),
            applied_at: chrono::NaiveDate::from_ymd_opt(2026, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        };
        assert!(m.is_applied("0001_initial"));
        assert!(!m.is_applied("0002_add_playback"));
        assert!(!m.is_applied("0001_initial "), "尾随空格也算不同名");
    }
}
