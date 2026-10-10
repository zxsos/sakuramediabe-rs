//! 播放列表 service，对应上游 `src/service/collections/playlist_service.py`。
//!
//! # 这里只放**业务规则**，不放查询编排
//!
//! 上游这个文件 456 行，其中约一半是 Peewee 表达式树（`Case(...)`、
//! 子查询、`fn.MAX(bitrate)`）用于列表页的排序与分辨率聚合。那部分依赖
//! `src/service/catalog/movie_resolution_service`，而后者又依赖影片媒体
//! 的聚合查询 —— **不在本批范围内**。
//!
//! 本文件落地的是可独立验证的五条规则，它们正是 API 行为的分叉点：
//!
//! | 规则 | 上游 | 后果 |
//! |---|---|---|
//! 名称非空 | `_normalize_name` | 422 `validation_error` |
//! 名称唯一（更新时排除自己） | `_ensure_name_available` | 409 `playlist_name_conflict` |
//! 系统保留名不可占用 | `_ensure_name_not_reserved` | 409 `playlist_reserved_name` |
//! 系统列表不可改 | `_require_custom_playlist` | 409 `playlist_managed_by_system` |
//! 空更新被拒 | `update_playlist` | 422 `validation_error` |
//!
//! # 查询编排：已全部落地
//!
//! 上述五条规则之外，三个查询也已落地：
//!
//! - [`PlaylistService::list`] —— 系统列表排序 + 批量成员计数。
//! - [`PlaylistService::resolution_options`] —— 分辨率档位聚合。
//! - [`PlaylistService::list_playlist_movies`] —— 列表内影片分页（影片卡片）。
//!
//! ## 影片卡片怎么拼
//!
//! 上游 `list_playlist_movies` 是一条大 JOIN（`with_movie_card_relations` 追加
//! 封面/薄封面/系列三个关联）+ `attach_movie_list_media` 挂媒体。Rust 侧拆成
//! **固定的几条查询**，而不是一条 20+ 列的 JOIN：
//!
//! | 步 | 查询 | 产出 |
//! |---|---|---|
//! | 1 | `PlaylistMovieRepository::count_movie_cards` | `total` |
//! | 2 | `PlaylistMovieRepository::list_movie_cards` | 当页 `(link_id, updated_at, movie_id)` |
//! | 3 | `MovieRepository::find_by_ids` | 影片本体 |
//! | 4 | `ImageRepository::find_by_ids` | 封面 + 薄封面（一条 `ANY` 查询） |
//! | 5 | `MovieSeriesRepository::find_by_ids` | 系列名 |
//! | 6 | `attach_movie_list_media` | `media_items` / `media_count` / `can_play` |
//!
//! 条数与页大小无关，也不随影片数增长 —— 3/4/5/6 全是批量。拆开的代价是
//! 多几次往返，换来的是**不必用 20+ 个位置元组**在四张表之间对齐字段
//! （那种错位是静默的，见 `sm_db::repo::collection::list_movie_cards` 的文档）。
//!
//! **顺序只由第 2 步的 SQL 决定**，后面几步都是按 id 补齐 —— 内存里再排一次
//! 会让 `added_at` / `bitrate` 这两个子查询排序列直接失效。
//!
//! # 两处容易搞反的地方
//!
//! **`update_playlist` 在名字未变时跳过唯一性检查。** 上游写的是
//! `if name != playlist.name: _ensure_name_available(...)`。少了这个判断，
//! 「只改描述不改名字」会因为撞到**自己**而失败。
//!
//! **`remove_movie_from_playlist` 在影片不存在时静默返回**，且只有真的删掉
//! 了行才推进列表的 `updated_at`。前者不是错误：列表里本来就没有它，
//! 结果状态已经达成。
//!
//! # 「重新加入」会更新时间，但**不改顺序**
//!
//! `add_movie_to_playlist` 在成员已存在时只更新 `PlaylistMovie.updated_at`
//! —— 而 `playlist_movie` **没有 `position` 列**，顺序靠 `id`。所以重新加入
//! 一部已经在列表里的影片，**不会**把它挪到末尾。这一点与
//! `moment_collection` / `clip_collection` 不同（那两个有 `position`）。

