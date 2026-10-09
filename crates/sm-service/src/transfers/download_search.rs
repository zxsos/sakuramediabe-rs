//! 下载候选搜索，对应上游
//! `src/service/transfers/downloads/search_service.py`（98 行）。
//!
//! # 三步，顺序不能换
//!
//! 1. **番号**：trim → 空则 422 `invalid_download_candidate_movie_number`；
//!    否则**大写**。后面的标题比对与检索词都用这个值，所以大写必须在这里做完 ——
//!    晚做一步，`?movie_number=ssni-888` 与候选标题里的 `SSNI-888` 就会因大小写
//!    不等而被**全部剔除**，表现为「搜得到但一条都不返回」。
//! 2. **索引器类型**：trim + 小写；空串等价于「不筛选」而不是「类型非法」。
//!    不在 `pt` / `bt` 里则 422 `invalid_download_candidate_indexer_kind`，
//!    `details.indexer_kind` 回显**原始输入**（客户端据此高亮用户填的那个值）。
//! 3. **搜索**：`continue_on_error = true`。单个坏索引器被跳过，但**全部可搜索
//!    的索引器都失败**时仍是 502 `download_candidate_search_failed` —— 否则
//!    「索引器全挂了」会伪装成「这个番号没有资源」，而两者的处置完全不同。
//!
//! # 标题番号过滤是启发式，不是内容闸门
//!
//! 能解析出番号且与请求不一致的候选**剔除**（那种资源提交后也会被导入侧归到
//! 别的影片）；解析不出番号的**保留**，交给提交阶段的内容闸门做最终确认。
//! 比对口径与内容闸门一致：`strip` + 大写原串，**不折叠分隔符**
//! （[`crate::movie_numbers`] 两个函数的口径不同，见那里的模块文档）。

use sm_db::transfers::downloads::indexer_kind;
use sm_db::Db;

use crate::error::{details_of, ServiceError};
use crate::movie_numbers::parse_movie_number_from_text;
use crate::transfers::torznab::{TorznabCandidate, TorznabClient};

/// 番号为空。
const INVALID_MOVIE_NUMBER: &str = "invalid_download_candidate_movie_number";
/// 索引器类型不在白名单里。
const INVALID_INDEXER_KIND: &str = "invalid_download_candidate_indexer_kind";
/// 搜索整体失败。
const SEARCH_FAILED: &str = "download_candidate_search_failed";

/// 下载候选搜索 service。
#[derive(Debug, Clone)]
pub struct DownloadSearchService {
    pool: Db,
    torznab: TorznabClient,
}

impl DownloadSearchService {
    pub fn new(db: &Db) -> Self {
        Self::with_torznab(db, TorznabClient::new())
    }

    /// 注入自定义 Torznab 客户端。
    ///
    /// 测试用它指向一个假索引器 —— 这里没有别的注入点：URL 来自库里的
    /// indexer 行，而 `TorznabClient::new()` 只能发真请求。
    pub fn with_torznab(db: &Db, torznab: TorznabClient) -> Self {
        Self {
            pool: db.clone(),
            torznab,
        }
    }

    /// `GET /download-candidates`。
    pub async fn search_candidates(
        &self,
        movie_number: &str,
        indexer_kind: Option<&str>,
    ) -> Result<Vec<TorznabCandidate>, ServiceError> {
        let requested = validate_movie_number(movie_number)?;
        let kind = validate_indexer_kind(indexer_kind)?;

        let candidates = self
            .torznab
            .search(&self.pool, &requested, kind.as_deref(), true)
            .await
            .map_err(|err| search_failed(err.message()))?;

        Ok(filter_title_mismatched_candidates(candidates, &requested))
    }
}

/// 番号：trim，空则 422，否则大写。
fn validate_movie_number(value: &str) -> Result<String, ServiceError> {
    let normalized = value.trim();
    if normalized.is_empty() {
        return Err(ServiceError::validation(
            INVALID_MOVIE_NUMBER,
            "movie_number cannot be empty",
        ));
    }
    Ok(normalized.to_uppercase())
}

/// 索引器类型：`None` / 空白 = 不筛选；未知值 422。
fn validate_indexer_kind(value: Option<&str>) -> Result<Option<String>, ServiceError> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() {
        return Ok(None);
    }
    if !indexer_kind::is_valid(&normalized) {
        return Err(ServiceError::validation_with(
            INVALID_INDEXER_KIND,
            "Unsupported indexer kind",
            details_of("indexer_kind", raw),
        ));
    }
    Ok(Some(normalized))
}

/// 剔除标题里解析出**别的**番号的候选；解析不出的保留。
fn filter_title_mismatched_candidates(
    candidates: Vec<TorznabCandidate>,
    movie_number: &str,
) -> Vec<TorznabCandidate> {
    let requested = movie_number.trim().to_uppercase();
    let total = candidates.len();
    let mut dropped = 0usize;

    let filtered: Vec<TorznabCandidate> = candidates
        .into_iter()
        .filter(|candidate| {
            let parsed = parse_movie_number_from_text(&candidate.title);
            if !parsed.is_empty() && parsed.to_uppercase() != requested {
                dropped += 1;
                tracing::info!(
                    movie_number = requested.as_str(),
                    title = %candidate.title,
                    parsed = %parsed,
                    "候选标题里的番号与请求不一致，已剔除"
                );
                return false;
            }
            true
        })
        .collect();

    if dropped > 0 {
        tracing::info!(
            movie_number = requested.as_str(),
            total,
            dropped,
            "Torznab 标题过滤完成"
        );
    }
    filtered
}

