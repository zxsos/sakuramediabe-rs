//! 影片目录 service，对应上游 `src/service/catalog/movie_service.py`（1,177 行）。
//!
//! # 本批只落「订阅状态流转」这一块
//!
//! | 上游方法 | 端点 | 状态 |
//! |---|---|---|
//! | `batch_set_subscription` | `POST /movies/subscriptions` | **已落** |
//! | `batch_unsubscribe_movies` | `POST /movies/unsubscriptions` | **已落** |
//! | `set_blacklisted` | `PUT` / `DELETE /movies/blacklist` | 未落 |
//! | `find_movie_by_number` + `require_movie_by_normalized_number` | 无（共用件） | **已落** |
//! | 其余 20 条端点（列表 / 详情 / 系列 / 黑名单 / 番号解析 …） | | 未落 |
//!
//! # 「部分成功」是这两条端点的核心语义
//!
//! 批量入参里混着不存在、已拉黑、已有本地媒体的番号时，上游**不整批失败**，
//! 而是把处理不了的逐条放进 `skipped`（带原因），其余照常写。客户端据此把
//! 未处理的那几行标出来。整批回滚会让用户重来一次，而他并不知道是哪几条
//! 出了问题 —— 那正是「批量」这个交互存在的理由。
//!
//! # 番号去重**不互换分隔符**
//!
//! `dedup_movie_number_keys` 只用 `strip + upper` 做 key。刻意**不**把 `_`
//! 与 `-` 折叠成同一个：两种分隔符的番号同时存在时（一本道 / 加勒比同日番号），
//! 折叠会让一次「订阅这部」扩散到**另一部影片**上。宁可 miss 进 `skipped`
//! 让用户看见，也不能错订。
//!
//! # 一处**刻意保留**的上游缺陷
//!
//! `batch_set_subscription` 的 `updated_count` 是 `len(matched_movies)`，而其中
//! 被拉黑的那几条**并没有被写**（它们同时出现在 `skipped` 里）。所以「订阅一部
//! 已拉黑影片」会返回 `updated_count=1, skipped_count=1` —— 同一部影片被记了两次。
//!
//! 这里照抄。客户端已经按这个数字渲染「已订阅 N 部」，改掉它就是一次**静默的
//! 契约变更**；要修应当作为显式行为变更连同客户端一起做。测试把当前形状钉住，
//! 附带说明，免得后来者以为是自己写错了。

use std::collections::{HashMap, HashSet};

use serde_json::Value as Json;

use sm_db::catalog::asset::Image;
use sm_db::catalog::movie::Movie;
use sm_db::common::Page;
use sm_db::repo::collection::SortDirection;
use sm_db::repo::gateway::{FieldPatch, MovieOwnershipGateway};
use sm_db::repo::movie::{MovieListFilter, MovieListSort};
use sm_db::repo::{ImageRepository, MediaRepository, MovieRepository, MovieSeriesRepository};
use sm_db::Db;

use crate::catalog::resolution;
use crate::error::{details_of, ServiceError};
use crate::movie_numbers::movie_number_lookup_values;
use crate::playback::media_summary::{attach_movie_list_media, MovieMediaAttachment};

/// 入参番号在库里找不到。
pub const SKIP_MOVIE_NOT_FOUND: &str = "movie_not_found";
/// 影片已有本地媒体 —— 退订会让「停止追踪」与「删本地资源」混成一个动作。
pub const SKIP_HAS_MEDIA: &str = "has_media";
/// 影片在黑名单里，先解除再订阅。
pub const SKIP_BLACKLISTED: &str = "blacklisted";

/// 批量订阅/退订里被跳过的一条（上游 `MovieSubscriptionSkippedItem`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSkippedItem {
    /// **原始展示番号**（用户输入的那个），不是归一后的大写 key。
    pub movie_number: String,
    /// 三个原因之一，见本模块的 `SKIP_*` 常量。
    pub reason: String,
}

/// 批量订阅/退订的结果（上游 `MovieSubscriptionBatchResponse`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionBatchResponse {
    /// **请求里给了几项**（含空串与重复项），不是去重后的个数。
    pub requested_count: i64,
    /// 见模块文档「一处刻意保留的上游缺陷」。
    pub updated_count: i64,
    pub skipped_count: i64,
    pub skipped: Vec<SubscriptionSkippedItem>,
}