use std::collections::HashMap;

use chrono::NaiveDateTime;

use sm_db::catalog::asset::Image;
use sm_db::catalog::movie::Movie;
use sm_db::collections::{
    Playlist, PLAYLIST_KIND_RECENTLY_PLAYED, RECENTLY_PLAYED_PLAYLIST_DESCRIPTION,
    RECENTLY_PLAYED_PLAYLIST_NAME,
};
use sm_db::common::Page;
use sm_db::error::DbError;
use sm_db::repo::collection::{PlaylistMovieCardSort, SortDirection};
use sm_db::repo::{
    ImageRepository, MovieRepository, MovieSeriesRepository, NewCollection,
    PlaylistMovieRepository, PlaylistRepository,
};
use sm_db::Db;

use crate::catalog::resolution::{self, RESOLUTION_LEVELS};
use crate::error::{details_of, ServiceError};
use crate::playback::media_summary::{attach_movie_list_media, MovieMediaAttachment};

/// 更新播放列表的请求。**两个字段都可缺省** —— 缺省表示不改动。
#[derive(Debug, Clone, Default)]
pub struct PlaylistUpdate {
    pub name: Option<String>,
    pub description: Option<String>,
}

impl PlaylistUpdate {
    /// 是否有任何字段被给出。上游对空更新返回 422。
    pub fn is_empty(&self) -> bool {
        self.name.is_none() && self.description.is_none()
    }
}

/// 一个播放列表**连同它的成员数**。
///
/// 计数与本体合成一个结构体，而不是让调用方拿 `Vec<Playlist>` 再自己配
/// `HashMap`：那会让「某个列表忘了查计数」变成一个静默的 0，而客户端读
/// `movie_count` 决定要不要显示条目数 —— 0 与「真的没有影片」不可区分。
#[derive(Debug, Clone)]
pub struct PlaylistWithCount {
    pub playlist: Playlist,
    pub movie_count: i32,
}

/// 播放列表内覆盖到的分辨率档位，供前端渲染筛选下拉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolutionOption {
    pub resolution: String,
    pub count: i32,
}

/// 列表内的一张影片卡片。
///
/// 上游的 `PlaylistMovieListItemResource` 是 `MovieListItemResource`（23 个字段）
/// 加上 `playlist_item_updated_at`。这里**不直接产出 DTO**：封面 `origin` 要签名，
/// 而签名密钥属于 HTTP 层（`sm-api` 每次请求重读配置 —— 见 `sm_api::signing`）。
/// 所以这一层交出原料，由调用方组装。
///
/// # 为什么把图与系列名一起带出来
///
/// 它们本来挂在 `Movie` 上（`cover_image_id` / `series_id`），但 DTO 要的是
/// **签名后的 URL 与系列名**，而不是 id。放在这里而不是让调用方各自再查一次，
/// 是为了让「一次请求几条查询」这件事在 service 里就定死。
#[derive(Debug, Clone)]
pub struct PlaylistMovieCard {
    pub movie: Movie,
    /// `movie.cover_image_id` 指向的图。没有封面、或图已被删时为 `None`。
    pub cover_image: Option<Image>,
    /// 薄封面。与封面是**两个独立**的 id，可能一个有一个没有。
    pub thin_cover_image: Option<Image>,
    /// `movie.series_id` 指向的系列名。不在系列里 / 系列被删时为 `None`。
    pub series_name: Option<String>,
    /// 媒体摘要与三个派生字段：`media_items` / `media_count` / `can_play`。
    pub media: MovieMediaAttachment,
    /// 列表关系上的最近触达时间。
    ///
    /// **可空** —— DDL 里 `playlist_movie.updated_at` 是 `timestamp NULL`，
    /// 而上游 DTO 把它声明成非空 `datetime`。本仓库对同类情况的约定是
    /// 序列化成空串（见 `sm_api::dto::PlaylistResource`）。
    pub playlist_item_updated_at: Option<NaiveDateTime>,
}

