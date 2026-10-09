//! 数据访问错误。
//!
//! # 为什么要分类
//!
//! service 层需要按错误种类映射 HTTP 状态码，混在一起就没法分：
//!
//! | 变体 | HTTP | 场景 |
//! |---|---|---|
//! | [`DbError::NotFound`] | 404 | 按主键/唯一键查找未命中 |
//! | [`DbError::ConstraintViolation`] | 409 | 唯一键冲突、外键被拒、CHECK 不通过 |
//! | [`DbError::Business`] | 422 | 不变量、字段护栏、状态机非法迁移 |
//! | [`DbError::Db`] | 500 | 连接失败、语法错误、类型不匹配 |
//!
//! 把 [`DbError::Business`] 与 [`DbError::ConstraintViolation`] 分开是关键：
//! 前者是我们能预先拦住的（写入前校验），后者是数据库兜底才发现的。
//! 同一个业务错误，拦在仓储层返回 422，漏到数据库就变成 409 —— 客户端
//! 看到的语义不同，所以能拦就必须在拦。

use std::fmt;

/// 数据访问错误。
#[derive(Debug)]
pub enum DbError {
    /// 数据库本身出错：连接中断、SQL 语法错误、类型不匹配等。
    ///
    /// 归到 500 而不是 4xx —— 这些不是客户端能修的。
    Db(sqlx::Error),

    /// 按主键或唯一键查找未命中。
    NotFound {
        /// 实体名，与 `repo` 里的 Repository 对应，便于日志定位。
        entity: &'static str,
        /// 用于查找的键值（主键 id、番号等）。
        key: String,
    },

    /// 数据库约束拒绝写入。
    ///
    /// 包括 UNIQUE 冲突、外键被拒、CHECK 不通过、NOT NULL 违反。
    /// 归到 409 而非 500：这是**状态冲突**，重试或换输入可能成功。
    ConstraintViolation {
        entity: &'static str,
        /// 约束名。从 PostgreSQL 错误里提取，便于排查是哪个约束。
        constraint: String,
    },

    /// 业务规则拒绝：XOR 不变量、字段护栏、状态机非法迁移。
    ///
    /// 与 [`DbError::ConstraintViolation`] 的区别是**谁先发现的**：
    /// 这里是仓储层写入前主动校验，服务层据此返回 422 并附上业务原因；
    /// 那里是数据库兜底，只能返回 409。
    Business {
        entity: &'static str,
        /// 人类可读的原因，直接进日志与错误详情。
        reason: String,
    },
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Db(err) => write!(f, "database error: {err}"),
            Self::NotFound { entity, key } => write!(f, "{entity} not found: {key}"),
            Self::ConstraintViolation { entity, constraint } => {
                write!(f, "{entity} violates constraint: {constraint}")
            }
            Self::Business { entity, reason } => write!(f, "{entity} rejected: {reason}"),
        }
    }
}

impl std::error::Error for DbError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Db(err) => Some(err),
            _ => None,
        }
    }
}

impl From<sqlx::Error> for DbError {
    fn from(err: sqlx::Error) -> Self {
        // 约束类错误就地归类，调用方不必自己 match sqlx 的错误码。
        //
        // sqlx 对 PostgreSQL 暴露 `DatabaseError::code()`，其中 23xxx 是
        // integrity_constraint_violation。
        if let Some(db_err) = err.as_database_error() {
            let code = db_err.code().unwrap_or_default().to_string();
            if code.starts_with("23") {
                return Self::ConstraintViolation {
                    entity: "unknown",
                    constraint: constraint_name_of(&code, db_err.constraint()),
                };
            }
        }
        Self::Db(err)
    }
}

/// 从 PostgreSQL 错误里取出约束名。
///
/// 优先用 `constraint()` —— 它给出具体的约束标识（如
/// `movie_subscription_blacklist_exclusive`）；拿不到时退回错误码，
/// 至少能看出是哪一类完整性违反。
fn constraint_name_of(code: &str, constraint: Option<&str>) -> String {
    if let Some(name) = constraint {
        return name.to_owned();
    }
    match code {
        "23505" => "unique_violation".to_owned(),
        "23503" => "foreign_key_violation".to_owned(),
        "23514" => "check_violation".to_owned(),
        "23502" => "not_null_violation".to_owned(),
        other => other.to_owned(),
    }
}

impl DbError {
    /// 构造 NotFound。
    pub fn not_found(entity: &'static str, key: impl fmt::Display) -> Self {
        Self::NotFound {
            entity,
            key: key.to_string(),
        }
    }

    /// 构造 Business 拒绝。
    pub fn business(entity: &'static str, reason: impl fmt::Display) -> Self {
        Self::Business {
            entity,
            reason: reason.to_string(),
        }
    }

    /// 补上实体名。
    ///
    /// [`From<sqlx::Error>`] 不知道是哪个 Repository 出的错，只能填
    /// `"unknown"`。仓储层在返回前调用它补齐，日志里就能直接定位。
    pub fn with_entity(mut self, entity: &'static str) -> Self {
        if let Self::ConstraintViolation { entity: e, .. } = &mut self {
            if *e == "unknown" {
                *e = entity;
            }
        }
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_carries_actionable_detail() {
        // 三种错误的文案都必须能让人不查代码就知道发生了什么。
        let nf = DbError::not_found("Movie", "ABC-001");
        assert_eq!(nf.to_string(), "Movie not found: ABC-001");

        let biz = DbError::business("Media", "movie_number 与 video_item_id 恰好其一非空");
        assert!(biz.to_string().contains("恰好其一非空"));

        let cv = DbError::ConstraintViolation {
            entity: "Movie",
            constraint: "movie_subscription_blacklist_exclusive".to_owned(),
        };
        assert!(cv
            .to_string()
            .contains("movie_subscription_blacklist_exclusive"));
    }

    #[test]
    fn with_entity_fills_only_the_unknown_placeholder() {
        let placeholder = DbError::ConstraintViolation {
            entity: "unknown",
            constraint: "23505".to_owned(),
        };
        match placeholder.with_entity("Movie") {
            DbError::ConstraintViolation { entity, .. } => assert_eq!(entity, "Movie"),
            other => panic!("expected ConstraintViolation, got {other:?}"),
        }

        // 已有实体名时不应被覆盖 —— Repository 可能已经填对了。
        let named = DbError::ConstraintViolation {
            entity: "Media",
            constraint: "x".to_owned(),
        };
        match named.with_entity("Movie") {
            DbError::ConstraintViolation { entity, .. } => assert_eq!(entity, "Media"),
            other => panic!("expected ConstraintViolation, got {other:?}"),
        }
    }

    #[test]
    fn constraint_names_fall_back_to_sqlstate() {
        // 拿不到约束名时至少要能分辨是哪一类违反。
        assert_eq!(constraint_name_of("23505", None), "unique_violation");
        assert_eq!(constraint_name_of("23503", None), "foreign_key_violation");
        assert_eq!(constraint_name_of("23514", None), "check_violation");
        assert_eq!(constraint_name_of("23502", None), "not_null_violation");
        assert_eq!(constraint_name_of("99999", None), "99999");
        // 真实约束名优先于错误码。
        assert_eq!(
            constraint_name_of("23514", Some("movie_blacklist_exclusive")),
            "movie_blacklist_exclusive"
        );
    }
}
