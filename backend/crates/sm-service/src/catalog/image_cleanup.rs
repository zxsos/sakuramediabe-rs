//! Image 记录与物理文件的清理（上游 `catalog/image_cleanup_service.py`，157 行）。
//!
//! # 「记录」与「文件」是两套东西，删除必须成对
//!
//! | 只删 | 后果 |
//! |---|---|
//! | 只删记录 | 磁盘留下没人引用的孤儿文件（`assets.zip` 里也还打包着） |
//! | 只删文件 | DB 指向打不开的图片，前端显示裂图 |
//!
//! 所以调用方要连着调 [`ImageCleanupService::delete_image_record_if_unused`] 与
//! [`ImageCleanupService::delete_obsolete_image_files`]：前者返回「记录没了、
//! 文件该删」的那批路径，后者拿它去删磁盘。
//!
//! # 判据是「现在还有没有人引用」，且那条判据只此一份
//!
//! 一张图可能被**八处**引用。清单与 SQL 都在 `sm_db::repo::image`
//! （[`IMAGE_REFERENCE_SITES`] / `ImageRepository::is_referenced`），这里只做
//! 转调 —— 本模块自己再列一遍就会和那边分叉，而分叉的后果是**删掉正在使用的
//! 图**，不报错，只表现为「封面忽然裂了」。
//!
//! # 删文件分三种布局
//!
//! | 布局 | 做法 |
//! |---|---|
//! | 未打包的普通文件 | 直接 `unlink` |
//! | `assets.zip` | **转 `MovieAssetPackService` 重建**（以数据库活跃集为准）|
//! | `thumbnails.zip` | 包内已无存活条目则删包，否则重建包 |
//!
//! 顺序：先删记录，再重建包 —— `assets.zip` 里存的是记录的副本，反过来会留下
//! 一个引用已删记录的包。
//!
//! # 顺序的另一半：先删记录，**再**删文件
//!
//! 反过来（先删文件）在中间崩溃时会留下「DB 指向打不开的图」—— 那是**显示
//! 错误**；而先删记录后崩溃留下的是「没人引用的孤儿文件」——那是**磁盘浪费**。
//! 后者可恢复，前者不可。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use sm_db::repo::ImageRepository;
use sm_db::Db;

use crate::catalog::image_store::{read_pack_entry, write_pack};
use crate::catalog::media_paths::{
    self, image_pack_relative_path, remove_file_if_exists, resolve_inside, MOVIE_ASSETS_PACK_NAME,
};
use crate::catalog::movie_asset_pack::MovieAssetPackService;
use crate::error::ServiceError;
use crate::system::config::ConfigService;

pub use sm_db::repo::IMAGE_REFERENCE_SITES;

/// 删除 -> 重建的顺序。**纯函数**，供测试锁住。
pub fn cleanup_steps() -> [&'static str; 2] {
    ["delete_record", "rebuild_pack"]
}

/// 图片清理服务。
///
/// # 骨架期那个 `RebuildPackHook` 已删除
///
/// 它原本是 `Box<dyn Fn(i64) -> Result<(), ServiceError>>`，收 `movie_id`。
/// 但那**形状是错的**：上游 `_delete_or_rebuild_pack` 收的是**目录**
/// （`pack_relative.parent`），而且它现在**根本不需要** —— 包重建已经走
/// [`MovieAssetPackService`]（那要 `Db` + `ConfigService`，一个闭包表达不了）。
/// 留着它只会让下一个人照着错的形状去实现。
pub struct ImageCleanupService {
    db: Db,
    config: ConfigService,
}

impl ImageCleanupService {
    /// 构造。
    ///
    /// 需要 `Db`（判据、删记录、查活跃集）与 `ConfigService`（图片根目录从配置
    /// 读，写死无法在测试里隔离）。
    pub fn new(db: &Db, config: &ConfigService) -> Self {
        Self {
            db: db.clone(),
            config: config.clone(),
        }
    }

    /// 图片根目录。**从配置读**（`media.import_image_root_path`）。
    ///
    /// 实现在 [`media_paths::media_image_root_path`] —— 影片包那边用同一个
    /// （`~` 展开 + 相对路径按 cwd 解析 + 缺省值三条规则只写一遍）。
    pub fn image_root_path(&self) -> Result<PathBuf, ServiceError> {
        media_paths::media_image_root_path(&self.config)
    }

    /// ★ 删掉已无人引用的图片记录，返回被删记录的 `origin` 集合。
    ///
    /// `image_id = None` 返回**空集**而非报错 —— 调用方拿到 None 说明记录
    /// 本来就查不到，报错会让「本来就很干净」的库无法完成清理。
    ///
    /// `origin` 为空串时也不返回（上游 `{relative_path} if relative_path else set()`）：
    /// 空路径进到删文件那一层没有意义，而它会被当成「图片根目录本身」。
    pub async fn delete_image_record_if_unused(
        &self,
        image_id: Option<i32>,
    ) -> Result<Vec<String>, ServiceError> {
        let Some(image_id) = image_id else {
            return Ok(Vec::new());
        };
        // 「查引用方」与「删记录」在同一个事务里 —— 见
        // `ImageRepository::delete_if_unreferenced` 的文档（分两步会删掉
        // 刚被别人挂上的图）。
        let deleted = ImageRepository::new(self.db.clone())
            .delete_if_unreferenced(image_id)
            .await?;
        match deleted {
            Some(origin) if !origin.trim().is_empty() => Ok(vec![origin]),
            _ => Ok(Vec::new()),
        }
    }

