//! 插件侧合集 service。
//!
//! 对应上游 `src/service/collections/plugin_collection_service.py`（217 行）。
//!
//! # 这是**给插件 facade 用的**另一套入口
//!
//! `PlaylistService` 那套给终端用户用：名称全局唯一、系统列表不可改、
//! 保留名不可占用。本模块给**插件**用，规则不同：
//!
//! | | 用户侧 | 插件侧 |
//! |---|---|---|
//! | 定位方式 | 名称（全局唯一） | `(plugin_id, plugin_key)` |
//! | 找不到时 | 404 | `ensure_*` **创建**；`set_*` 404 |
//! | 参数校验失败 | 422 | `ValueError` → **500** |
//!
//! 最后一行是这批最需要说清的一处，见 [`ProgrammerError`]。
//!
//! # `ensure_*` 与 `set_*` 的区别是本模块的核心
//!
//! - **`ensure_*`** = get-or-create，**并且会把名字同步成给定的那个**。
//!   上游 `_ensure_collection` 按 `(owner, key)` 找到后比 name 与
//!   description，不同就改。所以插件每次启动调一次 `ensure_playlist`，
//!   是把列表重命名成代码里写的那个名字 —— 这正是想要的：显示名由插件决定。
//! - **`set_*`** = 要求合集**已经存在**（`_require_owned`，找不到 404
//!   `plugin_collection_not_found`），然后替换成员。
//!
//! 混用这两者是危险的：`set_*` 若走 `ensure_*`，一个拼错的 key 会**静默
//! 创建一个空合集**而不是报错，插件会以为设置成功了。
//!
//! # `plugin_key` 上限 128，名称上限 255
//!
//! 两者不同：`plugin_key varchar(128)` 而 `name varchar(255)`。

use sm_db::collections::{ClipCollection, MomentCollection, Playlist};
use sm_db::repo::{
    ClipCollectionRepository, MomentCollectionRepository, MovieRepository, NewCollection,
    PlaylistMovieRepository, PlaylistRepository,
};
use sm_db::Db;

use crate::collections::ordered::{ClipCollectionService, MomentCollectionService};
use crate::error::{details_of, ProgrammerError, ServiceError};

/// `plugin_key` 的长度上限。对应 `plugin_key varchar(128)`。
const PLUGIN_KEY_MAX: usize = 128;
/// 合集名称的长度上限。对应 `name varchar(255)`。
const COLLECTION_NAME_MAX: usize = 255;

/// 校验并归一 `plugin_key`。失败抛 [`ProgrammerError`] 而**不是** 422。
fn validate_key(plugin_key: &str) -> Result<String, ProgrammerError> {
    let key = plugin_key.trim();
    if key.is_empty() {
        return Err(ProgrammerError::new("plugin_key 不能为空"));
    }
    if key.chars().count() > PLUGIN_KEY_MAX {
        return Err(ProgrammerError::new(format!(
            "plugin_key 不能超过 {PLUGIN_KEY_MAX} 个字符"
        )));
    }
    Ok(key.to_owned())
}

/// 校验并归一合集名称。失败同样是 [`ProgrammerError`]。
fn normalize_name(name: &str) -> Result<String, ProgrammerError> {
    let normalized = name.trim();
    if normalized.is_empty() {
        return Err(ProgrammerError::new("合集名称不能为空"));
    }
    if normalized.chars().count() > COLLECTION_NAME_MAX {
        return Err(ProgrammerError::new(format!(
            "合集名称不能超过 {COLLECTION_NAME_MAX} 个字符"
        )));
    }
    Ok(normalized.to_owned())
}

/// 归一描述。`None` → 空串。
fn normalize_description(description: Option<&str>) -> String {
    description.unwrap_or_default().trim().to_owned()
}

/// 名字或 key 撞唯一约束时的 409。details 同时带 key 与名字。
///
/// 插件需要能区分「key 撞了」与「名字撞了」，而裸约束错误不区分。
fn conflict(plugin_key: &str, name: &str, message: &str) -> ServiceError {
    let mut details = details_of("plugin_key", plugin_key);
    details.insert("name".to_owned(), name.to_owned().into());
    ServiceError::conflict("plugin_collection_conflict", message, Some(details))
}

/// 插件侧合集 service。
pub struct PluginCollectionService {
    playlists: PlaylistRepository,
    playlist_movies: PlaylistMovieRepository,
    movies: MovieRepository,
    moments: MomentCollectionRepository,
    moment_points: MomentCollectionService,
    clips: ClipCollectionRepository,
    clip_clips: ClipCollectionService,
}