/// 一张影片卡片（上游 `MovieListItemResource` 的原料）。
///
/// 与 `collections::playlist::PlaylistMovieCard` 只差一个字段：这里没有「列表
/// 关系上的最近触达时间」—— 那是播放列表特有的。两者共用同一套聚合口径
/// （分页查询定顺序 + 按 id 批量补影片/封面/系列/媒体），所以 `sm-api` 的
/// `MovieListItemResource` 能同时服务 `/playlists/{id}/movies` 与 `/movies*`。
#[derive(Debug, Clone)]
pub struct MovieCard {
    pub movie: Movie,
    /// `movie.cover_image_id` 指向的图；没有封面或图已被删时 `None`。
    pub cover_image: Option<Image>,
    /// 薄封面。与封面是两个**独立**的 id。
    pub thin_cover_image: Option<Image>,
    pub series_name: Option<String>,
    /// `media_items` / `media_count` / `can_play` 三个派生字段。
    pub media: MovieMediaAttachment,
}

/// 影片目录 service。
#[derive(Debug, Clone)]
pub struct MovieService {
    movies: MovieRepository,
    media: MediaRepository,
    images: ImageRepository,
    series: MovieSeriesRepository,
    /// 受保护字段（`is_collection` / `is_blacklisted` …）只能经它写。
    gateway: MovieOwnershipGateway,
}

impl MovieService {
    pub fn new(db: &Db) -> Self {
        Self {
            movies: MovieRepository::new(db.clone()),
            media: MediaRepository::new(db.clone()),
            images: ImageRepository::new(db.clone()),
            series: MovieSeriesRepository::new(db.clone()),
            gateway: MovieOwnershipGateway::new(db.clone()),
        }
    }

    /// `PUT` / `DELETE /movies/blacklist`。对应上游 `set_blacklisted`。
    ///
    /// # 两个 4xx 的形状都是 `details.movie_numbers` 数组
    ///
    /// - 有番号找不到 → 404 `movie_not_found`（**整批失败**，这个端点没有
    ///   `skipped`）；
    /// - 加入黑名单时已有订阅 → 409 `movie_is_subscribed`，
    ///   `details.movie_numbers` 只列**已订阅的那几个**。
    ///
    /// 两处的键都是复数数组，因为一次请求可能涉及多个番号 —— 客户端据此把它们
    /// 标回用户勾选的那几行。
    ///
    /// # 一处与上游的**已知偏差**
    ///
    /// 上游把「查 + 校验 + 写」放在同一个事务里，并对命中的行 `for_update`
    /// 加行锁。本仓库的字段主权网关没有接 `Ctx` 的变体，写只能在事务外做。
    ///
    /// 后果：校验通过后、写入前若有另一个请求把影片订阅上，写入会撞
    /// `CHECK (NOT (is_subscribed AND is_blacklisted))` —— 得到 500 而不是 409。
    /// **不会写出非法状态**（CHECK 仍然拦着），只是错误码不同。要闭合它，
    /// 得给网关补一个接 `Ctx` 的 `update_host_manual_in`。
    pub async fn set_blacklisted(
        &self,
        movie_numbers: &[String],
        blacklisted: bool,
    ) -> Result<(), ServiceError> {
        let (ordered_keys, display_by_key) = dedup_movie_number_keys(movie_numbers);
        if ordered_keys.is_empty() {
            return Ok(());
        }
        let matched = self.movies.list_by_upper_numbers(&ordered_keys).await?;

        let matched_keys: HashSet<String> = matched
            .iter()
            .map(|movie| movie.movie_number.trim().to_uppercase())
            .collect();
        let missing: Vec<String> = ordered_keys
            .iter()
            .filter(|key| !matched_keys.contains(*key))
            .map(|key| {
                display_by_key
                    .get(key)
                    .cloned()
                    .unwrap_or_else(|| key.clone())
            })
            .collect();
        if !missing.is_empty() {
            return Err(ServiceError::not_found_with(
                "movie_not_found",
                "影片不存在",
                details_of("movie_numbers", Json::from(missing)),
            ));
        }

        if blacklisted {
            let subscribed: Vec<String> = matched
                .iter()
                .filter(|movie| movie.is_subscribed)
                .map(|movie| movie.movie_number.clone())
                .collect();
            if !subscribed.is_empty() {
                return Err(ServiceError::conflict(
                    "movie_is_subscribed",
                    "已订阅影片不能加入黑名单，请先取消订阅",
                    Some(details_of("movie_numbers", Json::from(subscribed))),
                ));
            }
        }

        // `is_blacklisted` 是受保护字段：必须经网关，否则自动规则会覆盖回去。
        let ids: Vec<i32> = matched.iter().map(|movie| movie.id).collect();
        let mut patch = FieldPatch::new();
        patch.flag("is_blacklisted", blacklisted);
        self.gateway.update_host_manual(&ids, &patch).await?;
        Ok(())
    }

