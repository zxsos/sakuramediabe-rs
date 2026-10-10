//! `GET /tags`、`GET /tags/{tag_id}`、`GET /tags/{tag_id}/movies`。
//!
//! 与上游 `src/api/routers/catalog/tags.py` 的三条一一对应。
//!
//! # 四处参数校验，三个不同的错误码
//!
//! | 参数 | 非法时 | 码 |
//! |---|---|---|
//! | `query`（名字筛选） | 空白串 | 422 `invalid_tag_filter` |
//! | `sort` | 拼不出来 | 422 `invalid_tag_filter` |
//! | `status` / `collection_type` | 非枚举值 | 422 `validation_error` |
//! | `director_name` / `maker_name` | 空白串 | 422 `invalid_movie_filter` |
//!
//! 前两个是标签域自己的码，后两个**复用影片域**的 —— 上游就是这么分的，
//! 客户端按 `code` 高亮控件，合并任何一个都会让两种错误长得一样。
//!
//! # `{tag_id}/movies` 委托给影片列表
//!
//! 它只有 8 个筛选位（比 `GET /movies` 少 `actor_id` / `tag_match` /
//! `number_source` / `resolution` / `query` / `blacklisted`），因为上游也是
//! 直接调 `MovieService.list_movies(tag_ids=[tag_id], ...)`。
//!
//! **先验标签存在**：不存在的标签是 404，不是空列表 —— 空列表的含义是
//! 「这个标签下暂时没有影片」。

use axum::extract::{Path, State};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use sm_core::pagination::Paginated;
use sm_service::catalog::tag::{TagListItem, TagMovieFilters, TagService};

use crate::auth::CurrentUser;
use crate::dto::MovieListItemResource;
use crate::error::ErrorResponse;
use crate::extract::Query as EnvelopeQuery;
use crate::routes::method_not_allowed;
use crate::routes::movies::{
    default_page, default_page_size, parse_exact_text, MovieCollectionTypeQuery, MovieStatusQuery,
};
use crate::signing::{now_seconds, signing_secret};
use crate::state::AppState;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/tags", get(list_tags).fallback(method_not_allowed))
        .route("/tags/{tag_id}", get(get_tag).fallback(method_not_allowed))
        .route(
            "/tags/{tag_id}/movies",
            get(list_tag_movies).fallback(method_not_allowed),
        )
}

#[derive(Debug, Deserialize)]
struct TagPath {
    tag_id: i32,
}

/// 标签列表项（上游 `TagListItemResource`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagListItemResource {
    /// 字段名是 `tag_id`（上游 `validation_alias="id"` + 关闭按别名序列化）。
    pub tag_id: i32,
    pub name: String,
    pub movie_count: i64,
}

impl From<TagListItem> for TagListItemResource {
    fn from(value: TagListItem) -> Self {
        Self {
            tag_id: value.tag_id,
            name: value.name,
            movie_count: value.movie_count,
        }
    }
}

/// `GET /tags` 的查询参数。
#[derive(Debug, Deserialize)]
struct ListTagsQuery {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    sort: Option<String>,
}

async fn list_tags(
    _user: CurrentUser,
    State(state): State<AppState>,
    EnvelopeQuery(query): EnvelopeQuery<ListTagsQuery>,
) -> Result<Json<Vec<TagListItemResource>>, ErrorResponse> {
    let items = TagService::new(state.db())
        .list_tags(query.query.as_deref(), query.sort.as_deref())
        .await?;
    Ok(Json(items.into_iter().map(Into::into).collect()))
}

async fn get_tag(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<TagPath>,
) -> Result<Json<TagListItemResource>, ErrorResponse> {
    let item = TagService::new(state.db()).get_tag(path.tag_id).await?;
    Ok(Json(item.into()))
}

/// `GET /tags/{tag_id}/movies` 的查询参数（上游同名的 10 个参数）。
///
/// 分页与 `GET /movies` 一样**不校验**（上游是裸 `int`）。
#[derive(Debug, Deserialize)]
struct TagMoviesQuery {
    #[serde(default)]
    year: Option<i32>,
    #[serde(default)]
    status: MovieStatusQuery,
    #[serde(default)]
    collection_type: MovieCollectionTypeQuery,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    director_name: Option<String>,
    #[serde(default)]
    maker_name: Option<String>,
    #[serde(default)]
    heat_min: Option<i32>,
    #[serde(default)]
    heat_max: Option<i32>,
    #[serde(default = "default_page")]
    page: i64,
    #[serde(default = "default_page_size")]
    page_size: i64,
}

async fn list_tag_movies(
    _user: CurrentUser,
    State(state): State<AppState>,
    Path(path): Path<TagPath>,
    EnvelopeQuery(query): EnvelopeQuery<TagMoviesQuery>,
) -> Result<Json<Paginated<MovieListItemResource>>, ErrorResponse> {
    let filters = TagMovieFilters {
        year: query.year,
        status: query.status.as_str().to_owned(),
        collection_type: query.collection_type.as_str().to_owned(),
        sort: query.sort.clone(),
        director_name: parse_exact_text(query.director_name.as_deref(), "director_name")?,
        maker_name: parse_exact_text(query.maker_name.as_deref(), "maker_name")?,
        heat_min: query.heat_min,
        heat_max: query.heat_max,
    };

    let page = TagService::new(state.db())
        .list_tag_movies(path.tag_id, &filters, query.page, query.page_size)
        .await?;

    let secret = signing_secret(&state)?;
    let now = now_seconds();
    let items = page
        .items
        .iter()
        .map(|card| MovieListItemResource::from_movie_card(card, &secret, now))
        .collect();

    Ok(Json(Paginated::new(
        items,
        query.page,
        query.page_size,
        page.total,
    )))
}