/// 502 —— 外部索引器整体不可用，不是「没有资源」。
fn search_failed(detail: &str) -> ServiceError {
    ServiceError::bad_gateway(
        SEARCH_FAILED,
        "Torznab search failed",
        details_of("detail", detail),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(title: &str) -> TorznabCandidate {
        TorznabCandidate {
            source_uri: "magnet:?xt=urn:btih:AAA".to_owned(),
            indexer_name: "我的索引器".to_owned(),
            indexer_kind: "pt".to_owned(),
            resolved_client_id: 11,
            resolved_client_name: "qb".to_owned(),
            download_clients: Vec::new(),
            movie_number: "SSNI-888".to_owned(),
            title: title.to_owned(),
            size_bytes: 1024,
            seeders: 3,
        }
    }

    #[test]
    fn a_blank_movie_number_is_rejected_with_its_own_code() {
        for blank in ["", "   ", "\t\n"] {
            let err = validate_movie_number(blank).expect_err("空番号应被拒");
            assert_eq!((err.status, err.code()), (422, INVALID_MOVIE_NUMBER));
            assert_eq!(err.api.message, "movie_number cannot be empty");
        }
    }

    /// 大写化发生在 service 里，而不是调用方 —— 见模块文档第 1 条。
    #[test]
    fn the_movie_number_is_trimmed_and_uppercased() {
        assert_eq!(validate_movie_number("  ssni-888 ").unwrap(), "SSNI-888");
    }

    #[test]
    fn a_blank_kind_means_no_filter_rather_than_a_bad_kind() {
        assert_eq!(validate_indexer_kind(None).unwrap(), None);
        assert_eq!(validate_indexer_kind(Some("")).unwrap(), None);
        assert_eq!(validate_indexer_kind(Some("   ")).unwrap(), None);
    }

    #[test]
    fn the_kind_is_lowercased() {
        assert_eq!(
            validate_indexer_kind(Some(" PT ")).unwrap(),
            Some("pt".to_owned())
        );
        assert_eq!(
            validate_indexer_kind(Some("Bt")).unwrap(),
            Some("bt".to_owned())
        );
    }

    /// `details.indexer_kind` 回显**原始输入**（带空白），不是归一后的值。
    #[test]
    fn an_unknown_kind_echoes_the_raw_input_in_details() {
        let err = validate_indexer_kind(Some("  torznab  ")).expect_err("未知 kind 应被拒");
        assert_eq!((err.status, err.code()), (422, INVALID_INDEXER_KIND));
        assert_eq!(err.api.message, "Unsupported indexer kind");
        assert_eq!(
            err.api.details.as_ref().unwrap().get("indexer_kind"),
            Some(&serde_json::json!("  torznab  "))
        );
    }

    #[test]
    fn candidates_whose_title_names_another_number_are_dropped() {
        let kept = filter_title_mismatched_candidates(
            vec![
                candidate("SSNI-888 第一版"),
                candidate("ABC-123 完全不相干"),
                candidate("ssni-888 第二版"),
            ],
            "SSNI-888",
        );

        assert_eq!(
            kept.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(),
            vec!["SSNI-888 第一版", "ssni-888 第二版"],
            "只有解析出别的番号的那条该被剔除"
        );
    }

    #[test]
    fn candidates_without_a_parseable_number_survive() {
        // 启发式解析不出番号时保守保留 —— 最终由提交阶段的内容闸门确认。
        let kept = filter_title_mismatched_candidates(
            vec![candidate("完全无关的标题"), candidate("SSNI-888 版本")],
            "SSNI-888",
        );
        assert_eq!(kept.len(), 2);
    }

    /// 素人番号的分隔符是片商标识（`123456-789` 与 `123456_789` 是两部片子），
    /// 所以这里**不折叠**：分隔符不同就是不匹配。
    #[test]
    fn the_comparison_does_not_fold_separators() {
        assert_eq!(
            parse_movie_number_from_text("123456-789 资源"),
            "123456-789"
        );
        assert_eq!(
            parse_movie_number_from_text("123456_789 资源"),
            "123456_789"
        );

        let kept =
            filter_title_mismatched_candidates(vec![candidate("123456_789 资源")], "123456-789");
        assert!(kept.is_empty(), "分隔符不同不该被当成同一个番号");
    }

    #[test]
    fn the_search_failure_is_a_502_with_the_detail() {
        let err = search_failed("HTTP 500");
        assert_eq!((err.status, err.code()), (502, SEARCH_FAILED));
        assert_eq!(err.api.message, "Torznab search failed");
        assert_eq!(
            err.api.details.as_ref().unwrap().get("detail"),
            Some(&serde_json::json!("HTTP 500"))
        );
    }
}
