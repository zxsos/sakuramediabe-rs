//! PostgreSQL **会话级 advisory lock** 的 RAII 守卫。
//!
//! # 用途：媒体 I/O 与库配置的短时独占
//!
//! 上游 `src/service/playback/operation_locks.py` 用
//! `pg_try_advisory_lock(namespace, resource_id)` 让「同一个媒体 / 同一个
//! 媒体库」的操作串行化，拿不到锁就抛 409 `media_operation_busy`。
//!
//! 本模块只提供**机制**（拿到 / 释放 / 泄漏兜底）；409 的映射在
//! `sm_service::playback::operation_locks`。
//!
//! # 为什么必须是**会话级**，以及为什么那样很危险
//!
//! PostgreSQL 有两种 advisory lock：
//!
//! | 种类 | 释放时机 |
//! |---|---|
//! | `pg_advisory_lock`（**会话级**） | 显式 `pg_advisory_unlock`，**或连接关闭** |
//! | `pg_advisory_xact_lock`（事务级） | 事务提交 / 回滚 |
//!
//! 上游刻意用会话级：媒体 I/O（下载、解压、写文件）**不在事务里**，
//! 一个横跨几秒的操作没法包在事务中。
//!
//! 而会话级 + **连接池** = 一个真实的陷阱：
//!
//! ```text
//! 1. 从池里借到连接 C，在 C 上取锁
//! 2. 忘记解锁（或解锁失败）
//! 3. C 归还到池
//! 4. 池把 C 发给另一个请求 —— **锁还在 C 的会话上**
//! 5. 那个请求对同一个资源 `pg_try_advisory_lock` 永远失败
//! ```
//!
//! 于是那个资源**永久繁忙**，而症状是「偶发 409，重启就好」——
//! 极难定位。本模块用两条规则消掉它，见下。
//!
//! # 规则一：守卫**持有**那条连接
//!
//! 锁在会话上，所以连接必须活到解锁为止。守卫因此**自己持有**
//! `PoolConnection` —— 解锁只可能在**同一条连接**上生效，而类型保证了
//! 「释放的连接」与「取锁的连接」是同一条。
//!
//! # 规则二：`Drop` 里**detach** 而不是归还
//!
//! 正常路径调 [`AdvisoryLock::release`]：显式解锁，连接干净地归还池。
//!
//! 异常路径（`release` 没被调 —— `?` 提前返回、panic）走 `Drop`：
//! `PoolConnection::detach()` 把连接**从池里摘掉**，于是它被 drop 时
//! **关闭 socket**，而**关闭会话会自动释放该会话上的全部 advisory lock**。
//!
//! 代价是那条连接不能再被复用（要从池里补一条新的），但那
//! **只发生在异常路径**，而「永久繁忙」比「少一条连接」严重得多。
//!
//! 这个取舍是本模块存在的全部理由，所以写在最前面。
//!
//! # 运行约束：池必须**至少有 2 条连接**
//!
//! 守卫在持有期间**占着**一条连接，而调用方通常还要用池做别的事（读进度、
//! 写台账）。所以：
//!
//! ```text
//! 需要连接数 = 同时持有的锁数 + 1
//! ```
//!
//! `max_connections = 1` 的部署会在第一次取锁后**整个池耗尽** —— 表现是
//! 所有请求一起超时，而不是「媒体操作失败」。生产默认 20
//! （`sm_server::config`），余量充足；但把 `pool.max_connections` 调到 1
//! 的部署会踩到它，而症状与原因相隔很远，所以写在这里。
//!
//! 这也是**测试**要单独开多连接池的原因：`TestDb` 的池是
//! `max_connections(1)`（`search_path` 是会话级设置），用它验「第二个持有者
//! 被拒」会拿到**误导性的通过** —— 同一会话重复取锁总是成功。
//! 见 [`crate::testing::TestDb::pool_with_max_connections`]。

use sqlx::pool::PoolConnection;
use sqlx::{PgPool, Postgres};

use crate::error::DbError;

/// `media` 与 `media_library` 共用 int32 serial，所以用**不同的命名空间**
/// 区分两类资源 —— 否则 `media.id == 1` 与 `media_library.id == 1` 会互相
/// 挡住。
///
/// 上游 `operation_locks.py:8-9` 的两个常量，逐字一致。
pub mod namespace {
    /// 媒体 I/O。
    pub const MEDIA: i32 = 17001;
    /// 媒体库配置 / 清单。
    pub const LIBRARY: i32 = 17002;
}

/// 拿不到锁。调用方把它映射成 409。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockUnavailable;

/// 已持有的会话级 advisory lock。
///
/// # 它是 `!Send` 吗
///
/// 不是 —— `PoolConnection` 是 `Send`。所以这个守卫可以跨 `await` 持有，
/// 也可以移到别的任务上。**那意味着它可能被带到别的事务里**，而规则一
/// 仍然成立（连接是它自己那条，与谁在用无关）。
#[derive(Debug)]
pub struct AdvisoryLock {
    /// 必须持有到解锁 —— 见模块文档的「规则一」。
    conn: Option<PoolConnection<Postgres>>,
    namespace: i32,
    resource_id: i32,
}