    /// `GET /movies`。对应上游 `MovieService.list_movies`。
    ///
    /// # 参数校验分两处，各自的错误码不同
    ///
    /// - `status` / `collection_type` / `number_source` / `tag_match` 是**枚举**，
    ///   由路由层用 serde 反序列化 → 非法值走 `QueryRejection` → 422
    ///   `validation_error`（与上游 pydantic 同）；
    /// - `heat_min > heat_max`、非法档位、非法排序、检索词超限都是 422
    ///   `invalid_movie_filter`（上游分别来自 `_filtered_movies`、
    ///   `resolution_exists_expression`、`resolve_sort_expression`、
    ///   `split_search_terms`）。
    ///
    /// # `series_id` 不在这个端点上
    ///
    /// 上游 `_filtered_movies` 接它，但 `list_movies` 不暴露 —— 它由
    /// `POST /movies/by-series` 走。所以这里固定传 `None`。
    pub async fn list_movies(
        &self,
        params: &MovieListParams,
        page: i64,
        page_size: i64,
    ) -> Result<Page<MovieCard>, ServiceError> {
        if let (Some(min), Some(max)) = (params.heat_min, params.heat_max) {
            if min > max {
                let mut details = serde_json::Map::new();
                details.insert("heat_min".to_owned(), Json::from(min));
                details.insert("heat_max".to_owned(), Json::from(max));
                return Err(ServiceError::validation_with(
                    INVALID_MOVIE_FILTER,
                    "heat_min 不能大于 heat_max",
                    details,
                ));
            }
        }

        // 非法档位在查询层之前拦掉（上游同一位置）。
        let resolution =
            resolution::resolution_interval(params.resolution.as_deref(), INVALID_MOVIE_FILTER)?;
        let search_terms = split_search_terms(params.query.as_deref())?;
        let sort = parse_movie_list_sort(params.sort.as_deref())?;

        let filter = MovieListFilter {
            actor_id: params.actor_id,
            tag_ids: params.tag_ids.clone(),
            tag_match_all: params.tag_match_all,
            year: params.year,
            subscribed: match params.status.as_str() {
                "subscribed" => Some(true),
                "unsubscribed" => Some(false),
                _ => None,
            },
            playable_only: params.status == "playable",
            single_only: params.collection_type == "single",
            series_id: None,
            director_name: params.director_name.clone(),
            maker_name: params.maker_name.clone(),
            fc2: match params.number_source.as_str() {
                "fc2" => Some(true),
                "regular" => Some(false),
                _ => None,
            },
            heat_min: params.heat_min,
            heat_max: params.heat_max,
            resolution: resolution.map(|interval| (interval.threshold, interval.upper)),
            blacklisted: params.blacklisted,
            search_terms,
        };

        let total = self.movies.count_movies(&filter).await?;
        let offset = (page - 1).max(0) * page_size;
        let ids = self
            .movies
            .list_movie_card_ids(&filter, sort, page_size, offset)
            .await?;
        Ok(Page::new(self.load_cards(&ids).await?, total))
    }

    /// `POST /movies/by-series`。对应上游 `list_movies_by_series`。
    ///
    /// 与 `GET /movies` 共用同一套筛选/排序/卡片装配，只多固定一个 `series_id`
    /// （上游 `_filtered_movies(series_id=...)` 的默认其余位都是「不限」）。
    pub async fn list_movies_by_series(
        &self,
        series_id: i32,
        sort: Option<&str>,
        page: i64,
        page_size: i64,
    ) -> Result<Page<MovieCard>, ServiceError> {
        let sort = parse_movie_list_sort(sort)?;
        let filter = MovieListFilter {
            series_id: Some(series_id),
            ..Default::default()
        };
        let total = self.movies.count_movies(&filter).await?;
        let offset = (page - 1).max(0) * page_size;
        let ids = self
            .movies
            .list_movie_card_ids(&filter, sort, page_size, offset)
            .await?;
        Ok(Page::new(self.load_cards(&ids).await?, total))
    }

