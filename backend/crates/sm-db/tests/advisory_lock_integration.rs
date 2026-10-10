//! 会话级 advisory lock 的集成测试。
//!
//! # 为什么这些断言只能在真库上做
//!
//! `pg_try_advisory_lock` 的语义**完全由 PostgreSQL 实现**，而它的三个关键
//! 性质都是「跨会话」的：
//!
//! 1. 锁在**会话**上，不在事务上 —— 同一条连接反复取锁会成功，换一条就失败。
//! 2. 会话关闭（连接关闭）时锁**自动释放** —— 这是 `Drop` 兜底的基础。
//! 3. `pg_advisory_unlock` 只对**自己那条会话**上的锁有效。
//!
//! 用内存实现或 mock 替掉数据库，这三条全部「通过」，而真实并发下会出现
//! 「两个请求同时操作同一个媒体」。
//!
//! # 最重要的一条：`dropping_the_guard_does_not_leak_the_lock_into_the_pool`
//!
//! 那条模拟**异常路径**（`?` 提前返回 / panic → `Drop`）。若 `Drop` 只是把
//! 连接归还池，锁会跟着连接进池、被下一个请求继承 —— 那个资源**永久繁忙**，
//! 症状是「偶发 409，重启就好」。这是本模块存在的全部理由。

use sm_db::common::advisory_lock::{namespace, AdvisoryLock};
use sm_db::testing::TestDb;

/// **全库唯一**的 `resource_id`。
///
/// # 为什么不能用「进程内小计数器」
///
/// advisory lock 的命名空间是**整个数据库** —— `pg_locks` 里没有 schema
/// 概念，`pg_try_advisory_lock(17001, 1)` 与测试 schema 无关。所以两个
/// **并发跑的测试二进制**（`cargo test --workspace` 会并行跑多个
/// integration suite）只要都用 id = 1，就会互相挡住。
///
/// 症状是**偶发**的取锁失败：`.expect("lock")` 挂在「这个 id 明明只有我
/// 在用」的那一个上，而且重跑就绿 —— 正是最难定位的那类缺陷。
///
/// # 取值范围
///
/// 上游的约束是 `0 < id < 2**31`（两条表都是 serial / 有符号 int32）。
/// 这里用「20 位微秒计数 + 11 位进程内序号」拼出 31 位：
///
/// ```text
/// 最大 = ((2^20 - 1) << 11) | 0x7FF = 2^31 - 1   <- 含端点
/// ```
///
/// 微秒取模到 20 位，所以 71 分钟后会回绕 —— 那个尺度上并发跑的 suite
/// 早已结束，而序号位保证同一微秒内的多个 id 仍互不相同。
fn unique_id() -> i32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    let micros = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_micros() % (1 << 20))
        .unwrap_or(0);
    // `subsec_micros()` 已经是 u32，不需要再 cast。
    let id = (micros << 11) | (n & 0x7FF);
    // `.max(1)` 兜住「微秒为 0 且序号为 0」这一个点。
    (id as i32).max(1)
}

/// 查该资源上是否**有**锁被授予。
///
/// 用 `pg_locks` 而不是「再取一次锁」：后者会改变状态（取到就意味着我们
/// 拿到了锁），而 `pg_locks` 是纯观察。
///
/// `classid` / `objid` 的角色：PostgreSQL 把两个 int32 参数塞进 int64 的
/// lock id，高 32 位进 `classid`、低 32 位进 `objid`。
async fn is_lock_held(pool: &sqlx::PgPool, ns: i32, resource_id: i32) -> bool {
    let held: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_locks \
         WHERE locktype = 'advisory' AND classid = $1 AND objid = $2 AND granted",
    )
    .bind(ns)
    .bind(resource_id)
    .fetch_one(pool)
    .await
    .expect("query pg_locks");
    held > 0
}

