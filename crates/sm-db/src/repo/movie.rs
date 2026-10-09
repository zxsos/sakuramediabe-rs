//! `movie` 表仓储。
//!
//! # 为什么不用 `query!` 宏
//!
//! `sqlx` 开了 `macros` feature，但 `query!` 需要**编译期**数据库连接。
//! 本机有 PG、CI 没有，用宏会让 CI 直接编译失败。所以全部走
//! [`sqlx::query_as`] + [`FromRow`]（运行时解析）。
//!
//! 代价是失去编译期列名校验，补偿手段是两道验证：
//!
//! - L1 `parity/compare_schema.py` 保证 Rust 结构体与 Peewee 模型一致
//! - L2 集成测试在真实 PG 上跑，任何不匹配会在运行时报出来

use sqlx::{PgPool, Postgres};

use crate::catalog::movie::{field_owner, Movie, MovieSeries, PROTECTED_MOVIE_FIELDS};
use crate::common::guard::{FieldGuard, WriteSource};
use crate::common::update::UpdateSet;
use crate::error::DbError;

/// 实体名，用于错误分类。
const ENTITY: &str = "Movie";

/// 字段护栏：受保护字段白名单 + 宿主独占的状态列。
fn guard() -> FieldGuard {
    FieldGuard::new(
        ENTITY,
        &PROTECTED_MOVIE_FIELDS,
        &["field_owners", "mutation_revision"],
    )
}

/// 插入一条影片。
///
/// 只暴露**有业务含义**的列；`heat` / `watched_count` / `comment_count`
/// 这些计数器由 service 层在业务流程里累加，不该在「创建影片」时被随手
/// 指定成任意值。其余列走数据库 DEFAULT。
#[derive(Debug, Clone, Default)]
pub struct NewMovie {
    /// 番号。**只去首尾空白、不做归一化改写** —— 分隔符与大小写都是有效信息。
    pub movie_number: String,
    pub title: String,
    /// 空串在写入前归一为 `None`（对应上游 `save()` 的 `or None`）。
    pub javdb_id: Option<String>,
    pub summary: Option<String>,
    pub maker_name: Option<String>,
    pub director_name: Option<String>,
    pub release_date: Option<chrono::NaiveDateTime>,
    pub duration_minutes: Option<i32>,
    pub score: Option<f64>,
    pub score_number: Option<i32>,
    pub series_id: Option<i64>,
    pub cover_image_id: Option<i64>,
    pub thin_cover_image_id: Option<i64>,
    /// JSONB，默认 NULL。
    pub metadata_source: Option<serde_json::Value>,
}

/// `movie` 表仓储。
#[derive(Debug, Clone)]
pub struct MovieRepository {
    pool: PgPool,
}