    /// `GET /movies/subscribed-actors/latest`。对应上游
    /// `list_subscribed_actor_latest_movies`。
    ///
    /// 「已订阅演员的最新影片」：只列至少关联一位已订阅演员、且**不是合集番号**
    /// 的影片，按发行日期倒序（没有发行日期的垫最后）。分页与 `/latest` 一样是
    /// **裸 `int`**，不校验。
    pub async fn list_subscribed_actor_latest_movies(
        &self,
        page: i64,
        page_size: i64,
    ) -> Result<Page<MovieCard>, ServiceError> {
        let total = self.movies.count_subscribed_actor_movies().await?;
        let offset = (page - 1).max(0) * page_size;
        let ids = self
            .movies
            .list_subscribed_actor_movie_ids(page_size, offset)
            .await?;
        Ok(Page::new(self.load_cards(&ids).await?, total))
    }

    /// `POST /movie-subscriptions/search-resets`。对应上游
    /// `MovieSubscriptionSearchStateService.reset`。
    ///
    /// 返回**重开的影片数**。`movie_ids` 为 `None` 或空数组时只重开已放弃
    /// （`exhausted`）的那些 —— 见
    /// [`MovieRepository::reset_subscription_search`] 的三个口径。
    pub async fn reset_subscription_searches(
        &self,
        movie_ids: Option<&[i32]>,
    ) -> Result<i64, ServiceError> {
        Ok(self.movies.reset_subscription_search(movie_ids).await? as i64)
    }

    /// `GET /movies/{movie_number}/collection-status`。
    ///
    /// 返回的是**库内规范番号**，不是用户输入的那个 —— 上游如此，客户端据此
    /// 把结果对回真正的影片。
    pub async fn get_collection_status(
        &self,
        movie_number: &str,
    ) -> Result<MovieCollectionStatus, ServiceError> {
        let (movie, canonical) = self.require_by_normalized_number(movie_number).await?;
        Ok(MovieCollectionStatus {
            movie_number: canonical,
            is_collection: movie.is_collection,
        })
    }

    /// `PATCH /movies/collection-type`。
    ///
    /// # 为什么必须经网关
    ///
    /// `is_collection` 在 `PROTECTED_MOVIE_FIELDS` 里：人工标记要同时写
    /// `field_owners`（打上 `host:manual`）并推进 `mutation_revision`，否则
    /// 下一次自动导入会**把它覆盖回去**。普通 `update` 路径会被字段护栏拒绝。
    ///
    /// `updated_count` 取网关的影响行数（上游同样取它的返回值）。
    pub async fn mark_collection_type(
        &self,
        movie_numbers: &[String],
        collection_type: &str,
    ) -> Result<MovieCollectionMarkResponse, ServiceError> {
        let requested_count = movie_numbers.len() as i64;
        let (ordered_keys, _) = dedup_movie_number_keys(movie_numbers);
        if ordered_keys.is_empty() {
            return Ok(MovieCollectionMarkResponse {
                requested_count,
                updated_count: 0,
            });
        }

        let matched = self.movies.list_by_upper_numbers(&ordered_keys).await?;
        if matched.is_empty() {
            return Ok(MovieCollectionMarkResponse {
                requested_count,
                updated_count: 0,
            });
        }

        let ids: Vec<i32> = matched.iter().map(|movie| movie.id).collect();
        let mut patch = FieldPatch::new();
        patch.flag(
            "is_collection",
            collection_type == COLLECTION_TYPE_COLLECTION,
        );
        let updated_count = self.gateway.update_host_manual(&ids, &patch).await? as i64;

        Ok(MovieCollectionMarkResponse {
            requested_count,
            updated_count,
        })
    }

    /// `GET /movies/latest`。对应上游 `list_latest_movies`。
    ///
    /// 「最新到货」：只列**有本地媒体**的影片，按最近一次媒体入库时间倒序。
    /// 分页参数与上游一样**不校验**（裸 `int`，`max(page - 1, 0) * page_size`）。
    pub async fn list_latest_movies(
        &self,
        page: i64,
        page_size: i64,
    ) -> Result<Page<MovieCard>, ServiceError> {
        let total = self.movies.count_with_media().await?;
        let offset = (page - 1).max(0) * page_size;
        let ids = self
            .movies
            .list_latest_with_media_ids(page_size, offset)
            .await?;
        Ok(Page::new(self.load_cards(&ids).await?, total))
    }

