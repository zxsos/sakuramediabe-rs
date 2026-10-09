//! PostgreSQL 数据访问层。
//!
//! # schema 完全保留
//!
//! 本 crate **不引入迁移框架**，也不重新设计表结构。PostgreSQL schema 是与既有
//! 部署之间的契约，改动等于数据迁移。Peewee 定义的 40 个模型逐字段映射为
//! `sqlx` 结构体，列名、类型、约束、索引全部保持原样。
//!
//! # 时间类型的重要约定
//!
//! Peewee 的 `DateTimeField` 写入 **naive UTC**，PostgreSQL 列类型是
//! `timestamp without time zone`（项目注释明确「全项目统一使用 naive datetime
//! 与数据库交互」）。因此 Rust 侧一律用 `chrono::NaiveDateTime` 而**不是**
//! `DateTime<Utc>` —— 后者按 `timestamptz` 语义解码，与既有列不匹配。
//! 需要 UTC 时在边界处 `.and_utc()`。

#![forbid(unsafe_code)]

use sqlx::postgres::{PgPool, PgPoolOptions};

/// 连接池句柄。事务可用 `db.begin()` 直接开启。
pub type Db = PgPool;

/// 建立连接池。
///
/// `max_connections` 需与 PostgreSQL 的 `max_connections` 协调。后端是单进程多任务
/// （API + 调度器 + 插件回调用），默认给到 10。
pub async fn connect(url: &str, max_connections: u32) -> Result<Db, sqlx::Error> {
    PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(std::time::Duration::from_secs(10))
        .connect(url)
        .await
}

pub mod catalog;
pub mod collections;
pub mod common;
pub mod discovery;
pub mod error;
pub mod playback;
pub mod repo;
pub mod system;
pub mod testing;
pub mod transfers;
pub mod videos;

pub use error::DbError;

pub use catalog::actor::Actor;
pub use catalog::asset::{Image, MovieActor, MoviePlotImage, MovieTag, Subtitle, Tag};
pub use catalog::movie::{Movie, MovieSeries};
pub use collections::{
    ClipCollection, ClipCollectionItem, MomentCollection, MomentCollectionItem, Playlist,
    PlaylistMovie,
};
pub use discovery::{DailyRecommendationItem, MomentRecommendation, RankingItem};
pub use playback::{Media, MediaClip, MediaLibrary, MediaThumbnail};
pub use system::user::{RefreshTokenStatus, User, UserRefreshToken};
pub use transfers::{DownloadClient, DownloadSubmissionRecord, DownloadTask, Indexer};
pub use videos::{VideoCollection, VideoCollectionItem, VideoItem};