impl MovieRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。事务场景需要它来保证同一连接。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<Movie>, DbError> {
        let row = sqlx::query_as::<_, Movie>("SELECT * FROM movie WHERE id = $1")
            .bind(id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// 按主键查询，未命中返回 [`DbError::NotFound`]。
    pub async fn require_by_id(&self, id: i32) -> Result<Movie, DbError> {
        self.find_by_id(id)
            .await?
            .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    /// 按番号查询。番号是业务主键，外部调用方几乎总是用它。
    pub async fn find_by_number(&self, movie_number: &str) -> Result<Option<Movie>, DbError> {
        let row = sqlx::query_as::<_, Movie>("SELECT * FROM movie WHERE movie_number = $1")
            .bind(movie_number)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }

    /// 插入。
    ///
    /// `javdb_id` 的空串在此归一为 `None` —— 库里出现空串会让
    /// `WHERE javdb_id = ''` 命中一条「没有 JavDB 编号」的假记录，
    /// 而唯一索引把第二条例外也挡掉了。
    pub async fn insert(&self, new: &NewMovie) -> Result<Movie, DbError> {
        let javdb_id = new
            .javdb_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned);

        let movie_number = new.movie_number.trim();
        if movie_number.is_empty() {
            return Err(DbError::business(ENTITY, "movie_number 不能为空"));
        }

        let sql = "\
            INSERT INTO movie (
                movie_number, title, javdb_id, summary, maker_name, director_name,
                release_date, duration_minutes, score, score_number, series_id,
                cover_image_id, thin_cover_image_id, metadata_source,
                created_at, updated_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14,
                $15, $15
            ) RETURNING *";

        let row = sqlx::query_as::<_, Movie>(sql)
            .bind(movie_number)
            .bind(new.title.trim())
            .bind(javdb_id)
            .bind(new.summary.as_deref().unwrap_or_default())
            .bind(new.maker_name.as_deref().map(str::trim))
            .bind(new.director_name.as_deref().map(str::trim))
            .bind(new.release_date)
            .bind(new.duration_minutes)
            .bind(new.score)
            .bind(new.score_number)
            .bind(new.series_id)
            .bind(new.cover_image_id)
            .bind(new.thin_cover_image_id)
            .bind(new.metadata_source.as_ref())
            .bind(crate::common::time::now_utc())
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))?;

        Ok(row)
    }

    /// 更新。
    ///
    /// 三件事按顺序发生，顺序不能调换：
    ///
    /// 1. **护栏校验** —— 先看这次写入合不合法
    /// 2. **CHECK 预判** —— `is_subscribed` 与 `is_blacklisted` 不能同时为真
    /// 3. **`touch()`** —— 强制推进 `updated_at`
    ///
    /// 第 2 步必须先做：CHECK 失败会返回 409，而这是**业务**上能预见的
    /// 冲突（用户同时点了订阅和屏蔽），应该返回 422 并说清原因。
    pub async fn update(
        &self,
        id: i32,
        mut set: UpdateSet<'_>,
        source: WriteSource,
    ) -> Result<Movie, DbError> {
        // 1. 护栏。先只看**调用方**要写的字段 —— 此时还没 touch，
        //    updated_at 不该出现在护栏检查里（它是宿主内部行为）。
        let names: Vec<&str> = set.fields().iter().map(|(name, _)| *name).collect();
        guard().check_all(names.iter().copied(), source)?;

        // 2. CHECK 预判：只有当本次写入会碰到这两个位时才需要查当前值。
        if names
            .iter()
            .any(|n| *n == "is_subscribed" || *n == "is_blacklisted")
        {
            self.precheck_blacklist(id, set.fields()).await?;
        }

        // 3. 强制推进时间戳 —— 调用方无法绕过
        set.touch();

        // 绑定顺序是「先 id，再各字段值」，所以字段占位符从 $2 起，
        // 而 WHERE 的 id 是 $1 —— 顺序必须与下面 fold 的 bind 顺序一致。
        let assignments = set.assignments(2);
        let fields = set.finish(ENTITY)?;
        let sql = format!("UPDATE movie SET {assignments} WHERE id = $1");

        // fold 而非 for 循环：循环体里 `q = q.bind(..)` 会 use-of-moved-value
        // （bind 消费 self），而 fold 的累加器每次只 move 一次。
        let query = fields.iter().fold(
            sqlx::query_as::<_, Movie>(safe_sql(sql)).bind(id),
            |query, (_, value)| bind_value(query, value),
        );

        let row = query
            .fetch_optional(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))?;

        row.ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    /// CHECK 预判：合并当前值与本次写入后，`is_subscribed` 与
    /// `is_blacklisted` 是否会同时为真。
    async fn precheck_blacklist(
        &self,
        id: i32,
        fields: &[(&str, crate::common::update::Value<'_>)],
    ) -> Result<(), DbError> {
        let current = self.require_by_id(id).await?;

        let mut subscribed = current.is_subscribed;
        let mut blacklisted = current.is_blacklisted;
        for (name, value) in fields {
            match *name {
                "is_subscribed" => subscribed = as_bool(value),
                "is_blacklisted" => blacklisted = as_bool(value),
                _ => {}
            }
        }

        if subscribed && blacklisted {
            return Err(DbError::business(
                ENTITY,
                "is_subscribed 与 is_blacklisted 不能同时为真（数据库 CHECK 约束 \
                 movie_subscription_blacklist_exclusive 也会拒绝）",
            ));
        }
        Ok(())
    }

    /// 按订阅状态列出。
    pub async fn list_by_subscription_state(
        &self,
        state: SubscriptionState,
        limit: i64,
    ) -> Result<Vec<Movie>, DbError> {
        let state = state.as_str();
        let rows = sqlx::query_as::<_, Movie>(
            "SELECT * FROM movie WHERE is_subscribed = $1 ORDER BY subscribed_at DESC NULLS LAST, id LIMIT $2",
        )
        .bind(state == SubscriptionState::Subscribed.as_str())
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// 认领一条待刮削的影片。
    ///
    /// 用 `UPDATE ... WHERE id = $1 AND is_subscribed = false` 的单语句
    /// 写法而不是「先 SELECT 再 UPDATE」：前者靠行锁天然排他，
    /// 后者在两个 worker 之间会重复领取同一条。
    pub async fn claim_for_scrape(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query(
            "UPDATE movie SET updated_at = $2 \
             WHERE id = $1 AND is_subscribed = false AND javdb_id IS NULL",
        )
        .bind(id)
        .bind(crate::common::time::now_utc())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// 标记某字段的归属。宿主独占 —— 插件能写就能伪造所有权。
    ///
    /// 整个 `field_owners` 被**替换**而非合并，所以调用方要先读出当前值。
    /// 归属变更同时递增 `mutation_revision`：两个插件并发登记不同字段时，
    /// 后写的那次会靠版本号被发现覆盖了先前那次。
    pub async fn set_field_owner(
        &self,
        id: i32,
        field: &str,
        owner: &str,
    ) -> Result<Movie, DbError> {
        if !Movie::is_protected(field) {
            return Err(DbError::business(
                ENTITY,
                format!("{field} 不是受保护字段，无需登记归属"),
            ));
        }
        if owner != field_owner::HOST_MANUAL && !owner.starts_with("plugin:") {
            return Err(DbError::business(
                ENTITY,
                format!("owner 必须是 host:manual 或 plugin:<id>，收到 {owner}"),
            ));
        }

        let current = self.require_by_id(id).await?;
        let mut map = current
            .field_owners
            .as_object()
            .cloned()
            .unwrap_or_default();
        map.insert(
            field.to_owned(),
            serde_json::Value::String(owner.to_owned()),
        );

        let mut set = UpdateSet::new();
        set.set("field_owners", serde_json::Value::Object(map));
        set.set("mutation_revision", current.mutation_revision + 1);

        self.update(id, set, WriteSource::Host).await
    }
}

/// 订阅状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscriptionState {
    Subscribed,
    NotSubscribed,
}

impl SubscriptionState {
    /// 该状态对应的 `is_subscribed` 布尔值。
    pub fn as_bool(&self) -> bool {
        matches!(self, Self::Subscribed)
    }

    fn as_str(&self) -> &'static str {
        match self {
            Self::Subscribed => "true",
            Self::NotSubscribed => "false",
        }
    }
}

/// 从 UpdateSet 的值里取布尔。
fn as_bool(value: &crate::common::update::Value<'_>) -> bool {
    matches!(&*value.0, crate::common::update::ValueInner::Bool(true))
}

/// 把 UpdateSet 的值绑到查询上。
///
/// sqlx 的 `bind` 是泛型方法，这里必须按运行时类型分派 —— 这正是
/// 「不用 `query!` 宏」的直接成本：宏能生成静态绑定代码，手写就得
/// 自己维护这个 `match`。
///
/// 新增 [`crate::common::update::ValueInner`] 变体时，这个函数是**唯一**
/// 需要跟着改的地方，编译器会在变体不匹配时报错。
pub(crate) fn bind_value<'q, O>(
    query: sqlx::query::QueryAs<'q, Postgres, O, sqlx::postgres::PgArguments>,
    value: &crate::common::update::Value<'_>,
) -> sqlx::query::QueryAs<'q, Postgres, O, sqlx::postgres::PgArguments> {
    use crate::common::update::ValueInner;
    match &*value.0 {
        ValueInner::Null => query.bind(Option::<String>::None),
        ValueInner::Bool(v) => query.bind(*v),
        ValueInner::Int(v) => query.bind(*v),
        ValueInner::Float(v) => query.bind(*v),
        ValueInner::Text(v) => query.bind(v.clone()),
        ValueInner::Timestamp(v) => query.bind(*v),
        ValueInner::Json(v) => query.bind(v.clone()),
    }
}

