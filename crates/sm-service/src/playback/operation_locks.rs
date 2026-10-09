//! 媒体 I/O 与库配置的短时独占，对应上游
//! `src/service/playback/operation_locks.py`（52 行）。
//!
//! # 它解决的问题
//!
//! 「同一个媒体 / 同一个媒体库」同时被两个请求操作时，第二个必须立刻失败
//! 而不是排队等 —— 一个正在解压的视频文件和另一个正在删除它，排队会等到
//! 状态已经变了才执行。advisory lock 让这种冲突在**第一条语句**就暴露成
//! 409，客户端提示「稍后重试」。
//!
//! # 机制在 `sm_db::common::advisory_lock`
//!
//! 本模块只做三件事：把上游的 `with` 语法变成 Rust 的 RAII、把「拿不到锁」
//! 映射成上游那个**专属**的 409、以及导出两个命名空间常量。
//!
//! # 为什么 409 那个码不能复用
//!
//! `media_operation_busy` 与「名称冲突」同样是 409，但客户端要能分辨：
//! 前者「等一下就好」，后者「改个名字」。那个错误码是上游定下的
//! `ApiError(409, "media_operation_busy", "媒体或媒体库正在处理，请稍后重试")`，
//! 逐字沿用。
//!
//! # 锁泄漏是这个模块最容易出的错
//!
//! PostgreSQL 的会话级 advisory lock + **连接池** = 一个真实的泄漏路径
//! （连接带着锁归还池，被下一个请求继承）。机制层用「守卫持有连接」+
//! `Drop` 里 `detach()` 两条规则消掉它，细节见
//! [`sm_db::common::advisory_lock`] 的模块文档 —— 那里写清了为什么不能
//! 简单地 `Drop` 归还。

use sm_db::common::advisory_lock::{namespace, AdvisoryLock};
use sm_db::Db;

use crate::error::ServiceError;

/// 上游的错误码。逐字沿用。
pub const MEDIA_OPERATION_BUSY: &str = "media_operation_busy";

/// 上游的消息。逐字沿用。
pub const MEDIA_OPERATION_BUSY_MESSAGE: &str = "媒体或媒体库正在处理，请稍后重试";

/// 正在进行的媒体操作。
///
/// 构造成功即**持有**锁；[`Drop`] 之后锁已释放（正常路径显式、异常路径
/// 由 `AdvisoryLock` 的 `Drop` 兜底）。
///
/// # 为什么不把「拿不到」做成构造函数里的错误
///
/// 上游是 `with media_operation_lock(...)` —— 拿不到锁时**在进入 with 之前**
/// 就抛 `MediaOperationBusy`。Rust 里守卫的构造只能返回 `Result<Self, E>`，
/// 而这里需要区分两种失败：
///
/// | 情况 | 含义 | 该映射成 |
/// |---|---|---|
/// | `Err`（拿不到连接） | 数据库层面的问题 | 500 |
/// | `Ok(None)`（锁被占） | 预期内的并发结果 | 409 |
///
/// 把它们塞进同一个 `Err` 就得让调用方去分辨错误类型，而那正是「把 409
/// 报成 500」的来源。所以这里用 `Result<Option<Self>>`。
#[derive(Debug)]
pub struct MediaOperation {
    lock: AdvisoryLock,
}

impl MediaOperation {
    /// 独占某个**媒体**。
    pub async fn try_media(db: &Db, media_id: i32) -> Result<Option<Self>, ServiceError> {
        Ok(AdvisoryLock::try_acquire(db, namespace::MEDIA, media_id)
            .await?
            .map(|lock| Self { lock }))
    }

    /// 独占某个**媒体库**（配置 / 清单变更）。
    ///
    /// 命名空间与媒体分开 —— 否则 `media.id == 1` 与
    /// `media_library.id == 1` 会互相挡住（见 `namespace` 的文档）。
    pub async fn try_library(db: &Db, library_id: i32) -> Result<Option<Self>, ServiceError> {
        Ok(
            AdvisoryLock::try_acquire(db, namespace::LIBRARY, library_id)
                .await?
                .map(|lock| Self { lock }),
        )
    }

    /// 底层连接。做媒体 I/O 时**必须**用它 —— 那些语句要落在持锁的那条
    /// 会话上，否则锁没有真正保护任何东西。
    pub fn connection(&mut self) -> &mut sqlx::PgConnection {
        self.lock.connection()
    }

    /// 被占用的资源 id。用于日志与错误详情。
    pub fn resource_id(&self) -> i32 {
        self.lock.resource_id()
    }

    /// 显式释放。**不调也行** —— `Drop` 会兜底，但显式释放能让连接回到池里
    /// 复用，而 `Drop` 路径会**摘掉**那条连接（见类型文档）。
    pub async fn release(self) {
        self.lock.release().await;
    }
}

/// 「锁被占着」→ 409 信封。
///
/// 单独一个函数而不是让每个调用方自己拼：那个 `details` 的形状
/// （`{resource_id}`）是客户端要读的。
pub fn busy_error(resource_id: i32) -> ServiceError {
    ServiceError::conflict(
        MEDIA_OPERATION_BUSY,
        MEDIA_OPERATION_BUSY_MESSAGE,
        Some(crate::error::details_of("resource_id", resource_id)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_error_code_and_message_are_verbatim_upstream() {
        // 客户端可能按 code 分支、按文案做本地化映射，改字面量等于改契约
        assert_eq!(MEDIA_OPERATION_BUSY, "media_operation_busy");
        assert_eq!(
            MEDIA_OPERATION_BUSY_MESSAGE,
            "媒体或媒体库正在处理，请稍后重试"
        );
    }

    /// 409 与其它 409（名称冲突）的区分靠 `code`，所以它不能被复用。
    #[test]
    fn the_busy_code_is_not_a_generic_conflict() {
        let err = busy_error(7);
        assert_eq!(err.status, 409, "409 = 换个时机就好");
        assert_eq!(err.code(), MEDIA_OPERATION_BUSY);
        assert_eq!(err.api.message, MEDIA_OPERATION_BUSY_MESSAGE);
        assert_eq!(
            err.api.details.as_ref().unwrap().get("resource_id"),
            Some(&serde_json::json!(7)),
            "details 要带 resource_id，客户端据此提示是哪个资源忙"
        );
    }
}