/// 播放列表 service。
///
/// 方法都是 `&self` 上的异步函数，持有仓储而不是用类方法 —— Rust 没有
/// 上游那种「全类方法 + ORM 全局连接」的写法，依赖必须显式持有。
pub struct PlaylistService {
    playlists: PlaylistRepository,
    members: PlaylistMovieRepository,
    movies: MovieRepository,
    /// 影片卡片要的封面/薄封面。
    images: ImageRepository,
    /// 影片卡片要的系列名。
    series: MovieSeriesRepository,
}

impl PlaylistService {
    /// `Db` 是 `PgPool` 的类型别名，所以这里是**连接池本身**而不是
    /// 「持有池的句柄」—— 没有 `.pool()` 可调用。
    ///
    /// 每个 service 克隆三个仓储各自持有一份池句柄：池本身是 `Arc` 内部
    /// 共享的，克隆成本远小于让 service 自己去借。
    pub fn new(db: &Db) -> Self {
        Self {
            playlists: PlaylistRepository::new(db.clone()),
            members: PlaylistMovieRepository::new(db.clone()),
            movies: MovieRepository::new(db.clone()),
            images: ImageRepository::new(db.clone()),
            series: MovieSeriesRepository::new(db.clone()),
        }
    }

    // ---------------------------------------------------------------- 规则

    /// 名称归一：trim，空则 422。
    fn normalize_name(name: &str) -> Result<String, ServiceError> {
        let normalized = name.trim();
        if normalized.is_empty() {
            return Err(ServiceError::validation(
                "validation_error",
                "Playlist name cannot be empty",
            ));
        }
        Ok(normalized.to_owned())
    }

    /// 描述归一：`None` → 空串。
    ///
    /// 与名称不同：**空描述是合法的**。上游 `_normalize_description(None)`
    /// 返回 `""`，所以「不传描述」和「传空描述」是同一个结果，而不是
    /// 「未提供」这个第三态。
    fn normalize_description(description: Option<&str>) -> String {
        description.unwrap_or_default().trim().to_owned()
    }

    /// 名称唯一性。`exclude_id` 用于更新时排除自己。
    async fn ensure_name_available(
        &self,
        name: &str,
        exclude_id: Option<i32>,
    ) -> Result<(), ServiceError> {
        match self.playlists.find_by_name(name).await? {
            Some(existing) if Some(existing.id) == exclude_id => Ok(()),
            Some(_) => Err(ServiceError::conflict(
                "playlist_name_conflict",
                "Playlist name already exists",
                Some(details_of("name", name)),
            )),
            None => Ok(()),
        }
    }

    /// 系统保留名不可被普通列表占用。
    fn ensure_name_not_reserved(name: &str) -> Result<(), ServiceError> {
        if name == RECENTLY_PLAYED_PLAYLIST_NAME {
            return Err(ServiceError::conflict(
                "playlist_reserved_name",
                "Playlist name is reserved",
                Some(details_of("name", name)),
            ));
        }
        Ok(())
    }

