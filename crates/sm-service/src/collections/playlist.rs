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

use sm_db::collections::{
    Playlist, PLAYLIST_KIND_RECENTLY_PLAYED, RECENTLY_PLAYED_PLAYLIST_DESCRIPTION,
    RECENTLY_PLAYED_PLAYLIST_NAME,
};
use sm_db::error::DbError;
use sm_db::repo::{MovieRepository, NewCollection, PlaylistMovieRepository, PlaylistRepository};
use sm_db::Db;

use crate::error::{details_of, ServiceError};

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

/// 播放列表 service。
///
/// 方法都是 `&self` 上的异步函数，持有仓储而不是用类方法 —— Rust 没有
/// 上游那种「全类方法 + ORM 全局连接」的写法，依赖必须显式持有。
pub struct PlaylistService {
    playlists: PlaylistRepository,
    members: PlaylistMovieRepository,
    movies: MovieRepository,
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
impl From<DbError> for ServiceError {
    fn from(value: DbError) -> Self {
        Self {
            status: 500,
            api: Box::new(sm_core::ApiError::new("internal_error", value.to_string())),
        }
    }
}
