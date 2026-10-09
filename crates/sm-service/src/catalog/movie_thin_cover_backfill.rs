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

use sm_db::repo::MovieRepository;
use sm_db::Db;

use crate::error::ServiceError;

/// 一部缺竖封面的影片。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingThinCover {
    pub movie_id: i32,
    /// 只在日志里用 —— 上游失败时打 `movie_id={} movie_number={}`。
    pub movie_number: String,
}

/// 回填统计。**四个键与上游 dict 逐字一致**
/// （`movie_thin_cover_backfill_service.py:14-19`）。
///
/// ⚠️ 骨架期是自造的 `examined` / `backfilled` / `skipped` / `failed`。
/// 名字不同之外还有一处语义：`skipped` 是「**算不出来**」（`Ok(false)`，
/// 比如没有剧情图），`failed` 是「**抛了异常**」（切割/依赖出错）。
/// 两者都要有 —— 合成一个数就看不出该修哪边。
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
pub struct ThinCoverBackfillStats {
    /// 扫到的缺竖封面影片数。
    pub scanned_movies: i64,
    /// 成功补上的部数。
    pub updated_movies: i64,
    /// 算不出来的部数（没有剧情图等）。
    pub skipped_movies: i64,
    /// 抛异常的部数。**与 `skipped` 分开** —— 前者是数据问题，后者是
    /// 算法/依赖问题，排查方式不同。
    pub failed_movies: i64,
}

/// 回填服务。
///
/// ⚠️ [`ThinCoverBackfill`] **还没有宿主实现**：切竖封面要读已落盘的剧情图做
/// 切割（[`super::movie_image`] 那几个 `resolve_thin_cover_*`），而本仓还没有
/// image store 模块。所以 `movie_thin_cover_backfill` 的 worker handler
/// **仍未注册**。接线时只需实现这个 trait 与一行注册。
pub struct MovieThinCoverBackfillService {
    db: Db,
    import_service: Box<dyn ThinCoverBackfill>,
}

/// 竖封面计算能力（读本地已落盘的图片）。
pub trait ThinCoverBackfill {
    /// 为该影片补算竖封面。`Ok(false)` = 算不出来（缺剧情图等）。
    ///
    /// 上游传的是 `Movie` 对象（`backfill_movie_thin_cover(movie)`），这里只传
    /// `movie_id`：注入方若还要番号，自己按 id 查一次即可 ——
    /// 传整行会把「实现方到底读了哪些字段」藏起来。
    fn backfill_movie_thin_cover(&self, movie_id: i32) -> Result<bool, ServiceError>;
}

impl MovieThinCoverBackfillService {
    /// 构造。
    pub fn new(db: &Db, import_service: Box<dyn ThinCoverBackfill>) -> Self {
        Self {
            db: db.clone(),
            import_service,
        }
    }

    /// ★ 补算所有缺竖封面的影片。
    ///
    /// 上游 `backfill_missing_thin_cover_images() -> dict[str, int]`。
    /// **这个任务是 `manual_only`**（cron 表里 `movie_asset_pack_backfill`
    /// 那三条之一之外，它没有 cron）—— 跑一次可能很久。
    ///
    /// # 单部失败不中断整批
    ///
    /// 一部片的剧情图坏了不该让其余都停。异常计进 `failed_movies` 并继续
    /// （上游 `except Exception ... continue`）。
    ///
    /// # 不联网、不上报进度
    ///
    /// 上游这个方法**没有** `reporter` 参数（与 `movie_interaction_sync` 不同），
    /// 所以这里也不收 progress —— 别为了「看起来一致」加一个。
    pub async fn backfill_missing_thin_cover_images(
        &self,
    ) -> Result<ThinCoverBackfillStats, ServiceError> {
        let missing = MovieRepository::new(self.db.clone())
            .list_missing_thin_cover()
            .await?
            .into_iter()
            .map(|(movie_id, movie_number)| MissingThinCover {
                movie_id,
                movie_number,
            })
            .collect::<Vec<_>>();

        let mut stats = ThinCoverBackfillStats {
            scanned_movies: i64::try_from(missing.len()).unwrap_or(i64::MAX),
            ..Default::default()
        };
        for movie in missing {
            match self
                .import_service
                .backfill_movie_thin_cover(movie.movie_id)
            {
                Ok(true) => stats.updated_movies += 1,
                Ok(false) => stats.skipped_movies += 1,
                Err(error) => {
                    stats.failed_movies += 1;
                    tracing::error!(
                        movie_id = movie.movie_id,
                        movie_number = movie.movie_number.as_str(),
                        code = error.code(),
                        "竖封面回填失败"
                    );
                }
            }
        }
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ `failed_movies` 与 `skipped_movies` **必须分开**。
    ///
    /// 「没有剧情图」是数据问题（等抓图就好），「切割失败」是算法或依赖问题
    /// （Pillow/cv2 异常）。合成一个数就看不出该修哪边。
    #[test]
    fn failure_and_skip_are_distinguished() {
        let stats = ThinCoverBackfillStats {
            scanned_movies: 5,
            updated_movies: 2,
            skipped_movies: 2,
            failed_movies: 1,
        };
        assert_eq!(
            stats.scanned_movies,
            stats.updated_movies + stats.skipped_movies + stats.failed_movies
        );
        assert_eq!(stats.failed_movies, 1, "切割失败单独计数");
    }

    /// 四个键与上游 dict 逐字一致（骨架期是自造的 `examined` / `backfilled` …）。
    #[test]
    fn the_stats_keys_match_upstreams_dict() {
        let value = serde_json::to_value(ThinCoverBackfillStats::default()).expect("可序列化");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "failed_movies",
                "scanned_movies",
                "skipped_movies",
                "updated_movies"
            ]
        );
    }
}
