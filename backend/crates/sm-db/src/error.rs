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

/// PostgreSQL `unique_violation` 的 SQLSTATE。
pub const UNIQUE_VIOLATION: &str = "23505";
/// `foreign_key_violation`。
pub const FOREIGN_KEY_VIOLATION: &str = "23503";
/// `check_violation`。
pub const CHECK_VIOLATION: &str = "23514";
/// `not_null_violation`。
pub const NOT_NULL_VIOLATION: &str = "23502";

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
        /// PostgreSQL **SQLSTATE**，如 `23505`（唯一违例）、`23503`（外键）、
        /// `23514`（CHECK）、`23502`（NOT NULL）。
        ///
        /// # 为什么必须带上它
        ///
        /// 只有约束**名**时，「这是不是唯一违例」只能靠字符串猜 —— 而
        /// `background_task_run_mutex_key_uniq` 这种名字既可能变，也可能与
        /// 别的约束撞形状。业务上有一类逻辑**必须**区分它们：定时任务的
        /// 「同 mutex_key 已在队列 → 按 coalesce 跳过」与视频合集的
        /// 「重复加入 → 幂等返回」，都要求「唯一违例走跳过、其余照常报错」。
        /// 猜错的后果是幂等路径静默失效，并发下表现为「同一个任务被入队两次」。
        code: &'static str,
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
            Self::ConstraintViolation {
                entity, constraint, ..
            } => {
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
                    code: known_sqlstate(&code),
                    constraint: constraint_name_of(&code, db_err.constraint()),
                };
            }
        }
        Self::Db(err)
    }
}

/// 把 SQLSTATE 收敛成已知的四个常量之一。
///
/// `&'static str` 是为了让 [`DbError::ConstraintViolation::code`] 能按值比较 ——
/// 「这是不是唯一违例」这个问题每轮调度与每次合集成员追加都要问一次，
/// 不该让每个调用方各自写一遍字符串比较。
fn known_sqlstate(code: &str) -> &'static str {
    match code {
        UNIQUE_VIOLATION => UNIQUE_VIOLATION,
        FOREIGN_KEY_VIOLATION => FOREIGN_KEY_VIOLATION,
        CHECK_VIOLATION => CHECK_VIOLATION,
        NOT_NULL_VIOLATION => NOT_NULL_VIOLATION,
        _ => "23000",
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

    /// 是不是**唯一**约束违例（`23505`）。
    ///
    /// 「唯一冲突」与「外键 / CHECK / NOT NULL 违例」在业务上后果完全不同：
    /// 前者常常意味着「已经有人做过了，跳过即可」（定时任务的 coalesce、
    /// 合集成员的幂等加入），后者是**真错误**，必须冒泡。把两者混为一谈的
    /// 代价是掩盖真实缺陷 —— 所以这个判定只认 SQLSTATE，不看约束名。
    pub fn is_unique_violation(&self) -> bool {
        matches!(
            self,
            Self::ConstraintViolation { code, .. } if *code == UNIQUE_VIOLATION
        )
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
            code: CHECK_VIOLATION,
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
            code: UNIQUE_VIOLATION,
            constraint: "background_task_run_mutex_key_uniq".to_owned(),
        };
        match placeholder.with_entity("Movie") {
            DbError::ConstraintViolation { entity, .. } => assert_eq!(entity, "Movie"),
            other => panic!("expected ConstraintViolation, got {other:?}"),
        }

        // 已有实体名时不应被覆盖 —— Repository 可能已经填对了。
        let named = DbError::ConstraintViolation {
            entity: "Media",
            code: UNIQUE_VIOLATION,
            constraint: "x".to_owned(),
        };
        match named.with_entity("Movie") {
            DbError::ConstraintViolation { entity, .. } => assert_eq!(entity, "Media"),
            other => panic!("expected ConstraintViolation, got {other:?}"),
        }
    }

    #[test]
    fn only_sqlstate_decides_whether_it_is_a_unique_violation() {
        // 这条测试存在的理由：上一版按「约束名以 23505 开头」判定，而
        // `From<sqlx::Error>` 存的是**约束名**（`background_task_run_mutex_key_uniq`）
        // —— 于是判定在真实链路上恒为 false，定时任务的 coalesce 跳过与
        // 合集成员的幂等加入都静默失效。约束名长得像 SQLSTATE 的那个 case
        // （拿不到约束名时的回退值）恰好让人误以为它能用。
        let unique_by_name_only = DbError::ConstraintViolation {
            entity: "BackgroundTaskRun",
            code: FOREIGN_KEY_VIOLATION,
            constraint: "23505".to_owned(),
        };
        assert!(
            !unique_by_name_only.is_unique_violation(),
            "约束名像 23505 不等于唯一违例"
        );

        let real_unique = DbError::ConstraintViolation {
            entity: "BackgroundTaskRun",
            code: UNIQUE_VIOLATION,
            constraint: "background_task_run_mutex_key_uniq".to_owned(),
        };
        assert!(real_unique.is_unique_violation());

        for other in [FOREIGN_KEY_VIOLATION, CHECK_VIOLATION, NOT_NULL_VIOLATION] {
            let err = DbError::ConstraintViolation {
                entity: "X",
                code: other,
                constraint: "whatever".to_owned(),
            };
            assert!(!err.is_unique_violation(), "{other} 不是唯一违例");
        }
        assert!(!DbError::business("X", "y").is_unique_violation());
    }

    #[test]
    fn constraint_names_fall_back_to_sqlstate() {
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
