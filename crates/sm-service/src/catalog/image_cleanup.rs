//! Image 记录与物理文件的清理（上游 `catalog/image_cleanup_service.py`，157 行）。
//!
//! # 「记录」与「文件」是两套东西，删除必须成对
//!
//! | 只删 | 后果 |
//! |---|---|
//! | 只删记录 | 磁盘留下没人引用的孤儿文件（`assets.zip` 里也还打包着） |
//! | 只删文件 | DB 指向打不开的图片，前端显示裂图 |
//!
//! # 判据是「现在还有没有人引用」
//!
//! 一张图可能被五处引用（见 [`IMAGE_REFERENCE_SITES`]）。漏查一个就会删掉
//! 正在使用的图 —— 而且不会有任何报错，只表现为「封面忽然裂了」。
//!
//! # 顺序：先删记录，再重建包
//!
//! `assets.zip` 里存的是记录的副本。反过来会留下一个引用已删记录的包。
//! 重建通过回调注入 —— 本模块被「导入时」与「媒体硬删除」两处调用，而重建
//! 包只对影片目录有意义，直接依赖会把两件事绑死。

use crate::error::ServiceError;

/// 全部引用方。**漏一个就会删掉在用的图。**
pub const IMAGE_REFERENCE_SITES: [&str; 5] = [
    "movie.cover_image_id",
    "movie.thin_cover_image_id",
    "media_thumbnail.image_id",
    "plot_image.image_id",
    "actor.profile_image_id",
];

/// 删除 -> 重建的顺序。**纯函数**，供测试锁住。
pub fn cleanup_steps() -> [&'static str; 2] {
    ["delete_record", "rebuild_pack"]
}

/// 图片清理服务。
pub struct ImageCleanupService {
    /// 重建包的回调（接收 `movie_id`）。
    rebuild_pack_hook: Option<Box<dyn Fn(i64) -> Result<(), ServiceError>>>,
}

impl ImageCleanupService {
    /// 构造。
    pub fn new(rebuild_pack_hook: Box<dyn Fn(i64) -> Result<(), ServiceError>>) -> Self {
        Self {
            rebuild_pack_hook: Some(rebuild_pack_hook),
        }
    }

    /// 图片根目录。**从配置读**（`import_image_root_path`），写死无法隔离测试。
    pub fn image_root_path() -> std::path::PathBuf {
        todo!("骨架：从 config 读 import_image_root_path")
    }

    /// ★ 删掉已无人引用的图片记录，返回被删记录的 `origin` 集合。
    ///
    /// `image_id = None` 返回**空集**而非报错 —— 调用方拿到 None 说明记录
    /// 本来就查不到，报错会让「本来就很干净」的库无法完成清理。
    pub async fn delete_image_record_if_unused(
        &self,
        image_id: Option<i32>,
    ) -> Result<Vec<String>, ServiceError> {
        let _ = image_id;
        todo!("骨架：查全部引用方 -> 无引用则删记录并返回 origin 集合")
    }

    /// ★ 删掉不再被任何记录指向的**物理文件**。
    ///
    /// ⚠️ 直接操作文件系统。路径必须确认在 [`Self::image_root_path`] 之内 ——
    /// 上游有前缀越界防护（`../../etc/passwd` 那种）。**不要**因为「路径来自
    /// 数据库」就跳过这层检查。
    pub async fn delete_obsolete_image_files(
        &self,
        relative_paths: &[String],
    ) -> Result<u64, ServiceError> {
        let _ = relative_paths;
        todo!("骨架：逐个确认无引用 + 确认在 image_root 之内（防逃逸）-> 删文件")
    }

    /// 该记录是否仍被引用。上游 `image_record_is_still_used(image)`。
    pub async fn image_record_is_still_used(&self, image_id: i32) -> Result<bool, ServiceError> {
        let _ = image_id;
        todo!("骨架：查全部引用方；任一命中即为 true")
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

    /// 引用方恰好五处，一个都不能少。
    #[test]
    fn all_five_reference_sites_are_listed() {
        assert_eq!(IMAGE_REFERENCE_SITES.len(), 5);
        for site in ["movie.cover_image_id", "actor.profile_image_id"] {
            assert!(IMAGE_REFERENCE_SITES.contains(&site), "{site} 漏了");
        }
    }
}
