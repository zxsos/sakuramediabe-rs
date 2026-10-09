//! 媒体缩略图的**兼容门面**（上游 `playback/media_thumbnail_service.py`，**16 行**）。
//!
//! # 上游这个文件只有 16 行，因为它**什么都不做**
//!
//! 上游 docstring 原文：「媒体缩略图兼容门面；实现已拆至 `playback.thumbnails`。」
//!
//! 类里的六个「方法」全是**类属性赋值**，不是 `def`：
//!
//! ```python
//! count_pending_media = MediaThumbnailTaskService.count_pending_media
//! ```
//!
//! # 本仓**刻意不照抄**这个门面
//!
//! Rust 没有「类属性赋值」这种写法，要复刻只能写一串 `pub use` 或转发函数，
//! 结果是**同一批方法有两个名字**。而本仓的调用方（worker handler、路由）
//! 直接引 `thumbnails::task_service` 即可 —— 多一层门面只会让人搞不清
//! 「该调哪个」。
//!
//! 所以本文件只保留**常量重导出**（`TASK_KEY`），并在文档里记下这个差异。
//! 若将来发现确有外部调用方需要旧路径，加 `pub use` 重导出即可，
//! **不要**写转发函数 —— 那才是真的重复。

// 上游的 `MediaThumbnailService.TASK_KEY` 就是 `MediaThumbnailTaskService.TASK_KEY`。
pub use super::thumbnails::task_service::TASK_KEY;
