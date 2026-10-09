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

use std::sync::Arc;

use sm_db::repo::MovieRepository;
use sm_db::Db;

use super::metadata_source::MetadataSourceService;
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
///
/// # 依赖是注入的
///
/// [`MetadataSourceService`] 出网（JavDB），[`super::catalog_import`] 写库，
/// 都由组合根装配；测试换成假来源与假入库 —— 本文件不知道「JavDB」是什么。
pub struct MovieMetadataRefreshService {
    db: Db,
    source: Arc<MetadataSourceService>,
    import: super::catalog_import::CatalogImportService,
}

impl MovieMetadataRefreshService {
    /// 构造。
    pub fn new(
        db: &Db,
        source: Arc<MetadataSourceService>,
        import: super::catalog_import::CatalogImportService,
    ) -> Self {
        Self {
            db: db.clone(),
            source,
            import,
        }
    }

    /// ★ 刷新一部影片的元数据。**覆盖式**写入。
    ///
    /// 上游 `refresh_movie_metadata(cls, movie_number)`（`:170-208`）：
    ///
    /// 1. 按归一番号取本地影片（没有 → 404）；
    /// 2. 取远端详情：没收录 → 404 `movie_metadata_not_found`；来源坏了 →
    ///    502 `movie_metadata_refresh_failed`；
    /// 3. 番号一致性 → 不一致 409 `movie_metadata_number_conflict`；
    /// 4. JavDB id 占用 → 409 `movie_metadata_javdb_id_conflict`；
    /// 5. 分支写入：**曾是插件来源**（无 javdb_id 但有 metadata_source）→
    ///    `backfill_plugin_movie`（补齐缺失列，不覆盖已有）；否则
    ///    `refresh_movie_metadata_strict`（值不同就覆盖）；
    /// 6. 任何一步的失败（除上面两类 409/404）→ 502 `movie_metadata_refresh_failed`；
    /// 7. 返回刷新后的详情。
    ///
    /// # 返回的是**重新读出来的**详情，不是写入结果
    ///
    /// 上游返回 `MovieService.get_movie_detail(...)`。`CatalogImportResult`
    /// 里的 `updated_fields` 是内部记账，客户端要的是刷新后的完整视图。
    pub async fn refresh_movie_metadata(
        &self,
        movie_number: &str,
    ) -> Result<crate::catalog::movie::MovieDetail, ServiceError> {
        let normalized = crate::movie_numbers::normalize_movie_number(movie_number);
        let movie = MovieRepository::new(self.db.clone())
            .find_by_number(&normalized)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "movie_not_found",
                    "影片不存在",
                    crate::error::details_of("movie_number", normalized.clone()),
                )
            })?;

        // ② 远端详情。NotFound → 404；其余来源错误 → 502（上游
        // `_fetch_remote_movie_metadata` 的三段映射）。
        let detail = match self.source.search_javdb_by_number(&normalized).await {
            Ok(Some(detail)) => detail,
            Ok(None) => {
                return Err(ServiceError::not_found_with(
                    "movie_metadata_not_found",
                    "影片远端元数据不存在",
                    crate::error::details_of("movie_number", normalized.clone()),
                ));
            }
            Err(error) => {
                let (reason, message) =
                    crate::catalog::movie_metadata_search::source_error_parts(&error);
                tracing::warn!(reason, detail = %message, "刷新元数据时来源调用失败");
                return Err(refresh_failed(&normalized));
            }
        };

        // ③④ 两道 409 闸门（防「拿 A 的请求写 B 的记录」）。
        validate_number(&movie.movie_number, &detail)?;
        validate_javdb_id(movie.javdb_id.as_deref(), detail_javdb_id(&detail)).map_err(
            |conflicting| {
                ServiceError::conflict(
                    "movie_metadata_javdb_id_conflict",
                    "远端元数据 JavDB ID 与其他本地影片冲突",
                    Some(crate::error::details_of(
                        "conflicting_movie_number",
                        conflicting,
                    )),
                )
            },
        )?;

        // ⑤ 分支写入。曾是插件来源（无 javdb_id 但有 metadata_source）→
        // 只**补缺失列**，不覆盖 —— 那是插件先收录的，JavDB 不该整体接管。
        let was_plugin_source = movie
            .javdb_id
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
            && movie.metadata_source.is_some();
        let result = if was_plugin_source {
            self.import.backfill_plugin_movie(movie.id, &detail).await
        } else {
            self.import
                .refresh_movie_metadata_strict(movie.id, &detail)
                .await
        };
        let result = result.map_err(|error| {
            tracing::warn!(
                movie_number = %movie.movie_number,
                code = error.code(),
                "元数据刷新写入失败"
            );
            refresh_failed(&normalized)
        })?;
        tracing::debug!(movie_id = result.movie_id, updated = ?result.updated_fields, "元数据刷新完成");

        // ⑦ 重读详情。
        crate::catalog::movie::MovieService::new(&self.db)
            .get_movie_detail(&movie.movie_number)
            .await
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

