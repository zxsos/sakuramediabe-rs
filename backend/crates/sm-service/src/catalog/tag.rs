//! 标签 service，对应上游 `src/service/catalog/tag_service.py`（106 行）。
//!
//! 三个方法。第三个（「这个标签下的影片」）**直接委托**给
//! [`MovieService::list_movies`]（上游同样如此），所以它只暴露 8 个筛选位 ——
//! 比 `GET /movies` 少 `actor_id` / `tag_match` / `number_source` /
//! `resolution` / `query` / `blacklisted`。

use sm_db::common::Page;
use sm_db::repo::collection::SortDirection;
use sm_db::repo::tag_list::TagListSort;
use sm_db::repo::TagRepository;
use sm_db::Db;

use crate::catalog::movie::{MovieCard, MovieListParams, MovieService};
use crate::error::{details_of, ServiceError};

/// 标签列表项（上游 `TagListItemResource`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagListItem {
    pub tag_id: i32,
    pub name: String,
    /// 挂在这个标签下的影片数。**没挂影片的标签是 0**（左连接），不是缺行。
    pub movie_count: i64,
}

/// `GET /tags/{tag_id}/movies` 的筛选位。
///
/// `status` / `collection_type` 由路由层用枚举反序列化后再转成字符串
/// （与 `GET /movies` 同一套），这里不重复校验。
#[derive(Debug, Clone, Default)]
pub struct TagMovieFilters {
    pub year: Option<i32>,
    pub status: String,
    pub collection_type: String,
    pub sort: Option<String>,
    pub director_name: Option<String>,
    pub maker_name: Option<String>,
    pub heat_min: Option<i32>,
    pub heat_max: Option<i32>,
}

/// 标签 service。
#[derive(Debug, Clone)]
pub struct TagService {
    tags: TagRepository,
    /// 委托影片列表时要现造 `MovieService`，所以留存池句柄。
    db: Db,
}

impl TagService {
    pub fn new(db: &Db) -> Self {
        Self {
            tags: TagRepository::new(db.clone()),
            db: db.clone(),
        }
    }

    /// `GET /tags`。**不分页**（上游如此：标签筛选器一次要全部）。
    pub async fn list_tags(
        &self,
        query: Option<&str>,
        sort: Option<&str>,
    ) -> Result<Vec<TagListItem>, ServiceError> {
        let normalized = normalize_query(query)?;
        let sort = parse_tag_sort(sort)?;
        Ok(self
            .tags
            .list_with_counts(normalized.as_deref(), sort)
            .await?
            .into_iter()
            .map(into_item)
            .collect())
    }

    /// `GET /tags/{tag_id}`。未命中 404 `tag_not_found`。
    pub async fn get_tag(&self, tag_id: i32) -> Result<TagListItem, ServiceError> {
        self.tags
            .find_with_count(tag_id)
            .await?
            .map(into_item)
            .ok_or_else(|| {
                ServiceError::not_found("tag_not_found", "Tag not found", "tag_id", tag_id)
            })
    }

    /// `GET /tags/{tag_id}/movies`。
    ///
    /// **先验标签存在**（不存在时 404 而不是空列表 —— 空列表的含义是
    /// 「这个标签下暂时没有影片」，两者不同）。
    pub async fn list_tag_movies(
        &self,
        tag_id: i32,
        filters: &TagMovieFilters,
        page: i64,
        page_size: i64,
    ) -> Result<Page<MovieCard>, ServiceError> {
        self.get_tag(tag_id).await?;

        let params = MovieListParams {
            tag_ids: vec![tag_id],
            // 只有一个标签，用 OR 还是 AND 结果相同；取上游默认的 OR。
            tag_match_all: false,
            year: filters.year,
            status: filters.status.clone(),
            collection_type: filters.collection_type.clone(),
            number_source: "all".to_owned(),
            sort: filters.sort.clone(),
            director_name: filters.director_name.clone(),
            maker_name: filters.maker_name.clone(),
            heat_min: filters.heat_min,
            heat_max: filters.heat_max,
            ..Default::default()
        };
        MovieService::new(&self.db)
            .list_movies(&params, page, page_size)
            .await
    }
}

/// 投影行 → DTO。
fn into_item((tag_id, name, movie_count): (i32, String, i64)) -> TagListItem {
    TagListItem {
        tag_id,
        name,
        movie_count,
    }
}

/// `_normalize_query`：`None` 原样；strip 后为空是 422 `invalid_tag_filter`。
///
/// 空串**不是**「不筛」—— 客户端会以为它被忽略，实际会返回全部标签。
fn normalize_query(value: Option<&str>) -> Result<Option<String>, ServiceError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let normalized = raw.trim();
    if normalized.is_empty() {
        return Err(ServiceError::validation_with(
            "invalid_tag_filter",
            "Invalid tag filter",
            details_of("query", raw),
        ));
    }
    Ok(Some(normalized.to_owned()))
}

/// `_build_tag_sort`：**空白归一为缺省值** `movie_count:desc`（上游
/// `(sort or "").strip() or "movie_count:desc"`）。
fn parse_tag_sort(value: Option<&str>) -> Result<TagListSort, ServiceError> {
    let normalized = value.unwrap_or("").trim().to_lowercase();
    if normalized.is_empty() {
        return Ok(TagListSort::MovieCountDesc);
    }
    let (field, direction) = normalized
        .split_once(':')
        .ok_or_else(|| invalid_tag_sort(value.unwrap_or("")))?;
    let direction = match direction {
        "asc" => SortDirection::Asc,
        "desc" => SortDirection::Desc,
        _ => return Err(invalid_tag_sort(value.unwrap_or(""))),
    };
    let sort = match (field, direction) {
        ("movie_count", SortDirection::Desc) => TagListSort::MovieCountDesc,
        ("movie_count", SortDirection::Asc) => TagListSort::MovieCountAsc,
        ("name", SortDirection::Asc) => TagListSort::NameAsc,
        ("name", SortDirection::Desc) => TagListSort::NameDesc,
        _ => return Err(invalid_tag_sort(value.unwrap_or(""))),
    };
    Ok(sort)
}

fn invalid_tag_sort(raw: &str) -> ServiceError {
    ServiceError::validation_with(
        "invalid_tag_filter",
        "Invalid sort expression",
        details_of("sort", raw),
    )
}
