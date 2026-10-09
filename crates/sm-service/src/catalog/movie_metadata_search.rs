//! 只搜索元数据，**不建记录**（上游 `catalog/movie_metadata_search_service.py`，307 行）。
//!
//! # 与 [`super::catalog_import`] 的区别是本文件的核心
//!
//! | | 本文件 | `catalog_import` |
//! |---|---|---|
//! | 建 `movie` / `image` 记录 | **否** | 是 |
//! | 落盘图片 | 否（只缓存封面供预览） | 是 |
//! | 用途 | 「这个番号在 JavDB 有吗？长什么样？」 | 正式入库 |
//!
//! 端点是导入流程的**第一步**：用户先搜、看到候选、挑一个、再入库。
//! 若这一步就建了记录，用户一改主意就会留下垃圾影片。
//!
//! # 候选封面缓存 24 小时
//!
//! [`SEARCH_ASSET_MAX_AGE_SECONDS`]。缓存目录 `metadata-search/<uuid>/`。
//!
//! ⚠️ 缓存**必须有 TTL**，否则临时目录会无限增长（每次搜索都建一个 uuid 目录）。

use crate::error::ServiceError;

/// 候选资产缓存目录名。
pub const SEARCH_ASSET_DIR: &str = "metadata-search";
/// 缓存过期（24 小时）。
pub const SEARCH_ASSET_MAX_AGE_SECONDS: i64 = 24 * 60 * 60;

/// 预览用的图片扩展名白名单。
pub const IMAGE_EXTENSIONS: [&str; 6] = [".jpg", ".jpeg", ".png", ".webp", ".gif", ".avif"];

/// 一条搜索候选。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetadataCandidate {
    /// 候选 id。**回传给重试端点**的 `candidate_id`。
    pub candidate_id: String,
    pub title: String,
    pub date: Option<String>,
    /// 预览图 URL（**指向缓存**，不是 JavDB 原址 —— 那是带鉴权的）。
    pub preview_url: Option<String>,
    /// 置信度 [0, 1]。**降序**排列。
    pub confidence: f32,
    /// 来源（`javdb` / `plugin:xxx`）。
    pub source: String,
}

/// 元数据搜索服务。
pub struct MovieMetadataSearchService;

impl MovieMetadataSearchService {
    /// ★ 按番号搜候选。**不建任何记录**（见模块文档）。
    ///
    /// 上游 `search_by_number(cls, movie_number) -> ImportMetadataSearchResponse`。
    ///
    /// 错误码：番号为空 → `422 invalid_movie_number`；两个来源都没收录 →
    /// `NotFound`（由调用方映射成 404）。
    pub async fn search_by_number(
        movie_number: &str,
    ) -> Result<MetadataSearchResponse, ServiceError> {
        let _ = movie_number;
        todo!("骨架：查 JavDB + 已启用插件来源 -> 各下载封面到 24h 缓存 -> 按置信度降序")
    }

    /// 把 `candidate_id` 解成来源引用。上游 `resolve_candidate_reference(candidate_id) -> dict[str, str]`。
    ///
    /// 错误码：id 格式不对 → `422 invalid_metadata_candidate`。
    pub fn resolve_candidate_reference(
        candidate_id: &str,
    ) -> Result<serde_json::Value, ServiceError> {
        let _ = candidate_id;
        // ⚠️ 消息里**不能出现 `{...}`** —— `todo!` 的第一参数是格式串，
        // 里面的花括号会被当占位符（`{source, external_id}` 不是合法占位符）。
        // 上游我写成「解出 source 与 external_id」正是为了避开它。
        todo!("骨架：解出 source 与 external_id；格式不对 -> 422 invalid_metadata_candidate")
    }

    /// ★ 取候选详情，**闭包内有效**。
    ///
    /// 上游 `fetch_candidate(cls, candidate_id)` 是 contextmanager，yield
    /// `(detail, source, provider, None)`。
    ///
    /// 错误码：id 指向的来源与详情**不匹配** → `422 metadata_candidate_mismatch`。
    /// 那条检查防的是「id 被篡改成另一个来源的 id」。
    pub async fn fetch_candidate<R>(
        &self,
        candidate_id: &str,
        consume: impl AsyncFnOnce(serde_json::Value, serde_json::Value) -> R,
    ) -> Result<R, ServiceError> {
        let _ = (candidate_id, consume);
        todo!("骨架：解析 id -> 取详情 -> 校验来源匹配(422) -> use(detail, source).await")
    }

    /// 清理过期缓存目录，返回清掉多少个。上游 `cleanup_search_assets() -> int`。
    ///
    /// **按 mtime 判过期**，不是按目录名里的 uuid（那不含时间）。
    pub fn cleanup_search_assets() -> Result<u64, ServiceError> {
        todo!("骨架：扫 metadata-search/ 下 mtime 超过 24h 的目录并删除")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 缓存 TTL 是 24 小时，且目录名固定。
    #[test]
    fn the_cache_ttl_is_one_day() {
        assert_eq!(SEARCH_ASSET_MAX_AGE_SECONDS, 24 * 60 * 60);
        assert_eq!(SEARCH_ASSET_DIR, "metadata-search");
    }

    /// 预览图白名单**只收图片**。
    #[test]
    fn only_image_extensions_are_served_as_previews() {
        assert!(IMAGE_EXTENSIONS.contains(&".webp"));
        assert!(!IMAGE_EXTENSIONS.contains(&".mkv"));
        assert!(!IMAGE_EXTENSIONS.contains(&".srt"));
    }

    /// 候选列表按 `confidence` **降序** —— 客户端取第一个作默认选中项。
    #[test]
    fn candidates_are_ordered_by_confidence_descending() {
        let mut candidates = [
            MetadataCandidate {
                candidate_id: "a".to_owned(),
                title: "A".to_owned(),
                date: None,
                preview_url: None,
                confidence: 0.3,
                source: "javdb".to_owned(),
            },
            MetadataCandidate {
                candidate_id: "b".to_owned(),
                title: "B".to_owned(),
                date: None,
                preview_url: None,
                confidence: 0.9,
                source: "javdb".to_owned(),
            },
        ];
        candidates.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).expect("无 NaN"));
        assert_eq!(candidates[0].candidate_id, "b", "高置信度在前");
    }

    /// 每个候选**必须带** `candidate_id` —— 它是重试端点的唯一入参。
    #[test]
    fn every_candidate_carries_an_id() {
        let candidate = MetadataCandidate {
            candidate_id: "javdb:12345".to_owned(),
            title: "t".to_owned(),
            date: None,
            preview_url: None,
            confidence: 1.0,
            source: "javdb".to_owned(),
        };
        assert!(!candidate.candidate_id.is_empty());
    }
}

/// 搜索响应。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MetadataSearchResponse {
    pub movie_number: String,
    /// 按 `confidence` **降序**。客户端取第一个作默认选中项。
    pub candidates: Vec<MetadataCandidate>,
}
