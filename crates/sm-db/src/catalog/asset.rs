//! `image` / `tag` / 关联表 / `subtitle` 映射。
//!
//! 对应 `src/model/catalog/images.py`、`tags.py` 与 `movies.py` 里的关联模型。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// `image` 表。
///
/// 影片资产按目录前缀查询 `origin`（`LIKE '目录/%'`），因此有
/// `image_origin_pattern` 索引（`text_pattern_ops`，按字节序比较），
/// 保证前缀匹配走索引扫描而非全表扫。
#[derive(Debug, Clone, FromRow)]
pub struct Image {
    pub id: i32,
    /// 原图路径（相对 media 根目录）。唯一。
    pub origin: String,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

/// `tag` 表。
#[derive(Debug, Clone, FromRow)]
pub struct Tag {
    pub id: i32,
    pub name: String,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

/// `movie_actor` 关联表。
///
/// 对应 Peewee 的 `Meta.indexes = ((("movie", "actor"), True),)`，即 `(movie_id, actor_id)`
/// 唯一索引。
#[derive(Debug, Clone, FromRow)]
pub struct MovieActor {
    pub id: i32,
    pub movie_id: i64,
    pub actor_id: i64,
}

/// `movie_tag` 关联表。`(movie_id, tag_id)` 唯一。
#[derive(Debug, Clone, FromRow)]
pub struct MovieTag {
    pub id: i32,
    pub movie_id: i64,
    pub tag_id: i64,
}

/// `movie_plot_image` 关联表。
///
/// 除 `(movie_id, image_id)` 唯一外，还有 `(image_search_index_status, id)` 索引，
/// 供图搜索引任务按状态批量取件。
#[derive(Debug, Clone, FromRow)]
pub struct MoviePlotImage {
    pub id: i32,
    pub movie_id: i64,
    pub image_id: i64,
    /// 图搜索引状态。0 待处理 / 1 失败 / 2 成功。
    pub image_search_index_status: i32,
}

/// 图搜索引状态取值。对应 Peewee 的类常量。
pub mod image_search_index_status {
    /// 待处理。
    pub const PENDING: i32 = 0;
    /// 失败。
    pub const FAILED: i32 = 1;
    /// 成功。
    pub const SUCCESS: i32 = 2;

    /// 是否为终态（无需再处理）。
    pub const fn is_terminal(status: i32) -> bool {
        status == SUCCESS
    }
}

/// `subtitle` 表。
///
/// 对应 Peewee 的 `Meta.indexes = ((("movie", "file_path"), True),)`。
#[derive(Debug, Clone, FromRow)]
pub struct Subtitle {
    pub id: i32,
    pub movie_id: i64,
    /// 字幕文件路径。
    pub file_path: String,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_status_constants_match_backend() {
        assert_eq!(image_search_index_status::PENDING, 0);
        assert_eq!(image_search_index_status::FAILED, 1);
        assert_eq!(image_search_index_status::SUCCESS, 2);
    }

    #[test]
    fn only_success_is_terminal() {
        assert!(image_search_index_status::is_terminal(
            image_search_index_status::SUCCESS
        ));
        assert!(!image_search_index_status::is_terminal(
            image_search_index_status::PENDING
        ));
        assert!(
            !image_search_index_status::is_terminal(image_search_index_status::FAILED),
            "失败状态需要重试，不算终态"
        );
    }
}