    /// 取一个列表，不存在则 404。
    async fn require_playlist(&self, playlist_id: i32) -> Result<Playlist, ServiceError> {
        self.playlists
            .find_by_id(playlist_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found(
                    "playlist_not_found",
                    "Playlist not found",
                    "playlist_id",
                    playlist_id,
                )
            })
    }

    /// 取一个**自定义**列表 —— 系统托管的列表不可被外部改动。
    async fn require_custom_playlist(&self, playlist_id: i32) -> Result<Playlist, ServiceError> {
        let playlist = self.require_playlist(playlist_id).await?;
        if playlist.is_system() {
            return Err(ServiceError::conflict(
                "playlist_managed_by_system",
                "Playlist is managed by system",
                Some(details_of("playlist_id", playlist.id)),
            ));
        }
        Ok(playlist)
    }

    // ---------------------------------------------------------------- 列表

    /// 列出播放列表，系统列表在前，每个列表带上成员数。
    ///
    /// 对应上游 `list_playlists(include_system=True)`。
    ///
    /// **两次查询而不是 N+1**：一次取列表、一次批量计数
    /// （[`PlaylistMovieRepository::count_by_playlists`]）。
    ///
    /// # `include_system = false` 用 `kind <>` 而不是 `NOT IN (...)`
    ///
    /// 上游是 `Playlist.kind.not_in(cls.SYSTEM_KINDS)`。两者在 `kind` 非空时
    /// 等价，而该列是 NOT NULL（DDL 默认 `custom`，Rust 侧是 `String`），
    /// 所以改成 `<>` 只是为了少一次数组绑定。**若哪天 `kind` 变成可空，
    /// 必须换回 `NOT IN`** —— `NULL NOT IN (...)` 是 NULL 而非 TRUE，
    /// 那种行会被静默漏掉，而 `kind <> '...'` 同样把它漏掉（都不报错）。
    pub async fn list(&self, include_system: bool) -> Result<Vec<PlaylistWithCount>, ServiceError> {
        let playlists = self.playlists.list_ordered(include_system).await?;
        let ids: Vec<i32> = playlists.iter().map(|p| p.id).collect();
        let counts = self.members.count_by_playlists(&ids).await?;

        Ok(playlists
            .into_iter()
            .map(|playlist| {
                // 缺项按 0：列表存在但没有成员行，与「列表不存在」是不同的事。
                // 截断不可能发生 —— 成员数上界是 `playlist_movie` 的行数，
                // 而它的主键是 i32。
                let movie_count = counts.get(&playlist.id).copied().unwrap_or(0) as i32;
                PlaylistWithCount {
                    playlist,
                    movie_count,
                }
            })
            .collect())
    }

    /// 某个播放列表的成员数。
    ///
    /// 对应上游 `get_playlist` / `update_playlist` 里的
    /// `_playlist_counts([playlist.id]).get(playlist.id, 0)` —— 两次查询
    /// （取列表 + 数成员），与上游一致。
    ///
    /// 供「已经拿到 `Playlist`、只缺计数」的调用方使用。整页列表走
    /// [`PlaylistService::list`]，那里是一次批量查询而不是逐个。
    pub async fn member_count(&self, playlist_id: i32) -> Result<i32, ServiceError> {
        let counts = self.members.count_by_playlists(&[playlist_id]).await?;
        Ok(counts.get(&playlist_id).copied().unwrap_or(0) as i32)
    }

    /// 播放列表内影片覆盖到的分辨率档位。
    ///
    /// 对应上游 `list_playlist_resolutions`。
    ///
    /// # 顺序 = 档位从高到低，且**过滤掉计数为 0 的档位**
    ///
    /// 上游最后一步是 `if count > 0`。前端直接把返回数组当筛选项渲染，
    /// 留下 `count: 0` 的档位会让用户点进去得到空列表。
    ///
    /// # 分桶在 Rust 侧做，不在 SQL 里
    ///
    /// 上游注释写明了理由：`MAX(level)` 之后要按「序号落在哪个档位」归类，
    /// 而这个映射不是线性的（`level=5 → 2K`）。写进 SQL 就得让档位标签
    /// 进 `GROUP BY`，于是「按影片聚合」变成「按影片+标签聚合」，一部影片
    /// 会被计入多个桶 —— 计数直接错。
    pub async fn resolution_options(
        &self,
        playlist_id: i32,
    ) -> Result<Vec<ResolutionOption>, ServiceError> {
        // 列表不存在时 404，且**先于**聚合查询 —— 与上游
        // `cls._require_playlist(playlist_id)` 的位置一致。
        self.require_playlist(playlist_id).await?;

        let levels = self
            .movies
            .max_resolution_levels_by_playlist(playlist_id)
            .await?;

        let mut counts: std::collections::HashMap<&'static str, i32> =
            std::collections::HashMap::new();
        for row in levels {
            let level: resolution::MovieResolutionLevel = row.into();
            // `bucket_for_level` 对 level <= 0 返回 None —— 不可解析的媒体
            // 不计入任何档位，而不是被塞进最低档。
            if let Some(label) = resolution::bucket_for_level(level.max_level) {
                *counts.entry(label).or_insert(0) += 1;
            }
        }

        Ok(RESOLUTION_LEVELS
            .iter()
            .filter_map(|(label, _)| {
                let count = counts.get(label).copied().unwrap_or(0);
                (count > 0).then(|| ResolutionOption {
                    resolution: (*label).to_owned(),
                    count,
                })
            })
            .collect())
    }

    /// 列出播放列表内的影片（分页）。对应上游 `list_playlist_movies`。
    ///
    /// # 分页参数**刻意不校验**
    ///
    /// 上游这个端点的 `page` / `page_size` 是裸 `int`（没有 `ge` / `le`），
    /// 起始位置算的是 `max(page - 1, 0) * page_size`。别的列表端点走
    /// `validate_page`（`page_size` 上限 100），这里不走 —— 给 `page_size`
    /// 加个上限是**行为变更**，而客户端已经在按「传多少给多少」用它。
    ///
    /// # 404 先于一切查询
    ///
    /// 列表不存在 → 404，且**在任何聚合查询之前**（与上游
    /// `cls._require_playlist(playlist_id)` 的位置一致）。非法分辨率档位同理：
    /// 上游注释写明是「避免非法值到查询层才炸出未预期错误」。
    ///
    /// # 顺序由 SQL 决定，补齐不得重排
    ///
    /// 第 2 步（[`PlaylistMovieRepository::list_movie_cards`]）已经排好序，
    /// 后面几步只是按 id 补齐。在内存里再排一次会让 `added_at` / `bitrate`
    /// 这两个**相关子查询**排序列直接失效（它们不在返回的字段里）。
    pub async fn list_playlist_movies(
        &self,
        playlist_id: i32,
        page: i64,
        page_size: i64,
        sort: Option<&str>,
        resolution: Option<&str>,
    ) -> Result<Page<PlaylistMovieCard>, ServiceError> {
        self.require_playlist(playlist_id).await?;

        // 档位 → `[threshold, upper)`。`None` = 不筛。
        let interval = resolution::resolution_interval(resolution, "invalid_playlist_filter")?;
        let filter = interval.map(|interval| (interval.threshold, interval.upper));
        let sort_key = parse_playlist_sort(sort)?;

        let total = self.members.count_movie_cards(playlist_id, filter).await?;
        // `max(page - 1, 0)`：`page=0` 与负数都是第一页，与上游同一表达式。
        let offset = (page - 1).max(0) * page_size;
        let links = self
            .members
            .list_movie_cards(playlist_id, filter, sort_key, page_size, offset)
            .await?;

        let movie_ids: Vec<i32> = links.iter().map(|(_, _, movie_id)| *movie_id).collect();
        let mut movies = self.movies.find_by_ids(&movie_ids).await?;

        // 番号是「影片 ↔ 媒体」的连接键：`media.movie_number` 指向它，不是 id。
        let numbers: Vec<String> = links
            .iter()
            .filter_map(|(_, _, movie_id)| movies.get(movie_id).map(|m| m.movie_number.clone()))
            .collect();
        let mut media = attach_movie_list_media(self.movies.pool(), &numbers).await?;

        let images = self.load_card_images(&movies).await?;
        let series = self.load_card_series(&movies).await?;

        let mut items = Vec::with_capacity(links.len());
        for (_, playlist_item_updated_at, movie_id) in links {
            // 内连接保证影片存在（FK 也是）。真缺了只可能是并发删除 ——
            // 跳过这一行而不是 panic：release 是 `panic = "abort"`，
            // 一次竞态会带走整个进程，代价远大于少一行。
            let Some(movie) = movies.remove(&movie_id) else {
                continue;
            };
            items.push(PlaylistMovieCard {
                cover_image: movie.cover_image_id.and_then(|id| images.get(&id).cloned()),
                thin_cover_image: movie
                    .thin_cover_image_id
                    .and_then(|id| images.get(&id).cloned()),
                series_name: movie
                    .series_id
                    .and_then(|id| series.get(&id).map(|row| row.name.clone())),
                media: media.remove(&movie.movie_number).unwrap_or_default(),
                playlist_item_updated_at,
                movie,
            });
        }
        Ok(Page::new(items, total))
    }

    /// 一次取回这一页影片用到的全部封面与薄封面。
    ///
    /// 两种图合成**一条** `ANY` 查询：分别查会多一次往返，而它们总是同批用到。
    async fn load_card_images(
        &self,
        movies: &HashMap<i32, Movie>,
    ) -> Result<HashMap<i32, Image>, ServiceError> {
        let ids: Vec<i32> = movies
            .values()
            .flat_map(|movie| [movie.cover_image_id, movie.thin_cover_image_id])
            .flatten()
            .collect();
        Ok(self.images.find_by_ids(&ids).await?)
    }

    /// 一次取回这一页影片用到的全部系列。
    async fn load_card_series(
        &self,
        movies: &HashMap<i32, Movie>,
    ) -> Result<HashMap<i32, sm_db::catalog::movie::MovieSeries>, ServiceError> {
        let ids: Vec<i32> = movies
            .values()
            .filter_map(|movie| movie.series_id)
            .collect();
        Ok(self.series.find_by_ids(&ids).await?)
    }

    // ---------------------------------------------------------------- 写入

    /// 新建列表。
    ///
    /// 顺序是**先保留名、后唯一性** —— 与上游一致：保留名冲突时返回
    /// `playlist_reserved_name` 而不是 `playlist_name_conflict`，客户端据此
    /// 区分「这个名字你不能占」与「这个名字已被别人占了」。
    pub async fn create(
        &self,
        name: &str,
        description: Option<&str>,
    ) -> Result<Playlist, ServiceError> {
        let name = Self::normalize_name(name)?;
        Self::ensure_name_not_reserved(&name)?;
        self.ensure_name_available(&name, None).await?;

        Ok(self
            .playlists
            .insert(&NewCollection::host_owned(
                &name,
                &Self::normalize_description(description),
            ))
            .await?)
    }

    /// 取一个列表。**含系统列表** —— 读取不做 `_require_custom_playlist`，
    /// 上游 `get_playlist` 同样能取到「最近播放」。
    pub async fn get(&self, playlist_id: i32) -> Result<Playlist, ServiceError> {
        self.require_playlist(playlist_id).await
    }

    /// 更新列表。**只能改自定义列表。**
    ///
    /// 名字未变时**跳过**唯一性检查 —— 否则「只改描述」会因为撞到自己
    /// 而失败。
    pub async fn update(
        &self,
        playlist_id: i32,
        payload: PlaylistUpdate,
    ) -> Result<Playlist, ServiceError> {
        let playlist = self.require_custom_playlist(playlist_id).await?;
        if payload.is_empty() {
            return Err(ServiceError::validation(
                "validation_error",
                "At least one field must be provided",
            ));
        }

        let mut updated = playlist.clone();
        if let Some(name) = payload.name {
            let name = Self::normalize_name(&name)?;
            Self::ensure_name_not_reserved(&name)?;
            if name != updated.name {
                self.ensure_name_available(&name, Some(playlist.id)).await?;
            }
            updated.name = name;
        }
        if let Some(description) = payload.description {
            updated.description = Self::normalize_description(Some(&description));
        }

        self.apply_update(&playlist, &updated).await
    }

    /// 写回改动。**只写真正变动的字段。**
    ///
    /// 上游 `update_playlist` 直接 `playlist.save()`（Peewee 全字段写），
    /// 而这里分开处理：改名走仓库的 `rename`（带同样的空名校验），改描述
    /// 走一次 UPDATE。分开写而不是无条件全字段写，是为了让「没给的字段
    /// 保持原值」成为**显式**保证 —— 全字段写在并发下会把别人的改动覆盖掉。
    async fn apply_update(
        &self,
        before: &Playlist,
        after: &Playlist,
    ) -> Result<Playlist, ServiceError> {
        let mut current = before.clone();
        if after.name != before.name {
            self.playlists.rename(before.id, &after.name).await?;
            current.name = after.name.clone();
        }
        if after.description != before.description {
            self.playlists
                .set_description(before.id, &after.description)
                .await?;
            current.description = after.description.clone();
        }
        Ok(current)
    }

    /// 删除列表。**只能删自定义列表**，成员随外键 CASCADE 一并消失。
    pub async fn delete(&self, playlist_id: i32) -> Result<(), ServiceError> {
        let playlist = self.require_custom_playlist(playlist_id).await?;
        self.playlists.delete(playlist.id).await?;
        Ok(())
    }

    // ---------------------------------------------------------------- 成员

    /// 加入一部影片。**只能改自定义列表。**
    ///
    /// 已在列表里时**只更新 `updated_at`** —— 注意这**不会**改变顺序：
    /// `playlist_movie` 没有 `position`，顺序靠 `id`。
    ///
    /// 无论新增还是重新加入，都推进列表自身的 `updated_at`，便于 UI 按
    /// 最近活跃排序。
    pub async fn add_movie(
        &self,
        playlist_id: i32,
        movie_number: &str,
    ) -> Result<(), ServiceError> {
        let playlist = self.require_custom_playlist(playlist_id).await?;
        let movie = self.require_movie(movie_number).await?;
        self.members.add(playlist.id, movie.id).await?;
        self.playlists.touch(playlist.id).await?;
        Ok(())
    }

    /// 移出一部影片。**只能改自定义列表。**
    ///
    /// 影片不存在时**静默返回** —— 不是错误：列表里本来就没有它，目标
    /// 状态已经达成。上游同样直接 `return`。
    ///
    /// 只有真的删掉了行才推进列表的 `updated_at` —— 否则「移出一部不在
    /// 列表里的影片」会刷新排序时间，把列表顶到前面去。
    pub async fn remove_movie(
        &self,
        playlist_id: i32,
        movie_number: &str,
    ) -> Result<(), ServiceError> {
        let playlist = self.require_custom_playlist(playlist_id).await?;
        let Some(movie) = self.movies.find_by_number(movie_number).await? else {
            return Ok(());
        };
        if self.members.remove(playlist.id, movie.id).await? {
            self.playlists.touch(playlist.id).await?;
        }
        Ok(())
    }

    /// 取一部影片，按番号。不存在则 404（详情键是 `movie_number`）。
    async fn require_movie(
        &self,
        movie_number: &str,
    ) -> Result<sm_db::catalog::movie::Movie, ServiceError> {
        self.movies
            .find_by_number(movie_number)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "movie_not_found",
                    "Movie not found",
                    details_of("movie_number", movie_number.trim()),
                )
            })
    }

    // ---------------------------------------------------------------- 系统列表

    /// 取（或首次创建）系统「最近播放」列表。
    ///
    /// **单例**：按 `kind` 查，不存在才建。上游注释写得很直白 ——
    /// 「不允许外部创建多个实例」。
    pub async fn recently_played(&self) -> Result<Playlist, ServiceError> {
        if let Some(existing) = self
            .playlists
            .find_by_system_kind(PLAYLIST_KIND_RECENTLY_PLAYED)
            .await?
        {
            return Ok(existing);
        }
        // `host_owned` 的 `kind` 是 `None`（归一成 `custom`），系统列表必须
        // 显式指定 —— 用 `with_kind` 而不是直接构造 `NewCollection`，后者
        // 会让调用方去拼 `_marker: PhantomData`。
        Ok(self
            .playlists
            .insert(
                &NewCollection::host_owned(
                    RECENTLY_PLAYED_PLAYLIST_NAME,
                    RECENTLY_PLAYED_PLAYLIST_DESCRIPTION,
                )
                .with_kind(PLAYLIST_KIND_RECENTLY_PLAYED),
            )
            .await?)
    }

    /// 把一部影片记入「最近播放」。
    ///
    /// 与 `add_movie` 的区别是**它作用于系统列表** —— 那正是
    /// `_require_custom_playlist` 存在的原因：用户不能手动改最近播放，
    /// 但播放行为本身要能写进去。
    pub async fn touch_recently_played(&self, movie_id: i32) -> Result<(), ServiceError> {
        let playlist = self.recently_played().await?;
        self.members.add(playlist.id, movie_id).await?;
        self.playlists.touch(playlist.id).await?;
        Ok(())
    }
}