/// advisory lock 的语义**跨会话**，而 `TestDb` 的池是 `max_connections(1)`
/// （`search_path` 是会话级设置）。用它验「第二个持有者被拒」会拿到
/// **误导性的通过** —— 同一会话重复 `pg_try_advisory_lock` 总是成功。
///
/// 所以这批测试统一用一个多连接的池。生产是 20（`sm_server::config`）。
/// 理由见 [`TestDb::pool_with_max_connections`]。
async fn lock_pool(db: &TestDb) -> sqlx::PgPool {
    // **2 条，恰好够用，不要调大。**
    //
    // 这批测试最多同时需要「一条持锁 + 一条观察/抢锁」。而 `cargo test`
    // 默认按 CPU 数并行（这台机器 16），16 个测试各开一个池就会撞上
    // PostgreSQL 服务端的 `max_connections = 20` —— `connect` 失败、
    // 下面那个 `expect` panic，表现为**偶发**的测试失败。
    // `TestDb::drop` 的注释已经记过这个坑（它为清理也要抢一条连接）。
    db.pool_with_max_connections(2).await
}

// ================================================================ 基本互斥

/// 同一个资源：第一个拿到，第二个拿不到。
#[tokio::test]
async fn a_second_holder_of_the_same_resource_is_refused() {
    let db = TestDb::require().await;
    // 跨会话语义 -> 必须多连接池，见 `lock_pool` 的文档。
    let pool = lock_pool(&db).await;
    let id = unique_id();

    let first = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, id)
        .await
        .expect("first acquire")
        .expect("第一个应当拿到锁");
    assert_eq!(first.resource_id(), id);

    // 同进程的第二条**连接** —— advisory lock 是会话级的，
    // 同一会话重复取会成功，所以必须借另一条连接。
    let second = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, id)
        .await
        .expect("second acquire");
    assert!(
        second.is_none(),
        "同一资源被占用时必须返回 None（→ 409），而不是阻塞等待"
    );

    first.release().await;
}

/// 不同资源互不干扰。
#[tokio::test]
async fn different_resources_do_not_block_each_other() {
    let db = TestDb::require().await;
    // 跨会话语义 -> 必须多连接池，见 `lock_pool` 的文档。
    let pool = lock_pool(&db).await;
    // 两个 id 都要唯一，且不能相邻到撞上别的 suite
    let a = unique_id();
    let mut b = unique_id();
    while b == a {
        b = unique_id();
    }

    let first = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, a)
        .await
        .expect("acquire a")
        .expect("a 应当拿到");
    let other = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, b)
        .await
        .expect("acquire b");
    assert!(other.is_some(), "不同资源必须能同时取锁");
    if let Some(other) = other {
        other.release().await;
    }
    first.release().await;
}

/// **命名空间**隔离：`media.id == 1` 与 `media_library.id == 1` 不互相挡。
#[tokio::test]
async fn the_two_namespaces_are_independent() {
    let db = TestDb::require().await;
    // 跨会话语义 -> 必须多连接池，见 `lock_pool` 的文档。
    let pool = lock_pool(&db).await;
    let id = unique_id();

    let media = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, id)
        .await
        .expect("media")
        .expect("媒体锁应拿到");
    let library = AdvisoryLock::try_acquire(&pool, namespace::LIBRARY, id)
        .await
        .expect("library");
    assert!(
        library.is_some(),
        "同一个 id 在不同命名空间下必须能各取一次 —— 否则 media.id=1 会挡住 \
         media_library.id=1"
    );
    if let Some(library) = library {
        library.release().await;
    }
    media.release().await;
}

// ================================================================ 释放

/// 显式释放后**能再取到**。
#[tokio::test]
async fn releasing_lets_the_next_holder_in() {
    let db = TestDb::require().await;
    let id = unique_id();

    let first = AdvisoryLock::try_acquire(db.pool(), namespace::MEDIA, id)
        .await
        .expect("first")
        .expect("first");
    first.release().await;

    let second = AdvisoryLock::try_acquire(db.pool(), namespace::MEDIA, id)
        .await
        .expect("second")
        .expect("释放之后必须能再拿到");
    second.release().await;
}