    /// ★ 删掉不再被任何记录指向的**物理文件**，返回真删掉的个数。
    ///
    /// ⚠️ 直接操作文件系统。每条相对路径都要先确认解析结果**在
    /// [`Self::image_root_path`] 之内**（见 `media_paths::resolve_inside`）——
    /// 上游没有这层检查，这是本仓的加固。
    ///
    /// 调用方传进来的路径理应是刚被 [`Self::delete_image_record_if_unused`]
    /// 删掉记录的那批，所以这里**不重复查引用**：判据只此一份，查两遍就是
    /// 两个地方可能分叉。
    ///
    /// 返回值口径：删掉的**普通文件**每个计 1；删掉一个整包计 1；重建包计 0
    /// （没有「少一个文件」，是整包重写）。上游不返回计数，这是本仓为可观测性
    /// 加的 —— 所以别把它当成「清理了几个 origin」。
    pub async fn delete_obsolete_image_files(
        &self,
        relative_paths: &[String],
    ) -> Result<u64, ServiceError> {
        if relative_paths.is_empty() {
            return Ok(0);
        }
        let root = self.image_root_path()?;

        // 去空白、去重、排序。排序照上游（`sorted(relative_paths)`）：
        // 包内成员的顺序影响重建后的包内容，不稳定会让「同样的输入产出不同的
        // 包」，而包是要按内容比对/缓存的。
        let mut targets: Vec<&str> = relative_paths
            .iter()
            .map(|path| path.trim())
            .filter(|path| !path.is_empty())
            .collect();
        targets.sort_unstable();
        targets.dedup();

        let mut deleted = 0_u64;
        // `BTreeMap` 而不是 `HashMap`：包的重建顺序也要稳定。
        let mut pack_members: BTreeMap<PathBuf, Vec<String>> = BTreeMap::new();
        for relative in targets {
            match image_pack_relative_path(relative) {
                // 不在包里：就是图片根下的一个普通文件。
                None => {
                    if remove_file_if_exists(&resolve_inside(&root, relative)?)? {
                        deleted += 1;
                    }
                }
                Some(pack) => pack_members
                    .entry(pack)
                    .or_default()
                    .push(relative.to_owned()),
            }
        }

        for (pack, members) in pack_members {
            deleted += self.delete_or_rebuild_pack(&root, &pack, &members).await?;
        }
        Ok(deleted)
    }

    /// 该记录是否仍被引用。上游 `image_record_is_still_used(image)`。
    ///
    /// 转调仓储 —— 判据（八处引用方）只此一份，见模块文档。
    pub async fn image_record_is_still_used(&self, image_id: i32) -> Result<bool, ServiceError> {
        Ok(ImageRepository::new(self.db.clone())
            .is_referenced(image_id)
            .await?)
    }