/// `DbError` → `ServiceError`。
///
/// 仓储层的错误不带 HTTP 语义，所以统一映射成 500 —— **刻意不**把
/// `DbError::Business` 当成 422：业务错误在 service 层已经各自处理过了，
/// 漏到这里的是「仓储认为不合法而 service 没拦住」的情形，那是服务端
/// 问题而不是用户输入问题。
/// 解析 `field:direction` 排序表达式。
///
/// `None` / 空串 = 不指定，仓储走「列表关系最近触达倒序」。非法值 422
/// `invalid_playlist_filter`，`details.sort` 回显**原始输入**（不是归一后的
/// 小写串）—— 客户端据此高亮它自己填的那个值。
///
/// # 为什么只认这四个字段
///
/// 上游的 `field_map` 就是这四个（`heat` / `release_date` / `added_at` /
/// `bitrate`），其中后两个是相关子查询。多收一个写法不会让它工作，只会让
/// 客户端以为它能用。
fn parse_playlist_sort(
    value: Option<&str>,
) -> Result<Option<(PlaylistMovieCardSort, SortDirection)>, ServiceError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() {
        return Ok(None);
    }
    let (field, direction) = normalized
        .split_once(':')
        .ok_or_else(|| invalid_playlist_sort(raw))?;
    let direction = match direction {
        "asc" => SortDirection::Asc,
        "desc" => SortDirection::Desc,
        _ => return Err(invalid_playlist_sort(raw)),
    };
    let sort = match field {
        "heat" => PlaylistMovieCardSort::Heat,
        "release_date" => PlaylistMovieCardSort::ReleaseDate,
        "added_at" => PlaylistMovieCardSort::AddedAt,
        "bitrate" => PlaylistMovieCardSort::Bitrate,
        _ => return Err(invalid_playlist_sort(raw)),
    };
    Ok(Some((sort, direction)))
}

/// 非法排序表达式。
///
/// **排序与分辨率筛选共用 `invalid_playlist_filter` 这个码** —— 上游
/// `_build_playlist_sort` 与 `resolution_exists_expression` 拿到的是同一个
/// `error_code`。客户端按 `details` 的键（`sort` / `resolution`）区分是哪个
/// 参数错了。
fn invalid_playlist_sort(raw: &str) -> ServiceError {
    ServiceError::validation_with(
        "invalid_playlist_filter",
        "Invalid sort expression",
        details_of("sort", raw),
    )
}

impl From<DbError> for ServiceError {
    fn from(value: DbError) -> Self {
        Self {
            status: 500,
            api: Box::new(sm_core::ApiError::new("internal_error", value.to_string())),
        }
    }
}