impl AdvisoryLock {
    /// 尝试取锁。**拿不到就返回 `Ok(None)`，不是 `Err`。**
    ///
    /// 「锁被别人占着」是一个**预期内**的并发结果，不是错误 ——
    /// 上游对应 `MediaOperationBusy`（409），而 409 与 500 的区别对客户端
    /// 是「稍后重试」与「坏了」。
    ///
    /// # `resource_id` 的范围校验
    ///
    /// 上游是 `if not 0 < resource_id < 2**31: raise ValueError`。Python 的
    /// 整数无界，所以那个上界是真能撞到的；**Rust 的 `i32` 做不到** ——
    /// `i32::MAX` 就是 `2^31 - 1`，类型本身保证了上界。
    ///
    /// 所以这里只判下界。**不要**把它「补全」成
    /// `resource_id >= 2_i32.pow(31)`：那个表达式在 debug 构建下**溢出
    /// panic**（`2^31 > i32::MAX`），release 下回绕成 `i32::MIN` 让判断
    /// 变成「比最小值还小」而恒为 false —— 一个恒假的检查比没有检查更糟，
    /// 因为它看起来在校验。
    pub async fn try_acquire(
        pool: &PgPool,
        namespace: i32,
        resource_id: i32,
    ) -> Result<Option<Self>, DbError> {
        // 上界由 `i32` 类型保证（`i32::MAX == 2^31 - 1`），只判下界。
        // 理由见本方法文档的「`resource_id` 的范围校验」。
        if resource_id <= 0 {
            return Err(DbError::business(
                "AdvisoryLock",
                format!("resource_id 必须是正整数，收到 {resource_id}"),
            ));
        }

        let mut conn = pool.acquire().await?;
        let got: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, $2)")
            .bind(namespace)
            .bind(resource_id)
            .fetch_one(&mut *conn)
            .await
            .map_err(|e| DbError::from(e).with_entity("AdvisoryLock"))?;

        if !got {
            // 锁在**别人**的会话上。连接是干净的，直接还回去。
            drop(conn);
            return Ok(None);
        }

        Ok(Some(Self {
            conn: Some(conn),
            namespace,
            resource_id,
        }))
    }

    /// 显式解锁并把连接归还池。**消耗 `self`**，所以不可能解锁两次。
    ///
    /// 拿不到连接时**静默**忽略 —— 那说明连接已经坏了，而连接坏掉时
    /// 会话已经结束、锁已被 PostgreSQL 释放，再发一次 `unlock` 只会得到
    /// `false`。
    pub async fn release(mut self) {
        if let Some(mut conn) = self.conn.take() {
            let _ = sqlx::query("SELECT pg_advisory_unlock($1, $2)")
                .bind(self.namespace)
                .bind(self.resource_id)
                .fetch_one(&mut *conn)
                .await;
        }
    }

    /// 底层连接。做「必须落在同一条连接上」的事时用。
    pub fn connection(&mut self) -> &mut sqlx::PgConnection {
        // `conn` 在构造后一直是 `Some`，除非 `release` 消耗了 self ——
        // 而那时 `&mut self` 不可能存在。所以这里不可达。
        self.conn
            .as_mut()
            .expect("release 消耗 self，之后不可能再借出连接")
    }

    /// 该守卫持有的资源 id。用于日志与错误详情。
    pub fn resource_id(&self) -> i32 {
        self.resource_id
    }
}

impl Drop for AdvisoryLock {
    /// **不**归还连接，而是把它从池里摘掉。
    ///
    /// 见模块文档的「规则二」：这是异常路径的兜底，防止会话级锁泄漏到
    /// 池里、被下一个请求继承。
    ///
    /// 正常路径下这个 `Drop` 也会跑 —— 但那时 `conn` 已经被 `release`
    /// 取走（`take()` 置空），所以这里什么都不做。
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // `detach` 让连接不再被池复用；它随即被 drop，socket 关闭，
            // PostgreSQL 随之释放该会话上的全部 advisory lock。
            drop(conn.detach());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_namespaces_are_the_upstream_ones() {
        // 上游 `operation_locks.py:8-9`。改这两个数字会让存量库上
        // 已在跑的会话持有的锁与新代码对不上 —— 后者会拿到锁，
        // 于是两个请求同时操作同一个资源。
        assert_eq!(namespace::MEDIA, 17001);
        assert_eq!(namespace::LIBRARY, 17002);
        assert_ne!(
            namespace::MEDIA,
            namespace::LIBRARY,
            "两个命名空间必须不同，否则 media.id=1 与 media_library.id=1 互相挡住"
        );
    }

    /// `resource_id` 的范围校验。两条表都用 serial（有符号 int32），
    /// 所以越界一定是「传错了」。
    /// 非正 id 被拒。上界由类型保证，不需要（也不能）在这里检查。
    #[tokio::test]
    async fn a_non_positive_resource_id_is_rejected() {
        // 这条不需要真库：校验发生在取连接之前。
        let pool = PgPool::connect_lazy("postgres://localhost/never_connected").expect("lazy");
        for bad in [0, -1, i32::MIN] {
            let err = AdvisoryLock::try_acquire(&pool, namespace::MEDIA, bad)
                .await
                .expect_err("非正 id 应被拒");
            assert!(
                err.to_string().contains("必须是正整数"),
                "{bad} 的错误信息应说明下界，实际 {err}"
            );
        }
    }

    /// `i32::MAX`（= `2^31 - 1`）是**合法**的 —— 它正好落在上界内。
    ///
    /// 这条钉住「上界是 `2^31 - 1` 而不是 `2^31`」。serial 用满 int32
    /// 在理论上是可能的（那意味着同一张表有 21 亿行），而把上限写成
    /// `2^31` 会让最后一个合法 id 被拒 —— 或者更糟，在 i32 上溢出 panic。
    #[test]
    fn the_upper_bound_is_inclusive_of_i32_max() {
        // 注意用 `i64` 算 `2^31` —— `2_i32.pow(31)` 会**溢出 panic**
        // （`2^31 > i32::MAX`），这正是本文件里要避免的写法。
        assert_eq!(i64::from(i32::MAX), 2_i64.pow(31) - 1);
    }
}
