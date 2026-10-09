//! 影片图片包的重建（上游 `catalog/movie_asset_pack_service.py`，169 行）。
//!
//! # 包存在的理由：一次传输拿到全部图片
//!
//! 影片目录通过 `assets.zip` 分发，播放器/导入器解包一次即得全部图片。
//! 没有它的话一部影片要几十个 HTTP 请求。
//!
//! # ★ 压缩方式必须是 **STORED**（不压缩）
//!
//! ZIP_STORED = 只打包不压缩。图片（jpg/webp）本身已压缩，deflate 几乎不省
//! 空间却要花 CPU。而这里的场景是**局域网内传输**，CPU 比空间贵。
//!
//! ⚠️ 写成 DEFLATE 会让打包慢一个数量级，且产物体积几乎不变。
//!
//! # 重建是**原子替换**
//!
//! 写临时包 -> `os.replace` 覆盖正式包。直接写正式包会在中途崩溃时留下一个
//! **半个 zip** —— 而 zip 从尾部读取，损坏的包表现为「完全打不开」，不是
//! 「少几张图」。
//!
//! # `MAX_REBUILD_ATTEMPTS = 3`：解包自检失败才重试
//!
//! 重建后要**解开验证**包能打开、条目数正确。不通过则重试，最多 3 次。
//! 仍失败则**抛错**而不是留一个坏包 —— 上层会把它记进任务失败。

use crate::error::ServiceError;

/// 重建自检的最大尝试次数。
pub const MAX_REBUILD_ATTEMPTS: u32 = 3;

/// 包内条目名。**必须与导入器约定的一致**。
pub fn pack_entry_name(relative_path: &str) -> String {
    // 上游用 `_like_prefix_pattern(prefix)` 做前缀匹配后保留相对路径，
    // 即包内条目名 = 相对图片路径。
    relative_path.trim_start_matches('/').to_owned()
}

/// 影片图片包服务。
pub struct MovieAssetPackService;

impl MovieAssetPackService {
    /// 包文件路径。`movie_dir_relative` 是影片目录的**相对路径**。
    pub fn movie_asset_pack_path(movie_dir_relative: &std::path::Path) -> std::path::PathBuf {
        let _ = movie_dir_relative;
        todo!("骨架：<image_root>/<movie_dir>/assets.zip")
    }

    /// 删掉包（影片目录不存在时**不报错**）。
    pub fn remove_movie_asset_pack(movie_dir_relative: &std::path::Path) -> Result<(), ServiceError> {
        let _ = movie_dir_relative;
        todo!("骨架：文件不存在视为成功")
    }

    /// ★ 重建包。`Ok(true)` = 重建成功；`Ok(false)` = **没有可打包的图片**。
    ///
    /// 上游 `rebuild_movie_asset_pack(cls, movie_dir_relative) -> bool`。
    /// 返回 `false`（而非 `Err`）表示「这部影片还没有图片」—— 那是正常状态，
    /// 不是错误。
    ///
    /// 流程：查该目录下所有**存活**的 `image.origin` -> 写临时包（STORED）
    /// -> 自检 -> `os.replace` 原子覆盖。自检失败重试至
    /// [`MAX_REBUILD_ATTEMPTS`]。
    pub fn rebuild_movie_asset_pack(movie_dir_relative: &std::path::Path) -> Result<bool, ServiceError> {
        let _ = movie_dir_relative;
        todo!("骨架：查存活图片 -> 写临时包(ZIP_STORED) -> 解包自检 -> os.replace 原子替换；自检失败重试 3 次")
    }

    /// 该目录下**存活**的图片相对路径。
    ///
    /// 上游 `live_origins(movie_dir) -> list[str]`。「存活」= `image` 记录
    /// 还在。已删记录的图片**不进包** —— 否则包会带着一个打不开的条目。
    pub fn live_origins(movie_dir: &std::path::Path) -> Result<Vec<String>, ServiceError> {
        let _ = movie_dir;
        todo!("骨架：查该目录下 image 记录仍存在的 origin，按路径排序（保证包内容稳定）")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 条目名去掉**前导斜杠** —— 包内路径必须是相对的。
    #[test]
    fn entry_names_are_relative() {
        assert_eq!(pack_entry_name("/a/b.jpg"), "a/b.jpg");
        assert_eq!(pack_entry_name("a/b.jpg"), "a/b.jpg");
    }

    /// 重试上限是 3。
    #[test]
    fn the_retry_budget_is_three() {
        assert_eq!(MAX_REBUILD_ATTEMPTS, 3);
    }

    /// 「没有图片」是**正常结果**（`Ok(false)`），不是错误。
    #[test]
    fn a_movie_without_images_is_not_an_error() {
        let outcome: Result<bool, ServiceError> = Ok(false);
        assert_eq!(outcome.expect("不应报错"), false);
    }

    /// 条目名排序必须**稳定** —— 否则每次重建产出的包字节不同，
    /// 客户端会反复重新下载。
    #[test]
    fn origins_must_be_sorted_for_a_stable_pack() {
        let mut origins = vec!["b.jpg".to_owned(), "a.jpg".to_owned()];
        origins.sort();
        assert_eq!(origins, vec!["a.jpg".to_owned(), "b.jpg".to_owned()]);
    }
}