    /// 按给定的 id 顺序装配卡片。
    ///
    /// # 只查不排
    ///
    /// 顺序完全由调用方的 id 列表决定（它在 SQL 里已经排好）。这里再排一次会
    /// 让 `added_at` / `bitrate` / `MAX(media.created_at)` 这类**排序列**直接
    /// 失效 —— 它们都不在返回的字段里。
    ///
    /// # 四条批量查询，条数与影片数无关
    ///
    /// 影片本体 / 封面与薄封面 / 系列 / 媒体摘要各一条。不是 N+1，也不随页大小
    /// 增长。
    ///
    /// # 公开给「结果集带影片卡片」的其他域复用
    ///
    /// 它等价于上游 `with_movie_card_relations` + `attach_movie_list_media`
    /// 的组合（`service_helpers.py`）。每日推荐（`discovery::daily_recommendation`）
    /// 直接复用它装配卡片，不再抄一份 —— 上游那两处也是同一套聚合。
    pub async fn load_cards(&self, ids: &[i32]) -> Result<Vec<MovieCard>, ServiceError> {
        let movies = self.movies.find_by_ids(ids).await?;

        let numbers: Vec<String> = ids
            .iter()
            .filter_map(|id| movies.get(id))
            .map(|movie| movie.movie_number.clone())
            .collect();
        let mut media = attach_movie_list_media(self.movies.pool(), &numbers).await?;

        let image_ids: Vec<i32> = movies
            .values()
            .flat_map(|movie| [movie.cover_image_id, movie.thin_cover_image_id])
            .flatten()
            .collect();
        let images = self.images.find_by_ids(&image_ids).await?;

        let series_ids: Vec<i32> = movies
            .values()
            .filter_map(|movie| movie.series_id)
            .collect();
        let series = self.series.find_by_ids(&series_ids).await?;

        Ok(ids
            .iter()
            .filter_map(|id| {
                // 内连接保证了这些影片存在；真缺了只可能是并发删除 —— 跳过
                // 而不是 panic（release 是 `panic = "abort"`，一次竞态会带走
                // 整个进程）。
                let movie = movies.get(id)?;
                Some(MovieCard {
                    cover_image: movie.cover_image_id.and_then(|v| images.get(&v).cloned()),
                    thin_cover_image: movie
                        .thin_cover_image_id
                        .and_then(|v| images.get(&v).cloned()),
                    series_name: movie
                        .series_id
                        .and_then(|v| series.get(&v).map(|row| row.name.clone())),
                    media: media.remove(&movie.movie_number).unwrap_or_default(),
                    movie: movie.clone(),
                })
            })
            .collect())
    }

    /// 人工输入定位影片：逐个候选做大小写不敏感的点查，命中即返回。
    ///
    /// 对应上游 `find_movie_by_number`。候选顺序由
    /// [`movie_number_lookup_values`] 决定（原形优先，纯数字番号不互换分隔符）。
    ///
    /// **只用于人工输入**。库内两列规范值之间的比较（如 `media.movie_number`
    /// 与 `movie.movie_number` 的 JOIN）直接裸列相等即可 —— 走这条路既慢
    /// （函数索引）又多一层歧义。
    pub async fn find_by_number(&self, value: &str) -> Result<Option<Movie>, ServiceError> {
        for candidate in movie_number_lookup_values(value) {
            if let Some(movie) = self.movies.find_by_upper_number(&candidate).await? {
                return Ok(Some(movie));
            }
        }
        Ok(None)
    }

    /// 同上，但未命中是 404。
    ///
    /// 第二个返回值是**库内规范形态**（provider 原样写入的那个），后续要拿它
    /// 去查 provider —— 两侧形态一致才能精确回查，用用户输入会漏。
    pub async fn require_by_normalized_number(
        &self,
        movie_number: &str,
    ) -> Result<(Movie, String), ServiceError> {
        let movie = self.find_by_number(movie_number).await?.ok_or_else(|| {
            ServiceError::not_found_with(
                "movie_not_found",
                "影片不存在",
                details_of("movie_number", movie_number),
            )
        })?;
        let canonical = movie.movie_number.clone();
        Ok((movie, canonical))
    }

