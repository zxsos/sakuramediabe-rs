//! `catalog` 域模型。
//!
//! 对应后端 `src/model/catalog/` 的 9 个模型。

pub mod actor;
pub mod asset;
pub mod movie;

pub use actor::{Actor, PROTECTED_ACTOR_FIELDS};
pub use asset::{Image, MovieActor, MoviePlotImage, MovieTag, Subtitle, Tag};
pub use movie::{field_owner, Movie, MovieSeries, PROTECTED_MOVIE_FIELDS};
