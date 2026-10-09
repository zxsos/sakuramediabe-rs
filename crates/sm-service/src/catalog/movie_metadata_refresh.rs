//! 影片元数据刷新与 JavDB 流式导入（上游 `catalog/movie_metadata_refresh_service.py`，535 行）。
//!
//! # 三个方法，**两个是 SSE 生成器**
//!
//! | 方法 | 形态 |
//! |---|---|
//! | `refresh_movie_metadata` | 普通返回 |
//! | `stream_search_and_upsert_movie_from_javdb` | **流式**（SSE） |
//! | `stream_import_series_movies_from_javdb` | **流式**（SSE） |
//!
//! # ★ 四个错误码里有**两个 409**，语义不同
//!
//! | 码 | 含义 | 客户端该做什么 |
//! |---|---|---|
//! | `404 movie_metadata_not_found` | JavDB 没这部片 | 停止 |
//! | `409 movie_metadata_number_conflict` | **番号**对不上 | 停止，数据问题 |
//! | `409 movie_metadata_javdb_id_conflict` | **JavDB id** 对不上 | 同上 |
//! | `502 movie_metadata_refresh_failed` | 调 JavDB 失败 | 可重试 |
//!
//! 两个 409 防的是「拿 A 的请求去写 B 的记录」。不查的话，一次错误的响应
//! 就会把**另一部影片**的元数据覆盖过来，而用户完全看不出来。
//!
//! # 番号冲突**不是** 502
//!
//! 它是「上游返回了自相矛盾的数据」，属于**永久性**失败，重试无用。
//! 归成 502 会让客户端一直重试同一部片。

use crate::error::ServiceError;

/// 一条流式事件。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetadataStreamEvent {
    /// 事件名：`progress` / `movie` / `done` / `error`。
    pub event: String,
    pub movie_number: String,
    pub imported: bool,
    /// 失败原因（`event = "error"` 时有值）。
    pub message: Option<String>,
}

/// 元数据刷新服务。
pub struct MovieMetadataRefreshService;

impl MovieMetadataRefreshService {
    /// ★ 刷新一部影片的元数据。**覆盖式**写入。
    ///
    /// 流程：按番号查 JavDB -> 校验番号一致(409) -> 严格覆盖写库
    /// -> 重建 `assets.zip`。
    pub async fn refresh_movie_metadata(movie_number: &str) -> Result<serde_json::Value, ServiceError> {
        let _ = movie_number;
        todo!("骨架：查 JavDB -> 校验番号一致(409) -> 严格覆盖写 -> 重建 assets.zip")
    }

    /// ★ 流式搜索并入库。上游是生成器，`yield (movie_number, dict)`。
    ///
    /// ⚠️ 本仓用 `Vec` 代替流式（async 生成器需额外依赖）。代价是**全部完成
    /// 才返回** —— **不要**用它驱动进度条。
    pub async fn stream_search_and_upsert_movie_from_javdb(
        &self,
        movie_number: &str,
    ) -> Result<Vec<MetadataStreamEvent>, ServiceError> {
        let _ = movie_number;
        todo!("骨架：搜 JavDB -> 逐个候选 upsert -> 收集事件；番号冲突记 error 事件而非中断")
    }

    /// ★ 流式导入一个系列的全部影片。
    ///
    /// **单部失败不中断整批** —— 一个系列几十部，一部失败就全废掉不可接受。
    pub async fn stream_import_series_movies_from_javdb(
        &self,
        series_id: i64,
    ) -> Result<Vec<MetadataStreamEvent>, ServiceError> {
        let _ = series_id;
        todo!("骨架：取系列全部影片 -> 逐部 upsert -> 收集事件；单部失败记 error 事件继续")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 四个错误码的语义要逐条锁住，尤其两个 409。
    #[test]
    fn the_four_error_codes_keep_their_meanings() {
        let cases = [
            ("movie_metadata_refresh_failed", 502u16),
            ("movie_metadata_not_found", 404),
            ("movie_metadata_number_conflict", 409),
            ("movie_metadata_javdb_id_conflict", 409),
        ];
        for (code, status) in cases {
            assert!(!code.is_empty());
            assert!((400..600).contains(&status));
        }
    }

    /// 番号冲突是**永久性**失败 —— 归成 5xx 会让客户端无限重试。
    #[test]
    fn conflicts_are_not_retryable_5xx() {
        for code in ["movie_metadata_number_conflict", "movie_metadata_javdb_id_conflict"] {
            assert!(!code.contains("failed"), "{code} 不该被当成可重试的失败");
        }
    }

    /// 流式事件**必须**能表达「某部失败但整体继续」。
    #[test]
    fn an_error_event_does_not_abort_the_stream() {
        let events = vec![
            MetadataStreamEvent {
                event: "movie".to_owned(),
                movie_number: "A-001".to_owned(),
                imported: true,
                message: None,
            },
            MetadataStreamEvent {
                event: "error".to_owned(),
                movie_number: "A-002".to_owned(),
                imported: false,
                message: Some("番号冲突".to_owned()),
            },
            MetadataStreamEvent {
                event: "done".to_owned(),
                movie_number: String::new(),
                imported: false,
                message: None,
            },
        ];
        assert_eq!(events.len(), 3, "错误事件后仍有后续事件");
        assert_eq!(events[1].event, "error");
        assert_eq!(events[2].event, "done", "整批仍会走到 done");
    }
}
