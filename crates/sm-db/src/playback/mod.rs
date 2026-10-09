//! `playback` 域模型。
//!
//! 对应后端 `src/model/playback/` 的 6 张表。

pub mod media;

pub use media::{
    image_search_index_status, thumbnail_state, Media, MediaClip, MediaLibrary,
    MediaPoint, MediaProgress, MediaThumbnail,
};
