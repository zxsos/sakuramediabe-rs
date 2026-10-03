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
/// 读 `SMDB_TEST_DATABASE_URL`，回退到 `DATABASE_URL`。
///
/// 单独抽出来是因为 `TestDb` 的清理需要**独立于当前 runtime** 建立
/// 新连接，见 [`TestDb`] 的 `Drop` 实现。
pub fn test_database_url() -> Option<String> {
    std::env::var("SMDB_TEST_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .ok()
}

/// 拿到测试连接池，拿不到返回 `None`。
pub async fn maybe_pool() -> Option<PgPool> {
    let url = test_database_url()?;
    let options = PgConnectOptions::from_str(&url).ok()?;

    // max_connections=1：`search_path` 是**会话级**设置，只有单一连接
    // 才能保证 DDL 之后的所有查询都落在同一个测试 schema 里。
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
    /// 连接串，仅供 `Drop` 里的清理路径另建连接使用。
    url: String,
}

impl TestDb {
    /// 创建隔离 schema 并应用 DDL。
    ///
    /// 拿不到连接时返回 `None` —— 测试应直接 `return`，算作通过。
    pub async fn create() -> Option<Self> {
        let pool = test_pool().await?;
        let url = test_database_url()?;
        let schema = format!("smdb_test_{}", unique_suffix());

        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&pool)
            .await
            .expect("CREATE SCHEMA 失败（需要 CREATE 权限）");

        let db = Self { pool, schema, url };
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
        let url = self.url.clone();
        let schema = self.schema.clone();

        // 清理必须**在 Drop 返回前完成**，否则每次跑测试都会留下一批
        // 废弃 schema（实测 31 个测试留下 64 个，跑得越多库里越脏）。
        //
        // 两条走不通的路都试过：
        //
        // - `tokio::spawn`：测试主体结束时 runtime 已进入 shutdown，
        //   spawn 出去的任务不会被 poll，全部残留。
        // - `block_in_place` + `Handle::block_on`：`#[tokio::test]`
        //   默认是 **current_thread** runtime，而 `block_in_place`
        //   只在 multi_thread 下可用，会 panic。
        //
        // 所以走完全独立的路径：另开一个 OS 线程，在**自己的** runtime
        // 里新建连接执行 DROP，并 `join` 等它结束。新连接的代价是每个
        // 测试多一次握手（毫秒级），换来的是「Drop 返回即已清干净」
        // 这个可验证的性质。
        let worker = std::thread::spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                use sqlx::Connection;

                // 清理本身要和所有测试抢连接：本机 `max_connections=20`
                // 而 `cargo test` 默认按 CPU 数并行（这里是 16），峰值时
                // 16 个测试各持一个连接，这里再开一个就正好撞上限 ——
                // `connect` 失败，schema 残留。
                //
                // 所以退避重试：残留一个 schema 的代价（库里长期堆积
                // 40 张废弃表，远超几次握手的开销）远大于多等几百毫秒。
                for (attempt, backoff_ms) in [0u64, 40, 160, 500].into_iter().enumerate() {
                    if attempt > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
                    }

                    let Ok(mut conn) = sqlx::PgConnection::connect(&url).await else {
                        continue;
                    };
                    let dropped = sqlx::raw_sql(sqlx::AssertSqlSafe(format!(
                        "DROP SCHEMA IF EXISTS {schema} CASCADE"
                    )))
                    .execute(&mut conn)
                    .await
                    .is_ok();
                    let _ = conn.close().await;

                    if dropped {
                        return;
                    }
                }

                // 四次都失败：只能留给调用者兜底，但必须可见 ——
                // 静默残留正是这个 bug 当初的成因。
                eprintln!(
                    "WARN: 未能清理测试 schema {schema}；\
                     可执行 DROP SCHEMA {schema} CASCADE 手动清理"
                );
            });
        });

        // join 而非 detach：进程可能在 detach 的任务完成前就退出，
        // 那和 spawn 一样留残留。测试收尾时阻塞几毫秒是值得的。
        let _ = worker.join();
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