    /// 包内成员的清理。上游 `_delete_or_rebuild_pack`。
    ///
    /// # 为什么判据是「包在不在」而不是「origin 长什么样」
    ///
    /// `image_pack_relative_path` 只回答「这个路径**按约定**属于哪个包」
    /// （它明说了不检查包是否存在）。包还没建（未打包的旧布局）时，那些文件
    /// 就还是平铺的普通文件 —— 所以这里必须再问一次磁盘。
    async fn delete_or_rebuild_pack(
        &self,
        root: &Path,
        pack_relative: &Path,
        members: &[String],
    ) -> Result<u64, ServiceError> {
        let pack_path = resolve_inside(root, &pack_relative.to_string_lossy())?;

        if !pack_path.is_file() {
            // 未打包的旧布局：维持逐个文件删除。
            let mut deleted = 0_u64;
            for member in members {
                if remove_file_if_exists(&resolve_inside(root, member)?)? {
                    deleted += 1;
                }
            }
            return Ok(deleted);
        }

        // 包目录的相对路径（`assets.zip` 与 `thumbnails.zip` 都靠它定位）。
        let Some(pack_dir) = pack_relative.parent() else {
            return Ok(0);
        };

        if pack_relative
            .file_name()
            .is_some_and(|name| name == MOVIE_ASSETS_PACK_NAME)
        {
            // 影片图片包：以**数据库活跃集**为准重建。被删的那几条自然不在新包里
            // —— 不必（也不能）逐个从 zip 里摘出来。活跃集为空时服务会删包。
            MovieAssetPackService::new(&self.db, &self.config)
                .rebuild_movie_asset_pack(pack_dir)
                .await?;
            return Ok(0);
        }

        // 其余（`thumbnails.zip`）：包内还有没有**存活**条目，看数据库。
        let thumbnails_prefix = format!(
            "{}/",
            pack_dir
                .join(
                    pack_relative
                        .file_stem()
                        .map(|stem| stem.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                )
                .to_string_lossy()
                .replace('\\', "/")
        );
        let remaining = self.origins_under_prefix(&thumbnails_prefix).await?;
        if remaining.is_empty() {
            // 包内没有任何存活条目 —— 整包删掉。
            remove_file_if_exists(&pack_path)?;
            return Ok(1);
        }

        // 还有被时刻点钉住的条目：重建包，维持「包 == 数据库活跃集合」。
        self.rebuild_thumbnail_pack(&pack_path, &remaining)?;
        Ok(0)
    }

    /// 前缀之下的**存活** `origin`（去重、升序）。
    ///
    /// 与 `MovieAssetPackService::live_origins` 的差别：那个只取**直接子文件**
    /// （影片资产是平铺的），这里要取**整个子树**（缩略图按
    /// `media/<media_id>/thumbnails/` 分组，但包内条目名只有文件名，所以子目录
    /// 层级在这里是透明的）。
    async fn origins_under_prefix(&self, prefix: &str) -> Result<Vec<String>, ServiceError> {
        let pattern = ImageRepository::like_prefix(prefix);
        let origins = ImageRepository::new(self.db.clone())
            .list_origins_by_pattern(&pattern)
            .await?;
        // `LIKE` 是索引友好的粗筛（`_` 已转义，但尾部仍可能有别的候选），
        // 精确判定在这一侧 —— 上游同款两步。
        Ok(origins
            .into_iter()
            .filter(|origin| origin.starts_with(prefix))
            .collect())
    }

    /// 用给定的 `origin` 集合重写缩略图包。上游 `_rebuild_thumbnail_pack`。
    ///
    /// 字节从**旧包**里取（缩略图打包后 loose 文件就没了，旧包是唯一的来源）。
    ///
    /// # 两条「宁可不做」的分支
    ///
    /// 1. 包内一个条目都读不出来 → **保留旧包、直接返回**。那属于异常状态
    ///    （磁盘坏了或包被替换过），而重写会产出一个空包 —— 等于把数据删了。
    /// 2. 写临时包失败 → 删掉临时包再向上报错。**正式包一个字节都没动**。
    ///
    /// 两者都遵循同一条：失败时**保持现状**，绝不留下一个已知不完整的包。
    fn rebuild_thumbnail_pack(
        &self,
        pack_path: &Path,
        origins: &[String],
    ) -> Result<(), ServiceError> {
        let mut entries: Vec<(String, Vec<u8>)> = Vec::with_capacity(origins.len());
        for origin in origins {
            let entry_name = crate::catalog::movie_asset_pack::pack_entry_name(origin);
            match read_pack_entry(pack_path, &entry_name) {
                Some(bytes) => entries.push((entry_name, bytes)),
                None => tracing::warn!(
                    pack = %pack_path.display(),
                    %entry_name,
                    "缩略图包内缺少待保留的条目（数据库说有、包里没有）"
                ),
            }
        }
        if entries.is_empty() {
            tracing::warn!(
                pack = %pack_path.display(),
                "缩略图包内没有任何可保留的条目，保留旧包不重写"
            );
            return Ok(());
        }

        let tmp_path = pack_path.with_file_name(format!(
            "{}.tmp-{}",
            pack_path.file_name().map_or_else(
                || "thumbnails.zip".to_owned(),
                |n| n.to_string_lossy().into_owned()
            ),
            uuid::Uuid::new_v4().simple()
        ));
        if let Err(error) = write_pack(&tmp_path, &entries) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(error);
        }
        std::fs::rename(&tmp_path, pack_path).map_err(|error| {
            let _ = std::fs::remove_file(&tmp_path);
            ServiceError::from(sm_db::DbError::business(
                "ImageCleanup",
                format!("缩略图包原子替换失败：{error}"),
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 顺序必须先删记录、后重建包。
    #[test]
    fn the_record_goes_before_the_pack_rebuild() {
        assert_eq!(cleanup_steps(), ["delete_record", "rebuild_pack"]);
    }

    /// 引用方恰好八处，一个都不能少 —— 且**表名是真名**。
    ///
    /// 骨架期这份清单只有五项，且把 `movie_plot_image` 写成了 `plot_image`
    /// （那张表不存在）。漏掉 `media_point.image_id` 的后果最直接：它
    /// `NOT NULL` + `RESTRICT`，漏查等于每次删时刻点都顺手删掉那张图。
    ///
    /// 这条只锁常量本身；**与 DDL 的一致性由集成测试
    /// `tests/image_reference_sites.rs` 保证**（它直接读 `information_schema`）。
    #[test]
    fn all_eight_reference_sites_are_listed() {
        assert_eq!(IMAGE_REFERENCE_SITES.len(), 8);
        for site in [
            "movie.cover_image_id",
            "movie.thin_cover_image_id",
            "actor.profile_image_id",
            "actor.profile_image_override_id",
            "movie_plot_image.image_id",
            "media_thumbnail.image_id",
            "media_point.image_id",
            "video_item.cover_image_id",
        ] {
            assert!(IMAGE_REFERENCE_SITES.contains(&site), "{site} 漏了");
        }
    }
}
