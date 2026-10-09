//! 批量为已有影片补算竖封面（上游 `catalog/movie_thin_cover_backfill_service.py`，39 行）。
//!
//! # 上游只有 39 行，因为它**不联网**
//!
//! 竖封面（thin cover）是从**已下载的剧情图**里切出来的（见
//! [`super::movie_image`] 里 `resolve_thin_cover_*` 那几个方法）。所以这个
//! 回填任务只读本地文件 + 查库。
//!
//! ⚠️ 别把它和 `movie_javdb_backfill` 混起来 —— 那个要出网，这个不出网。
//!
//! # 为什么需要「回填」而不是导入时就算
//!
//! 竖封面依赖剧情图，而剧情图是**导入时抓的**，可能失败或为空。等影片入库后
//! 再补一轮，能救回那些当时没抓到图的影片。
//!
//! # 「只补没有的」—— 已有的不覆盖
//!
//! 上游方法名就是 `backfill_missing_thin_cover_images`。覆盖已有的会让用户
//! 手动挑过的竖封面被算法结果替换掉。

use crate::error::ServiceError;

/// 一部缺竖封面的影片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingThinCover {
    pub movie_id: i64,
    pub movie_number: String,
}

/// 回填统计。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ThinCoverBackfillStats {
    /// 缺竖封面的影片数（考察数）。
    pub examined: i32,
    /// 成功补上的部数。
    pub backfilled: i32,
    /// 补不上的部数（没有剧情图、或切割失败）。
    pub skipped: i32,
    /// 切割失败的部数。**与 `skipped` 分开** —— 前者是数据问题，后者是
    /// 算法/依赖问题，排查方式不同。
    pub failed: i32,
}

/// 回填服务。
// `import_service` 尚未被方法体引用（回填动作还是 `todo!()`），落地后删 allow。
#[allow(dead_code)]
pub struct MovieThinCoverBackfillService {
    import_service: Option<Box<dyn ThinCoverBackfill>>,
}

/// 竖封面计算能力（读本地已落盘的图片）。
pub trait ThinCoverBackfill {
    /// 为该影片补算竖封面。`Ok(false)` = 算不出来（缺剧情图等）。
    fn backfill_movie_thin_cover(&self, movie_id: i64) -> Result<bool, ServiceError>;
}

impl MovieThinCoverBackfillService {
    /// 构造。
    pub fn new(import_service: Box<dyn ThinCoverBackfill>) -> Self {
        Self {
            import_service: Some(import_service),
        }
    }

    /// ★ 补算所有缺竖封面的影片。
    ///
    /// 上游 `backfill_missing_thin_cover_images() -> dict[str, int]`。
    /// **这个任务是 `manual_only`**（cron 表里 `movie_asset_pack_backfill`
    /// 那三条之一之外，它没有 cron）—— 跑一次可能很久。
    ///
    /// 单部失败不中断整批。
    pub async fn backfill_missing_thin_cover_images(
        &self,
    ) -> Result<ThinCoverBackfillStats, ServiceError> {
        todo!("骨架：查缺 thin_cover 的影片 -> 逐部 backfill_movie_thin_cover -> 记统计")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ `failed` 与 `skipped` **必须分开**。
    ///
    /// 「没有剧情图」是数据问题（等抓图就好），「切割失败」是算法或依赖问题
    /// （Pillow/cv2 异常）。合成一个数就看不出该修哪边。
    #[test]
    fn failure_and_skip_are_distinguished() {
        let stats = ThinCoverBackfillStats {
            examined: 5,
            backfilled: 2,
            skipped: 2,
            failed: 1,
        };
        assert_eq!(
            stats.examined,
            stats.backfilled + stats.skipped + stats.failed
        );
        assert_eq!(stats.failed, 1, "切割失败单独计数");
    }
}
