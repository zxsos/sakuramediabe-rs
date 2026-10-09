//! `playback` 域：媒体、缩略图、进度、时刻点、片段。
//!
//! 上游 `src/service/playback/` 共 19 个文件 / 6,683 行（含 `thumbnails/`
//! 子包）。本模块目前只有 [`media_summary`] 一块 —— 它是影片卡片聚合的底座，
//! 而那个聚合被 `playlists` 与 `catalog` 两个域的列表端点共同依赖。
//!
//! # 推进次序
//!
//! 沿用 `sm-service`  crate 文档里的约定：**最小且完整的先定型**。
//! `media_summary`(40) 之后是 `operation_locks`(52)、
//! `thumbnails/progress`(56)、`search_filters`(61) 这几个纯逻辑 /
//! 小查询，最后才是 `media_service`(781)、`media_clip_service`(535)、
//! `thumbnails/task_service`(508) 这些大块。
//!
//! # 依赖方向的硬约束
//!
//! - `media_library_service`(331) 的 `storage_space_usages` 被
//!   `system::status` 的磁盘空间三列等着（那里现在是 `null`）。
//! - `thumbnails/task_service`(508) 与 `media_service` 的封面生成是
//!   `svc-probe`（ffprobe）与 `svc-image` 有损 WebP 的消费者。
//! - `provider_helpers` / `thumbnails/contracts` 依赖插件 ABI，阻塞。

pub mod clip_artifact;
pub mod media_summary;
pub mod operation_locks;
pub mod search_filters;

pub use media_summary::{
    attach_movie_list_media, list_movie_media_summaries, MediaSummary, MovieMediaAttachment,
};
pub use operation_locks::{busy_error, MediaOperation};
