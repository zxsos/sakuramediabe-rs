//! 集成测试夹具。
//!
//! DDL 在**编译期**用 `include_str!` 内嵌，所以测试不依赖外部文件，
//! 也不怕工作目录不对。改 schema 只需重跑 `gen_ddl.py` 然后重新编译。

use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use std::str::FromStr;

/// 内嵌的 DDL，由 `parity/gen_ddl.py` 从 Peewee 模型生成。
pub const SCHEMA_SQL: &str = include_str!("../../../../docker/schema.sql");

/// 拿到测试连接池，拿不到返回 `None`。
///
/// 读 `SMDB_TEST_DATABASE_URL`，回退到 `DATABASE_URL`。两个都没有时
/// 返回 `None` —— 调用方应据此跳过测试而不是 panic。
pub async fn maybe_pool() -> Option<PgPool> {
    let url = std::env::var("SMDB_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()?;

    let options = PgConnectOptions::from_str(&url).ok()?;

    // max_connections=1：schema 是共享的，并发连接会互相看到对方的
    // search_path 状态。测试本身是串行的，池大没有意义。
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect_with(options)
        .await
        .ok()?;

    Some(pool)
}

/// 拿到测试连接池，缺配置时跳过整个测试。
///
/// 打印一行原因便于 `--nocapture` 下看清「为什么没跑」。
pub async fn test_pool() -> Option<PgPool> {
    match maybe_pool().await {
        Some(pool) => Some(pool),
        None => {
            eprintln!(
                "SKIP: 未设置 SMDB_TEST_DATABASE_URL / DATABASE_URL —— \
                 集成测试需要 PostgreSQL"
            );
            None
        }
    }
}

/// 一次性测试库：独立 schema + 建表 + 析构时清理。
pub struct TestDb {
    pool: PgPool,
    schema: String,
}

impl TestDb {
    /// 创建隔离 schema 并应用 DDL。
    ///
    /// 拿不到连接时返回 `None` —— 测试应直接 `return`，算作通过。
    pub async fn create() -> Option<Self> {
        let pool = test_pool().await?;
        let schema = format!("smdb_test_{}", unique_suffix());

        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&pool)
            .await
            .expect("CREATE SCHEMA 失败（需要 CREATE 权限）");

        let db = Self { pool, schema };
        db.apply_schema().await;
        Some(db)
    }

    /// 把 DDL 应用到测试 schema。
    async fn apply_schema(&self) {
        // DDL 里的表名不带 schema 前缀，靠 search_path 落位。
        // schema 名由我们生成，DDL 是编译期内嵌的常量，两者都无需审计。
        let setup = format!("SET search_path TO {};\n{SCHEMA_SQL}", self.schema);
        sqlx::raw_sql(sqlx::AssertSqlSafe(setup))
            .execute(&self.pool)
            .await
            .expect("应用 schema.sql 失败");
    }

    /// 底层连接池，search_path 已指向测试 schema。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// schema 名，用于断言信息。
    pub fn schema(&self) -> &str {
        &self.schema
    }

    /// 重新应用 DDL（清空所有数据但保留结构）。
    pub async fn reset(&self) {
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "DROP SCHEMA {} CASCADE",
            self.schema
        )))
        .execute(&self.pool)
        .await
        .expect("DROP SCHEMA 失败");
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
            "CREATE SCHEMA {}",
            self.schema
        )))
        .execute(&self.pool)
        .await
        .expect("CREATE SCHEMA 失败");
        self.apply_schema().await;
    }
}

impl Drop for TestDb {
    fn drop(&mut self) {
        // 同步清理：Drop 里不能 await。用后台任务，但 pool 可能已被
        // drop，所以克隆一个连接先绑好。
        let pool = self.pool.clone();
        let schema = self.schema.clone();
        tokio::spawn(async move {
            let _ = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                "DROP SCHEMA IF EXISTS {schema} CASCADE"
            )))
            .execute(&pool)
            .await;
            pool.close().await;
        });
    }
}

/// 公开的 DDL 应用入口，供需要手动建表的测试使用。
pub async fn apply_schema(pool: &PgPool, schema: &str) -> Result<(), sqlx::Error> {
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "CREATE SCHEMA IF NOT EXISTS {schema};"
    )))
    .execute(pool)
    .await?;
    sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
        "SET search_path TO {schema};\n{SCHEMA_SQL}"
    )))
    .execute(pool)
    .await?;
    Ok(())
}

/// 随机后缀。用纳秒时间戳 + 进程内计数器，避免同刻并发撞名。
fn unique_suffix() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{nanos:x}{n:x}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_is_embedded_and_not_empty() {
        // include_str! 保证测试不依赖工作目录；空文件说明生成器没跑。
        assert!(SCHEMA_SQL.len() > 1000, "schema.sql 看起来是空的");
        assert!(SCHEMA_SQL.contains("CREATE TABLE"));
    }

    #[test]
    fn ddl_contains_the_three_hard_tables() {
        for table in ["movie", "media", "download_task"] {
            assert!(
                SCHEMA_SQL.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
                "DDL 缺少 {table}"
            );
        }
    }

    #[test]
    fn blacklist_check_survived_generation() {
        // 这是 Movie 仓储预判的那条约束，必须真的在 DDL 里。
        assert!(SCHEMA_SQL.contains("movie_subscription_blacklist_exclusive"));
        assert!(SCHEMA_SQL.contains("CHECK (NOT (is_subscribed AND is_blacklisted))"));
    }

    #[test]
    fn unique_suffix_does_not_repeat() {
        let a = unique_suffix();
        let b = unique_suffix();
        assert_ne!(a, b, "并发测试不能撞 schema 名");
    }
}