    /// `POST /movies/subscriptions`。对应上游 `batch_set_subscription`。
    ///
    /// # `subscribed_at` 只在「原本没订」或「时间为空」时覆盖
    ///
    /// 否则重复点「订阅」会把订阅时间一路推到现在，而客户端用它排「最近订阅」
    /// —— 那个列表会莫名其妙地重排。检索状态也只在同一条件下重置：重复订阅
    /// 不该把一个正在重试中的抓取任务打回起点。
    pub async fn batch_set_subscription(
        &self,
        movie_numbers: &[String],
    ) -> Result<SubscriptionBatchResponse, ServiceError> {
        let requested_count = movie_numbers.len() as i64;
        let (ordered_keys, display_by_key) = dedup_movie_number_keys(movie_numbers);
        if ordered_keys.is_empty() {
            return Ok(SubscriptionBatchResponse {
                requested_count,
                updated_count: 0,
                skipped_count: 0,
                skipped: Vec::new(),
            });
        }

        let matched = self.movies.list_by_upper_numbers(&ordered_keys).await?;
        let mut skipped = missing_skips(&ordered_keys, &display_by_key, &matched);
        skipped.extend(
            matched
                .iter()
                .filter(|movie| movie.is_blacklisted)
                .map(|movie| SubscriptionSkippedItem {
                    movie_number: movie.movie_number.clone(),
                    reason: SKIP_BLACKLISTED.to_owned(),
                }),
        );

        for movie in &matched {
            if movie.is_blacklisted {
                continue;
            }
            // 「原本没订」或「订阅时间为空」才算真订阅：只有这时才覆盖订阅时间
            // 并重置检索状态，否则重复点订阅会把订阅时间推到现在、还会把一个
            // 正在重试中的抓取任务打回起点。
            let fresh = !movie.is_subscribed || movie.subscribed_at.is_none();
            self.movies.mark_subscribed(movie.id, fresh).await?;
        }

        Ok(SubscriptionBatchResponse {
            requested_count,
            // 照抄上游：含被拉黑、实际没写的那几条。见模块文档。
            updated_count: matched.len() as i64,
            skipped_count: skipped.len() as i64,
            skipped,
        })
    }

    /// `POST /movies/unsubscriptions`。对应上游 `batch_unsubscribe_movies`。
    ///
    /// # 有本地媒体就跳过，**不报错**
    ///
    /// 单条退订在同样情况下是 409 `movie_subscription_has_media`（上游
    /// `unsubscribe_movie`），而批量走「部分成功」：报错会让整批失败。
    /// 两个口径不同是有意的 —— 单条操作里用户就看着那一部，能看见原因；
    /// 批量里报错只会让他知道「有一部不行」。
    ///
    /// 这里的 `updated_count` 与订阅那条不同，**只数真的写了的**（上游如此）。
    pub async fn batch_unsubscribe_movies(
        &self,
        movie_numbers: &[String],
    ) -> Result<SubscriptionBatchResponse, ServiceError> {
        let requested_count = movie_numbers.len() as i64;
        let (ordered_keys, display_by_key) = dedup_movie_number_keys(movie_numbers);
        if ordered_keys.is_empty() {
            return Ok(SubscriptionBatchResponse {
                requested_count,
                updated_count: 0,
                skipped_count: 0,
                skipped: Vec::new(),
            });
        }

        let matched = self.movies.list_by_upper_numbers(&ordered_keys).await?;
        let mut skipped = missing_skips(&ordered_keys, &display_by_key, &matched);

        // 一次聚合查询判定「有媒体」，而不是逐条查 —— 页大小能到几百。
        // 参数是**番号**不是 id，见 `MediaRepository::numbers_with_media` 的文档。
        let numbers: Vec<String> = matched.iter().map(|m| m.movie_number.clone()).collect();
        let with_media = self.media.numbers_with_media(&numbers).await?;

        let mut updated_count = 0i64;
        for movie in &matched {
            if with_media.contains(&movie.movie_number) {
                skipped.push(SubscriptionSkippedItem {
                    movie_number: movie.movie_number.clone(),
                    reason: SKIP_HAS_MEDIA.to_owned(),
                });
                continue;
            }
            self.movies.clear_subscription(movie.id).await?;
            updated_count += 1;
        }

        Ok(SubscriptionBatchResponse {
            requested_count,
            updated_count,
            skipped_count: skipped.len() as i64,
            skipped,
        })
    }
}

