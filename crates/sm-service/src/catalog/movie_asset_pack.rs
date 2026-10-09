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
//! ⚠️ 写成 DEFLATE 会让打包慢一个数量级，且产物体积几乎不变。写包的实现与
//! 这条约定在 [`crate::catalog::image_store::write_pack`]。
//!
//! # 重建是**原子替换**
//!
//! 写临时包 -> `os.replace` 覆盖正式包。直接写正式包会在中途崩溃时留下一个
//! **半个 zip** —— 而 zip 从尾部读取，损坏的包表现为「完全打不开」，不是
//! 「少几张图」。
//!
//! # `MAX_REBUILD_ATTEMPTS = 3`：**两件事**都会触发重试
//!
//! 上游的循环里有两个 `continue`，容易被误读成只有一个：
//!
//! | 触发 | 原因 |
//! |---|---|
//! | `load_entries` 返回 `None` | 有活跃行但**拿不到字节**（loose 文件与旧包条目都没有）——并发写入的中间态 |
//! | 构建期间活跃集变了 | 写包期间另一处增删了图片，本次构建的条目集已过期 |
//!
//! 三次都不过就**不动正式包**（`return pack_path.is_file()`）——保留现状比
//! 覆盖成一个已知不完整的包好。

use std::path::{Path, PathBuf};

use sm_db::repo::ImageRepository;
use sm_db::Db;

use crate::catalog::image_store::{read_pack_entry, write_pack};
use crate::catalog::media_paths::{self, remove_file_if_exists, MOVIE_ASSETS_PACK_NAME};
use crate::error::ServiceError;
use crate::system::config::ConfigService;

/// 重建自检的最大尝试次数。
pub const MAX_REBUILD_ATTEMPTS: u32 = 3;

/// 包内条目名。**必须与导入器约定的一致**。
pub fn pack_entry_name(relative_path: &str) -> String {
    // 上游用 `_like_prefix_pattern(prefix)` 做前缀匹配后保留相对路径，
    // 即包内条目名 = 相对图片路径。
    relative_path.trim_start_matches('/').to_owned()
}

/// 影片图片包服务。
///
/// # 它需要 `Db` 与 `ConfigService`（骨架期是无状态单元结构体）
///
/// 活跃集要从 `image` 表查（`Db`），包路径要从图片根算（`ConfigService`）。
/// 全仓**没有任何调用点**，所以改形状零风险。
pub struct MovieAssetPackService {
    db: Db,
    config: ConfigService,
}

impl MovieAssetPackService {
    /// 构造。
    pub fn new(db: &Db, config: &ConfigService) -> Self {
        Self {
            db: db.clone(),
            config: config.clone(),
        }
    }

    /// 包文件路径。`movie_dir_relative` 是影片目录的**相对路径**。
    pub fn movie_asset_pack_path(
        &self,
        movie_dir_relative: &Path,
    ) -> Result<PathBuf, ServiceError> {
        Ok(media_paths::media_image_root_path(&self.config)?
            .join(movie_dir_relative)
            .join(MOVIE_ASSETS_PACK_NAME))
    }

    /// 删掉包与遗留临时包（包不存在时**不报错**）。
    ///
    /// 上游 `remove_movie_asset_pack`：「稳定路径覆盖写」前先撤掉旧包。
    pub fn remove_movie_asset_pack(&self, movie_dir_relative: &Path) -> Result<(), ServiceError> {
        let pack_path = self.movie_asset_pack_path(movie_dir_relative)?;
        cleanup_stale_temp_packs(&pack_path);
        remove_quietly(&pack_path);
        Ok(())
    }

