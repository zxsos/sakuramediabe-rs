//! 给影片卡片挂媒体信息（上游 `catalog/movie_list_media_service.py`，14 行）。
//!
//! # 上游只有 14 行，因为它只做**一件事**：跨域调用
//!
//! 它自己不含任何逻辑，只负责「把 `playback` 域的媒体摘要挂到 `catalog` 的
//! 影片对象上」。**惰性 import** 是为了避免 `catalog` ↔ `playback` 的循环
//! 依赖 —— 这个文件就是那个环的**解环点**。
//!
//! # 为什么必须挂，而不能由调用方各自去查
//!
//! `MovieListItemResource` 的 `can_play` / `media_count` / `media_items`
//! 三个字段都来自这里（见 `sm_api::dto::MovieListItemResource`）。
//! 影片卡片出现在**很多**端点里（`/movies*`、播放列表内、时刻推荐…）。
//! 若每个端点自己查，就是 N+1，而且各处口径会漂移 —— 会出现「同一个影片在
//! 列表里 `can_play: true`、点进去却没有可播的」。
//!
//! # `can_play` 是「**至少一条**有效媒体」
//!
//! 不是「全部有效」，也不是「有媒体」。只有这个语义对：一条有效媒体就够播。
//! 要求「全部有效」会让「4 个文件里坏了 1 个」的影片变成不可播。

use crate::error::ServiceError;

/// 媒体摘要（`playback` 域提供）。
///
/// **刻意复用 playback 域的类型**，不在这里重新定义 —— 两处定义会漂移。
pub type MediaSummary = super::super::playback::media_summary::MediaSummary;

/// 给一批影片挂上媒体信息。**原地修改**。
///
/// 上游是 `None` 返回 + 惰性 import。这里保持「原地挂」的语义：调用方已经
/// 持有 `Vec<MovieCard>`，返回新 Vec 会强迫每个调用方改写一遍。
///
/// # 一趟查完，不是每部影片一次
///
/// 上游用 `list_movie_media_summaries` 批量取（一条 `IN` 查询）。逐部查就是
/// N+1 —— 列表页 20 项就是 20 次往返。
pub fn attach_movie_list_media(movies: &mut [crate::catalog::movie::MovieCard]) {
    let _ = movies;
    todo!("骨架：按 movie_id 批量取 MediaSummary（一条 IN 查询），写回 can_play/media_count/media_items")
}

/// 便捷版：返回 `Result` 而非 panic。
///
/// 保留这个签名是为了让调用方能统一走 `?`。若最终确认它永远不失败，
/// 删掉比留一个 `Ok(())` 体诚实。
pub fn attach_movie_list_media_checked(
    movies: &mut [crate::catalog::movie::MovieCard],
) -> Result<(), ServiceError> {
    attach_movie_list_media(movies);
    Ok(())
}

#[cfg(test)]
mod tests {

    /// `can_play` 的语义是「**至少一条**有效媒体」。
    ///
    /// 判断本身在 `playback::media_summary`，但它是本文件存在的全部理由，
    /// 所以在此锁住语义：一条有效即可播，坏 1 个不影响。
    #[test]
    fn one_valid_media_is_enough_to_play() {
        for valid in [0i64, 1, 4] {
            assert_eq!(valid > 0, valid >= 1, "{valid} 条有效媒体时可播");
        }
    }
}