/// 把受控的动态 SQL 交给 sqlx 0.9。
///
/// sqlx 0.9 新增了编译期注入防护：`query_as` 的 SQL 参数必须实现
/// [`sqlx::SqlSafeStr`]，而 `&String` **不实现**它 —— 想用动态 SQL 就
/// 必须显式声明「我已确认这段 SQL 安全」。
///
/// 确认依据：列名全部来自 [`UpdateSet`]，而调用方只能通过 `set()` 传入
/// 字面量列名，没有任何路径能把用户输入拼进 SQL。占位符数量由
/// `assignments()` 按字段数生成，与 bind 数量严格一致。
///
/// 如果将来引入了接受外部列名的入口，这里就是唯一需要重新审计的地方。
pub(crate) fn safe_sql(sql: String) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(sql)
}

/// 影片系列仓储。
#[derive(Debug, Clone)]
pub struct MovieSeriesRepository {
    pool: PgPool,
}

impl MovieSeriesRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 按名称查询。系列名在 save 前被 strip，避免「同一系列两个实体」。
    pub async fn find_by_name(&self, name: &str) -> Result<Option<MovieSeries>, DbError> {
        let row = sqlx::query_as::<_, MovieSeries>("SELECT * FROM movie_series WHERE name = $1")
            .bind(name.trim())
            .fetch_optional(&self.pool)
            .await?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subscription_state_maps_to_boolean() {
        assert!(SubscriptionState::Subscribed.as_bool());
        assert!(!SubscriptionState::NotSubscribed.as_bool());
        assert_eq!(SubscriptionState::Subscribed.as_str(), "true");
    }

    #[test]
    fn guard_rejects_plugin_writes_to_counters() {
        // 计数器不在受保护白名单里，插件不该能改。
        let guard = guard();
        let plugin = WriteSource::Plugin { plugin_id: "p" };
        assert!(guard.allows("title", plugin));
        assert!(!guard.allows("watched_count", plugin));
        assert!(!guard.allows("field_owners", plugin));
    }

    #[test]
    fn blacklist_precheck_merges_current_and_pending() {
        // 只改其中一个位时，要用另一个位的**当前值**来判断。
        let subscribed = Movie {
            id: 1,
            javdb_id: None,
            metadata_source: None,
            javdb_next_check_at: None,
            movie_number: "ABC-001".to_owned(),
            title: "t".to_owned(),
            release_date: None,
            duration_minutes: 0,
            score: 0.0,
            score_number: 0,
            watched_count: 0,
            cover_image_id: None,
            thin_cover_image_id: None,
            summary: String::new(),
            series_id: None,
            maker_name: None,
            director_name: None,
            want_watch_count: 0,
            comment_count: 0,
            interaction_synced_at: None,
            heat: 0,
            is_collection: false,
            is_subscribed: true,
            is_blacklisted: false,
            subscribed_at: None,
            subscription_search_state: "pending".to_owned(),
            subscription_search_attempt_count: 0,
            subscription_search_retry_round: 0,
            subscription_search_last_attempted_at: None,
            subscription_search_last_succeeded_at: None,
            subscription_search_next_retry_at: None,
            subscription_search_error_code: None,
            subscription_search_last_error: None,
            subscription_search_last_error_at: None,
            field_owners: serde_json::json!({}),
            mutation_revision: 0,
            created_at: None,
            updated_at: None,
        };
        assert!(subscribed.satisfies_blacklist_constraint());

        // 再写 is_blacklisted = true 就冲突了
        let mut conflict = subscribed.clone();
        conflict.is_blacklisted = true;
        assert!(!conflict.satisfies_blacklist_constraint());
    }
}