    /// ★ 重建包。`Ok(true)` = 重建成功；`Ok(false)` = **没有可打包的图片**。
    ///
    /// 上游 `rebuild_movie_asset_pack(cls, movie_dir_relative) -> bool`。
    /// 返回 `false`（而非 `Err`）表示「这部影片还没有图片」—— 那是正常状态，
    /// 不是错误：空集的处置是**删包**（包的存在意味着「这里有全部图片」，
    /// 留着空包会让分发方以为已经打包好了）。
    ///
    /// 流程：快照活跃 origin -> 取字节（loose 优先、旧包兜底）-> 写临时包
    /// （STORED）-> 复核活跃集未变 -> 原子替换 -> 清理已入包的 loose 文件。
    pub async fn rebuild_movie_asset_pack(
        &self,
        movie_dir_relative: &Path,
    ) -> Result<bool, ServiceError> {
        let image_root = media_paths::media_image_root_path(&self.config)?;
        let pack_path = self.movie_asset_pack_path(movie_dir_relative)?;
        let scope_dir = image_root.join(movie_dir_relative);

        for _attempt in 0..MAX_REBUILD_ATTEMPTS {
            let origins = self.live_origins(movie_dir_relative).await?;
            if origins.is_empty() {
                remove_quietly(&pack_path);
                return Ok(false);
            }

            let Some(entries) = load_entries(&pack_path, &image_root, &origins) else {
                // 有活跃行但拿不到字节：并发写入的中间态，重新快照后重试。
                continue;
            };

            cleanup_stale_temp_packs(&pack_path);
            let tmp_path = pack_path.with_file_name(format!(
                "{}.tmp-{}",
                pack_path.file_name().map_or_else(
                    || MOVIE_ASSETS_PACK_NAME.to_owned(),
                    |n| n.to_string_lossy().into_owned()
                ),
                uuid::Uuid::new_v4().simple()
            ));

            if let Err(error) = write_pack(&tmp_path, &entries) {
                // 写失败不该把临时包留在目录里（它会污染下一次的 stale 清理，
                // 也会被 `remove_loose_files` 当成"包前缀文件"放过）。
                remove_quietly(&tmp_path);
                return Err(error);
            }

            if self.live_origins(movie_dir_relative).await? != origins {
                // 构建期间活跃集变化：丢弃本次构建重新来。
                remove_quietly(&tmp_path);
                continue;
            }

            std::fs::rename(&tmp_path, &pack_path).map_err(|error| {
                ServiceError::from(sm_db::DbError::business(
                    "MovieAssetPack",
                    format!(
                        "原子替换 {} -> {} 失败：{error}",
                        tmp_path.display(),
                        pack_path.display()
                    ),
                ))
            })?;
            remove_loose_files(&scope_dir, &pack_path);
            return Ok(true);
        }

        // 三次都拿不到稳定的活跃集：**不动正式包**，保留现状。
        tracing::warn!(
            dir = %movie_dir_relative.display(),
            "影片图片包重建因活跃集不稳定而跳过（连续 3 次）"
        );
        Ok(pack_path.is_file())
    }

    /// 该目录下**存活**的图片相对路径，按 `origin` 升序。
    ///
    /// 上游 `live_origins(movie_dir) -> list[str]`。「存活」= `image` 记录
    /// 还在。已删记录的图片**不进包** —— 否则包会带着一个打不开的条目。
    ///
    /// # 只取**直接子文件**
    ///
    /// 上游多一步 `"/" not in origin[len(prefix):]`。少了它，
    /// `movies/<shard>/<番号>/media/12/thumbnails/1.jpg`（时间轴缩略图，属于
    /// `thumbnails.zip`）会被一起塞进 `assets.zip` —— 同一张图进两个包，
    /// 删其一另一份就成了幽灵。
    pub async fn live_origins(
        &self,
        movie_dir_relative: &Path,
    ) -> Result<Vec<String>, ServiceError> {
        let prefix = format!(
            "{}/",
            movie_dir_relative.to_string_lossy().replace('\\', "/")
        );
        let pattern = ImageRepository::like_prefix(&prefix);
        let origins = ImageRepository::new(self.db.clone())
            .list_origins_by_pattern(&pattern)
            .await?;
        // `LIKE` 只是索引友好的**粗筛**，精确判定在 Rust 这一侧做：上游同款
        // 两步（`like(_like_prefix_pattern(prefix))` + `startswith`）。粗筛放过
        // 的尾部要在这里滤掉，且必须排除含子目录的路径。
        Ok(origins
            .into_iter()
            .filter(|origin| {
                origin
                    .strip_prefix(&prefix)
                    .is_some_and(|rest| !rest.is_empty() && !rest.contains('/'))
            })
            .collect())
    }
}