impl PluginCollectionService {
    pub fn new(db: &Db) -> Self {
        Self {
            playlists: PlaylistRepository::new(db.clone()),
            playlist_movies: PlaylistMovieRepository::new(db.clone()),
            movies: MovieRepository::new(db.clone()),
            moments: MomentCollectionRepository::new(db.clone()),
            moment_points: MomentCollectionService::new(db),
            clips: ClipCollectionRepository::new(db.clone()),
            clip_clips: ClipCollectionService::new(db),
        }
    }

    /// 按 `(plugin_id, plugin_key)` 取一个**已存在**的合集。
    ///
    /// 不存在则 404 `plugin_collection_not_found`。这是 `set_*` 系列的
    /// 入口 —— 它们**不**创建合集。
    async fn require_owned_playlist(
        &self,
        plugin_id: &str,
        plugin_key: &str,
    ) -> Result<Playlist, ServiceError> {
        let key = validate_key(plugin_key)?;
        self.playlists
            .find_by_plugin_key(plugin_id, &key)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "plugin_collection_not_found",
                    "插件合集不存在",
                    details_of("plugin_key", key.as_str()),
                )
            })
    }

    async fn require_owned_moment(
        &self,
        plugin_id: &str,
        plugin_key: &str,
    ) -> Result<MomentCollection, ServiceError> {
        let key = validate_key(plugin_key)?;
        self.moments
            .find_by_plugin_key(plugin_id, &key)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "plugin_collection_not_found",
                    "插件合集不存在",
                    details_of("plugin_key", key.as_str()),
                )
            })
    }

    async fn require_owned_clip(
        &self,
        plugin_id: &str,
        plugin_key: &str,
    ) -> Result<ClipCollection, ServiceError> {
        let key = validate_key(plugin_key)?;
        self.clips
            .find_by_plugin_key(plugin_id, &key)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "plugin_collection_not_found",
                    "插件合集不存在",
                    details_of("plugin_key", key.as_str()),
                )
            })
    }

    // ---------------------------------------------------------------- playlist

    /// 取或建插件的播放列表，**并把名字同步成给定的那个**。
    pub async fn ensure_playlist(
        &self,
        plugin_id: &str,
        plugin_key: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<Playlist, ServiceError> {
        let key = validate_key(plugin_key)?;
        let name = normalize_name(name)?;
        let description = normalize_description(description);

        match self.playlists.find_by_plugin_key(plugin_id, &key).await? {
            Some(mut existing) => {
                if existing.name != name || existing.description != description {
                    self.playlists
                        .rename(existing.id, &name)
                        .await
                        .map_err(|_| conflict(&key, &name, "插件合集名称已存在"))?;
                    self.playlists
                        .set_description(existing.id, &description)
                        .await
                        .map_err(|_| conflict(&key, &name, "插件合集名称已存在"))?;
                    existing.name = name;
                    existing.description = description;
                }
                Ok(existing)
            }
            None => self
                .playlists
                .insert(&NewCollection::plugin_owned(
                    plugin_id,
                    &key,
                    &name,
                    &description,
                ))
                .await
                .map_err(|_| conflict(&key, &name, "插件合集名称或 key 已存在")),
        }
    }

    /// 把插件播放列表的影片**替换**为给定的一组。
    ///
    /// 按 `movie_number` 而非 id —— 插件手里通常只有番号，而那是影片的
    /// 业务标识。
    ///
    /// **要求合集已存在**（`_require_owned`）—— 见模块文档。
    ///
    /// 去重按 movie id 且保留首次出现。上游把整个「清空 + 重建」放在一个
    /// 事务里；这里同样在写之前**先**把全部番号解析完，避免「清空之后才
    /// 发现有一个番号不存在」——那时列表已经空了。
    pub async fn set_playlist_movies(
        &self,
        plugin_id: &str,
        plugin_key: &str,
        movie_numbers: &[String],
    ) -> Result<Playlist, ServiceError> {
        let playlist = self.require_owned_playlist(plugin_id, plugin_key).await?;

        let mut ids: Vec<i32> = Vec::with_capacity(movie_numbers.len());
        for raw in movie_numbers {
            // 空白番号是**编程错误**（上游抛 ValueError），不是用户输入问题。
            let number = raw.trim();
            if number.is_empty() {
                return Err(ProgrammerError::new("movie_number 不能为空").into());
            }
            let movie = self.movies.find_by_number(number).await?.ok_or_else(|| {
                ServiceError::not_found_with(
                    "movie_not_found",
                    "影片不存在",
                    details_of("movie_number", number),
                )
            })?;
            if !ids.contains(&movie.id) {
                ids.push(movie.id);
            }
        }

        let existing: Vec<i32> = self
            .playlist_movies
            .list_by_playlist(playlist.id)
            .await?
            .iter()
            .map(|r| r.movie_id)
            .collect();
        for id in existing {
            self.playlist_movies.remove(playlist.id, id).await?;
        }
        for id in &ids {
            self.playlist_movies.add(playlist.id, *id).await?;
        }
        self.playlists.touch(playlist.id).await?;
        Ok(playlist)
    }

    // ---------------------------------------------------------------- moment

    /// 取或建插件的时刻合集。
    pub async fn ensure_moment(
        &self,
        plugin_id: &str,
        plugin_key: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<MomentCollection, ServiceError> {
        let key = validate_key(plugin_key)?;
        let name = normalize_name(name)?;
        let description = normalize_description(description);

        match self.moments.find_by_plugin_key(plugin_id, &key).await? {
            Some(mut existing) => {
                if existing.name != name || existing.description != description {
                    self.moments
                        .rename(existing.id, &name)
                        .await
                        .map_err(|_| conflict(&key, &name, "插件合集名称已存在"))?;
                    self.moments
                        .set_description(existing.id, &description)
                        .await
                        .map_err(|_| conflict(&key, &name, "插件合集名称已存在"))?;
                    existing.name = name;
                    existing.description = description;
                }
                Ok(existing)
            }
            None => self
                .moments
                .insert(&NewCollection::plugin_owned(
                    plugin_id,
                    &key,
                    &name,
                    &description,
                ))
                .await
                .map_err(|_| conflict(&key, &name, "插件合集名称或 key 已存在")),
        }
    }

    /// 把时刻合集的成员替换为给定的 `media_point` id 顺序。
    ///
    /// 委托给 [`MomentCollectionService::set_members`] —— 上游也是这么写的
    /// （`MomentCollectionService.set_points(...)`），所以去重、顺序、
    /// 事务边界只有一份实现。
    pub async fn set_moment_points(
        &self,
        plugin_id: &str,
        plugin_key: &str,
        point_ids: &[i32],
    ) -> Result<MomentCollection, ServiceError> {
        let collection = self.require_owned_moment(plugin_id, plugin_key).await?;
        self.moment_points
            .set_members(collection.id, point_ids)
            .await?;
        Ok(collection)
    }

    // ---------------------------------------------------------------- clip

    /// 取或建插件的片段合集。
    pub async fn ensure_clip(
        &self,
        plugin_id: &str,
        plugin_key: &str,
        name: &str,
        description: Option<&str>,
    ) -> Result<ClipCollection, ServiceError> {
        let key = validate_key(plugin_key)?;
        let name = normalize_name(name)?;
        let description = normalize_description(description);

        match self.clips.find_by_plugin_key(plugin_id, &key).await? {
            Some(mut existing) => {
                if existing.name != name || existing.description != description {
                    self.clips
                        .rename(existing.id, &name)
                        .await
                        .map_err(|_| conflict(&key, &name, "插件合集名称已存在"))?;
                    self.clips
                        .set_description(existing.id, &description)
                        .await
                        .map_err(|_| conflict(&key, &name, "插件合集名称已存在"))?;
                    existing.name = name;
                    existing.description = description;
                }
                Ok(existing)
            }
            None => self
                .clips
                .insert(&NewCollection::plugin_owned(
                    plugin_id,
                    &key,
                    &name,
                    &description,
                ))
                .await
                .map_err(|_| conflict(&key, &name, "插件合集名称或 key 已存在")),
        }
    }

    /// 把片段合集的成员替换为给定的 `media_clip` id 顺序。
    ///
    /// 委托给 [`ClipCollectionService::set_members`]，理由同
    /// [`Self::set_moment_points`]。
    pub async fn set_clip_clips(
        &self,
        plugin_id: &str,
        plugin_key: &str,
        clip_ids: &[i32],
    ) -> Result<ClipCollection, ServiceError> {
        let collection = self.require_owned_clip(plugin_id, plugin_key).await?;
        self.clip_clips.set_members(collection.id, clip_ids).await?;
        Ok(collection)
    }
}
