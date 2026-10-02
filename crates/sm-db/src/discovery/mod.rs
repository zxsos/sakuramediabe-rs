//! `discovery` 域模型（5 张表，完成 40/40）。
//!
//! 对应后端 `src/model/discovery/` 的 4 个源文件。

pub mod image_search;
pub mod rankings;

pub use image_search::{
    image_search_status, ImageSearchIndexState, ImageSearchSession, IMAGE_SEARCH_STATE_ID,
};
pub use rankings::{DailyRecommendationItem, MomentRecommendation, MomentSeedKind, RankingItem};