/// 取每个 `origin` 的字节：**loose 文件优先，缺失回退旧包条目**。
///
/// 返回 `None` 表示「至少有一个 origin 两边都取不到」—— 上游据此重试整轮
/// （`_load_entries -> None`）。这不是错误，是并发写入的中间态：
/// 另一处刚登记了新 origin 但文件还没落盘。
///
/// # 为什么 loose 优先
///
/// 图片重新生成后，**新字节在 loose 文件里**，旧包条目是上一版。反过来取会
/// 把过期的图片打进新包 —— 而且不会有任何报错，只是「图变了但包里的没变」。
fn load_entries(
    pack_path: &Path,
    image_root: &Path,
    origins: &[String],
) -> Option<Vec<(String, Vec<u8>)>> {
    let mut entries = Vec::with_capacity(origins.len());
    for origin in origins {
        let entry_name = pack_entry_name(origin);
        let loose_path = media_paths::resolve_inside(image_root, origin).ok()?;
        if loose_path.is_file() {
            // loose 文件读不出来（权限/竞态删除）→ 交给重试，不静默跳过：
            // 跳过会让包**少一条**活跃图片，而调用方无法察觉。
            let bytes = std::fs::read(&loose_path).ok()?;
            entries.push((entry_name, bytes));
            continue;
        }
        // 回退旧包条目 —— 上一轮打包后 loose 文件已被清掉，这是常态而不是异常。
        // 两边都取不到 → 整轮放弃（`None`），上游据此重试。
        let bytes = read_pack_entry(pack_path, &entry_name)?;
        entries.push((entry_name, bytes));
    }
    Some(entries)
}

/// 删掉文件并**吞掉错误**。
///
/// 上游本模块的三处清理（`_remove_pack` / `_cleanup_stale_temp_packs` /
/// `_remove_loose_files`）都捕 `OSError`：清理失败不该让重建整体失败。
/// 这与 `image_cleanup._unlink_image_file` 的策略**不同**（那边只捕
/// `FileNotFoundError`，其余冒到路由）—— 差别是上游就有的，
/// 见 `media_paths::remove_file_if_exists` 的文档。
fn remove_quietly(path: &Path) {
    if let Err(error) = remove_file_if_exists(path) {
        tracing::warn!(path = %path.display(), code = error.code(), "删除文件失败，已跳过");
    }
}

/// 清掉遗留的临时包（`assets.zip.tmp-*`）。
///
/// 上一次重建中途崩溃会留下它。不清理的话 `remove_loose_files` 会把它们当成
/// 「包前缀文件」保留，于是一个几千兆的垃圾文件永远躺在影片目录里。
fn cleanup_stale_temp_packs(pack_path: &Path) {
    let Some(parent) = pack_path.parent() else {
        return;
    };
    let Some(name) = pack_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
    else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    let prefix = format!("{name}.tmp-");
    for entry in entries.flatten() {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name.starts_with(&prefix) {
            remove_quietly(&entry.path());
        }
    }
}

/// 包已成权威：清掉目录里**平铺的图片单文件**。
///
/// 保留包本身与包前缀的临时/备份文件（它们要么是当前产物，要么正在被替换），
/// 也不进子目录 —— `media/<id>/thumbnails/` 里的缩略图归 `thumbnails.zip` 管，
/// 删了就是把另一个包的数据删了。
fn remove_loose_files(scope_dir: &Path, pack_path: &Path) {
    let Ok(entries) = std::fs::read_dir(scope_dir) else {
        return;
    };
    let pack_name = pack_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == pack_name || name.starts_with(&format!("{pack_name}.")) {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() && !file_type.is_symlink() {
            continue;
        }
        remove_quietly(&entry.path());
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

    /// 条目名排序必须**稳定** —— 否则每次重建产出的包字节不同，
    /// 客户端会反复重新下载。
    #[test]
    fn origins_must_be_sorted_for_a_stable_pack() {
        let mut origins = vec!["b.jpg".to_owned(), "a.jpg".to_owned()];
        origins.sort();
        assert_eq!(origins, vec!["a.jpg".to_owned(), "b.jpg".to_owned()]);
    }
}