/// 释放之后 `pg_locks` 里不再有那条锁。
#[tokio::test]
async fn releasing_clears_the_lock_in_pg_locks() {
    let db = TestDb::require().await;
    // 跨会话语义 -> 必须多连接池，见 `lock_pool` 的文档。
    let pool = lock_pool(&db).await;
    let id = unique_id();

    let lock = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, id)
        .await
        .expect("acquire")
        .expect("lock");
    assert!(
        is_lock_held(&pool, namespace::MEDIA, id).await,
        "取锁后 pg_locks 里应该有它"
    );

    lock.release().await;
    assert!(
        !is_lock_held(&pool, namespace::MEDIA, id).await,
        "释放后 pg_locks 里不该还有它"
    );
}

// ================================================================ Drop 兜底

/// **最重要的一条**：守卫被直接 drop（异常路径）时，锁**不能**留在池里。
///
/// 机制是 `PoolConnection::detach()`：连接被摘出池、随即关闭，PostgreSQL
/// 因会话结束而自动释放该会话上的全部 advisory lock。
#[tokio::test]
async fn dropping_the_guard_does_not_leak_the_lock_into_the_pool() {
    let db = TestDb::require().await;
    // 跨会话语义 -> 必须多连接池，见 `lock_pool` 的文档。
    let pool = lock_pool(&db).await;
    let id = unique_id();

    // 模拟 `?` 提前返回 / panic：守卫离开作用域，不调 `release`。
    {
        let lock = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, id)
            .await
            .expect("acquire")
            .expect("lock");
        assert_eq!(lock.resource_id(), id);
        // 不 release，直接离开作用域
    }

    assert!(
        !is_lock_held(&pool, namespace::MEDIA, id).await,
        "守卫 drop 后锁必须已被释放 —— 会话关闭时 PostgreSQL 自动释放"
    );

    // 真正的检验：池里**没有**一条连接带着这把锁。多借几次，覆盖池的复用。
    for _ in 0..8 {
        let again = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, id)
            .await
            .expect("acquire again");
        assert!(
            again.is_some(),
            "锁泄漏的表现就在这里：借遍池里的连接都取不到锁"
        );
        if let Some(again) = again {
            again.release().await;
        }
    }
}

/// 守卫活着时 `pg_locks` 里能看到它 —— 与上面那条互为对照。
#[tokio::test]
async fn a_live_guard_is_visible_in_pg_locks() {
    let db = TestDb::require().await;
    // 跨会话语义 -> 必须多连接池，见 `lock_pool` 的文档。
    let pool = lock_pool(&db).await;
    let id = unique_id();
    let lock = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, id)
        .await
        .expect("acquire")
        .expect("lock");
    assert!(is_lock_held(&pool, namespace::MEDIA, id).await);
    lock.release().await;
}

/// 反复「取了就 drop」20 次，每次都必须能重新取到。
///
/// 一次 drop 泄漏了锁，第二次就会失败 —— 所以这个循环是上面那条的放大版，
/// 且顺带覆盖了「池在不同连接之间轮换」的情形。
#[tokio::test]
async fn repeated_acquire_and_drop_never_accumulates_locks() {
    let db = TestDb::require().await;
    // 跨会话语义 -> 必须多连接池，见 `lock_pool` 的文档。
    let pool = lock_pool(&db).await;
    let id = unique_id();

    for round in 0..20 {
        let lock = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, id)
            .await
            .unwrap_or_else(|e| panic!("第 {round} 轮取锁失败：{e}"))
            .unwrap_or_else(|| panic!("第 {round} 轮取不到锁 —— 前某一轮的 drop 泄漏了它"));
        drop(lock);
    }
    assert!(
        !is_lock_held(&pool, namespace::MEDIA, id).await,
        "20 轮之后不该残留任何锁"
    );
}

// ================================================================ id 校验

/// 非正的 `resource_id` 被拒。上界（`2^31 - 1`）由 `i32` 类型保证。
///
/// `i32::MAX` **不在**这个列表里 —— 它是合法 id。
#[tokio::test]
async fn a_non_positive_resource_id_is_rejected() {
    let db = TestDb::require().await;
    for bad in [0, -1, i32::MIN] {
        let err = AdvisoryLock::try_acquire(db.pool(), namespace::MEDIA, bad)
            .await
            .expect_err("非正 id 应被拒");
        assert!(
            err.to_string().contains("必须是正整数"),
            "{bad} 的错误信息应说明下界，实际 {err}"
        );
    }
}