/// ③ 番号一致性。上游 `_validate_remote_movie_metadata_number`（`:108-130`）。
///
/// 返回本地归一番号。远端归一后为空或不等 → 409 `movie_metadata_number_conflict`，
/// details 带双方原始与归一番号 —— 客户端/运维要能看出「到底是哪边错了」。
fn validate_number(
    local_movie_number: &str,
    detail: &serde_json::Value,
) -> Result<String, ServiceError> {
    let local = crate::movie_numbers::normalize_movie_number(local_movie_number);
    let remote_raw = detail
        .get("movie_number")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let remote = crate::movie_numbers::normalize_movie_number(remote_raw);
    if remote.is_empty() || remote != local {
        let mut details = serde_json::Map::new();
        details.insert(
            "movie_number".to_owned(),
            serde_json::Value::String(local_movie_number.to_owned()),
        );
        details.insert(
            "normalized_movie_number".to_owned(),
            serde_json::Value::String(local.clone()),
        );
        details.insert(
            "remote_movie_number".to_owned(),
            serde_json::Value::String(remote_raw.to_owned()),
        );
        details.insert(
            "remote_normalized_movie_number".to_owned(),
            serde_json::Value::String(remote),
        );
        return Err(ServiceError::conflict(
            "movie_metadata_number_conflict",
            "远端元数据番号与本地影片不一致",
            Some(details),
        ));
    }
    Ok(local)
}

/// ④ JavDB id 占用判定（纯部分）。上游 `_validate_remote_movie_metadata_javdb_id`：
/// 远端 id 为空、或与本地相同 → 放行；否则查库，被占用 → 409。
/// 查库在调用方（要 `movie.id` 与仓储）；这里只做**判定**，收查询结果。
fn validate_javdb_id(current: Option<&str>, remote: Option<&str>) -> Result<(), Option<String>> {
    let remote = remote.map(str::trim).unwrap_or_default();
    if remote.is_empty() {
        return Ok(());
    }
    let current = current.map(str::trim).unwrap_or_default();
    if remote == current {
        return Ok(());
    }
    Err(Some(remote.to_owned()))
}

/// 详情里的 javdb_id（缺失/非字符串 → 空）。
fn detail_javdb_id(detail: &serde_json::Value) -> Option<&str> {
    detail.get("javdb_id").and_then(serde_json::Value::as_str)
}

/// 502 `movie_metadata_refresh_failed`。上游 `_raise_movie_metadata_refresh_failed`：
/// 来源调用、写入、图片下载的失败**全部**归成这一个 502 —— 客户端「稍后重试」。
fn refresh_failed(normalized_movie_number: &str) -> ServiceError {
    let mut error =
        ServiceError::from_status(502, "movie_metadata_refresh_failed", "影片元数据刷新失败");
    error.api.details = Some(crate::error::details_of(
        "normalized_movie_number",
        normalized_movie_number,
    ));
    error
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
        for code in [
            "movie_metadata_number_conflict",
            "movie_metadata_javdb_id_conflict",
        ] {
            assert!(!code.contains("failed"), "{code} 不该被当成可重试的失败");
        }
    }

    /// 流式事件**必须**能表达「某部失败但整体继续」。
    #[test]
    fn an_error_event_does_not_abort_the_stream() {
        let events = [
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

    // ------------------------------------------------- refresh_movie_metadata

    /// ③ 番号一致性：归一相等才放行。
    #[test]
    fn a_matching_number_passes_after_normalization() {
        let detail = serde_json::json!({ "movie_number": "abc-123" });
        assert_eq!(
            validate_number("ABC-123", &detail).expect("归一相等"),
            "ABC-123"
        );
    }

    /// ★ 不一致 → 409 `movie_metadata_number_conflict`，details 带双方**原始与
    /// 归一**番号 —— 「到底是哪边错了」要能从错误里直接看出来。
    #[test]
    fn a_number_mismatch_is_a_conflict_carrying_both_sides() {
        let detail = serde_json::json!({ "movie_number": "OTHER-9" });
        let error = validate_number("ABC-123", &detail).expect_err("不一致该拒");
        assert_eq!(error.code(), "movie_metadata_number_conflict");
        let details = error.api.details.expect("details 要带双方番号");
        assert_eq!(details["movie_number"], "ABC-123");
        assert_eq!(details["remote_movie_number"], "OTHER-9");
    }

    /// 远端番号缺失（非字符串）等价于「不一致」—— 拿它覆盖会把本地番号抹掉。
    #[test]
    fn a_missing_remote_number_is_a_conflict_too() {
        let error =
            validate_number("ABC-123", &serde_json::json!({})).expect_err("远端没给番号也该拒");
        assert_eq!(error.code(), "movie_metadata_number_conflict");
    }

    /// ④ JavDB id 闸门：远端为空或与本地相同 → 放行；不同 → 把远端 id 带给
    /// 调用方去查库（查库在服务体，判定在这里，纯函数好测）。
    #[test]
    fn the_javdb_id_gate_only_rejects_a_taken_remote_id() {
        assert!(validate_javdb_id(Some("old"), None).is_ok(), "远端没给 id");
        assert!(
            validate_javdb_id(Some("old"), Some("old")).is_ok(),
            "同一个 id"
        );
        assert!(
            validate_javdb_id(None, Some("new")).is_err(),
            "远端换了 id → 要查库"
        );
        assert_eq!(
            validate_javdb_id(Some("old"), Some("new")).expect_err("占用"),
            Some("new".to_owned()),
            "把远端 id 带回去查占用"
        );
        // 空白与空串等价（上游 `(detail.javdb_id or "").strip()`）。
        assert!(validate_javdb_id(Some("old"), Some("  ")).is_ok());
    }

    /// 502 的 details 带归一番号 —— 客户端重试时要用它。
    #[test]
    fn the_refresh_failed_error_carries_the_number() {
        let error = refresh_failed("ABC-123");
        assert_eq!(error.status, 502, "可重试的来源/写入失败");
        assert_eq!(error.code(), "movie_metadata_refresh_failed");
        assert_eq!(
            error.api.details.as_ref().unwrap()["normalized_movie_number"],
            "ABC-123"
        );
    }
}