/// `POST /movies/search/parse-number` 的结果（上游 `MovieNumberParseResponse`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieNumberParseResult {
    /// **strip 之后**的输入 —— 上游回显的是 pydantic 校验后的值。
    pub query: String,
    pub parsed: bool,
    /// 解析成功时是识别出的番号。
    pub movie_number: Option<String>,
    /// 解析失败时是 `movie_number_not_found`，成功时 `None`。
    pub reason: Option<String>,
}

/// 解析失败的原因码。
pub const MOVIE_NUMBER_NOT_FOUND: &str = "movie_number_not_found";

/// 从用户输入里识别番号。对应上游 `MovieService.parse_movie_number_query`。
///
/// **不查库**：用户输入整体就是扫描范围（不做路径截断），识别是纯文本启发式，
/// 输出是「查找键」而不是规范值 —— 见 [`crate::movie_numbers`] 的模块文档。
///
/// 识别不出时**不是错误**：返回 `parsed: false` + `reason`，客户端据此提示
/// 「这串看起来不是番号」，而不是弹一个失败。
pub fn parse_movie_number_query(query: &str) -> MovieNumberParseResult {
    let normalized = query.trim();
    let parsed = crate::movie_numbers::parse_movie_number_from_text(normalized);
    if parsed.is_empty() {
        MovieNumberParseResult {
            query: normalized.to_owned(),
            parsed: false,
            movie_number: None,
            reason: Some(MOVIE_NUMBER_NOT_FOUND.to_owned()),
        }
    } else {
        MovieNumberParseResult {
            query: normalized.to_owned(),
            parsed: true,
            movie_number: Some(parsed),
            reason: None,
        }
    }
}

/// 不合法的筛选/排序/检索参数。
const INVALID_MOVIE_FILTER: &str = "invalid_movie_filter";

/// 检索词上限（上游 `common/text_search.py`）。
const SEARCH_TERM_MAX_LENGTH: usize = 64;
const SEARCH_TERM_MAX_COUNT: usize = 6;

/// 影片列表的请求参数（上游 `list_movies` 的入参）。
#[derive(Debug, Clone, Default)]
pub struct MovieListParams {
    pub actor_id: Option<i32>,
    /// 由路由层从 `tag_ids` 的逗号串解析而来（空串/非正整数即 422）。
    pub tag_ids: Vec<i32>,
    /// `true` = AND（须同时含全部），`false` = OR（命中任一）。
    pub tag_match_all: bool,
    pub year: Option<i32>,
    /// `all` / `subscribed` / `unsubscribed` / `playable`。**路由层已校验**。
    pub status: String,
    /// `all` / `single`。**路由层已校验**。
    pub collection_type: String,
    /// `all` / `regular` / `fc2`。**路由层已校验**。
    pub number_source: String,
    pub sort: Option<String>,
    /// 精确匹配；路由层已 strip 且空串归一为 `None`。
    pub director_name: Option<String>,
    pub maker_name: Option<String>,
    pub heat_min: Option<i32>,
    pub heat_max: Option<i32>,
    pub resolution: Option<String>,
    pub blacklisted: bool,
    /// 原始检索串（自己切词，见 `split_search_terms`）。
    pub query: Option<String>,
}

/// 把搜索输入按空白拆成检索词。
///
/// - 空输入 → 空列表（不是错误）；
/// - 超过 6 个词 / 单个词超过 64 字符 → 422 `invalid_movie_filter`，
///   `details.query` 回显**原始输入**；
/// - **去重且保持顺序** —— 重复词会让相关度分数翻倍（多词求和），
///   排序就失真了。
fn split_search_terms(value: Option<&str>) -> Result<Vec<String>, ServiceError> {
    let Some(raw) = value else {
        return Ok(Vec::new());
    };
    let terms: Vec<String> = raw.split_whitespace().map(str::to_owned).collect();
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    if terms.len() > SEARCH_TERM_MAX_COUNT {
        return Err(ServiceError::validation_with(
            INVALID_MOVIE_FILTER,
            "搜索关键词过多",
            details_of("query", raw),
        ));
    }
    // Python 的 `len(term)` 是字符数，不是字节数 —— 中文检索词要按字符算。
    if terms
        .iter()
        .any(|term| term.chars().count() > SEARCH_TERM_MAX_LENGTH)
    {
        return Err(ServiceError::validation_with(
            INVALID_MOVIE_FILTER,
            "搜索关键词过长",
            details_of("query", raw),
        ));
    }
    let mut seen: HashSet<String> = HashSet::new();
    Ok(terms
        .into_iter()
        .filter(|term| seen.insert(term.clone()))
        .collect())
}

/// 非法排序表达式。
fn invalid_movie_sort(raw: &str) -> ServiceError {
    ServiceError::validation_with(
        INVALID_MOVIE_FILTER,
        "Invalid sort expression",
        details_of("sort", raw),
    )
}

/// 解析 `field:direction`（上游 `MOVIE_LIST_SORT_FIELD_MAP`）。
///
/// 七个字段与上游一一对应；`added_at` 的语义随 `status = playable` 变化，
/// 那一步在仓储层（它才知道 `playable_only`）。
fn parse_movie_list_sort(
    value: Option<&str>,
) -> Result<Option<(MovieListSort, SortDirection)>, ServiceError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() {
        return Ok(None);
    }
    let (field, direction) = normalized
        .split_once(':')
        .ok_or_else(|| invalid_movie_sort(raw))?;
    let direction = match direction {
        "asc" => SortDirection::Asc,
        "desc" => SortDirection::Desc,
        _ => return Err(invalid_movie_sort(raw)),
    };
    let sort = match field {
        "release_date" => MovieListSort::ReleaseDate,
        "added_at" => MovieListSort::AddedAt,
        "subscribed_at" => MovieListSort::SubscribedAt,
        "comment_count" => MovieListSort::CommentCount,
        "score_number" => MovieListSort::ScoreNumber,
        "want_watch_count" => MovieListSort::WantWatchCount,
        "heat" => MovieListSort::Heat,
        _ => return Err(invalid_movie_sort(raw)),
    };
    Ok(Some((sort, direction)))
}

/// 合集标记取值（上游 `MovieCollectionMarkType`）。
///
/// 只有这两个：`collection` = 合集 / 系列片，`single` = 单片。协议里没有第三个。
pub const COLLECTION_TYPE_COLLECTION: &str = "collection";
pub const COLLECTION_TYPE_SINGLE: &str = "single";

/// 影片合集状态（上游 `MovieCollectionStatusResource`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieCollectionStatus {
    /// **库内规范番号**，不是用户输入的那个。
    pub movie_number: String,
    pub is_collection: bool,
}

/// 批量标记合集的结果（上游 `MovieCollectionMarkResponse`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MovieCollectionMarkResponse {
    pub requested_count: i64,
    /// 网关的影响行数。**找不到的番号不进这里，也不进 `skipped`** ——
    /// 这个端点没有 skipped 字段，比 `requested_count` 少的差额就是没命中的。
    pub updated_count: i64,
}

/// 按大小写不敏感的精确 key（`strip + upper`）对入参去重。
///
/// 返回 `(有序 key, key → 原始展示番号)`。顺序是**入参顺序** —— `skipped`
/// 按它输出，客户端才能稳定对应到用户勾选的那几行。重复项保留**第一次**
/// 出现的那个展示形态。
fn dedup_movie_number_keys(movie_numbers: &[String]) -> (Vec<String>, HashMap<String, String>) {
    let mut ordered: Vec<String> = Vec::new();
    let mut display_by_key: HashMap<String, String> = HashMap::new();
    for raw in movie_numbers {
        let key = raw.trim().to_uppercase();
        if key.is_empty() || display_by_key.contains_key(&key) {
            continue;
        }
        display_by_key.insert(key.clone(), raw.clone());
        ordered.push(key);
    }
    (ordered, display_by_key)
}

/// 「库里没有」那部分 skip。`skipped` 的顺序由它开头 —— 与上游一致。
fn missing_skips(
    ordered_keys: &[String],
    display_by_key: &HashMap<String, String>,
    matched: &[Movie],
) -> Vec<SubscriptionSkippedItem> {
    let matched_keys: HashSet<String> = matched
        .iter()
        .map(|movie| movie.movie_number.trim().to_uppercase())
        .collect();
    ordered_keys
        .iter()
        .filter(|key| !matched_keys.contains(*key))
        .map(|key| SubscriptionSkippedItem {
            movie_number: display_by_key
                .get(key)
                .cloned()
                .unwrap_or_else(|| key.clone()),
            reason: SKIP_MOVIE_NOT_FOUND.to_owned(),
        })
        .collect()
}
