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

/// 一条候选 / 搜索响应 / 来源失败 —— **直接复用**重试端点那一份。
///
/// ⚠️ 骨架期本文件自己声明了 `MetadataCandidate { candidate_id, title, date,
/// preview_url, confidence, source }`，与 `transfers::import_task` 里那份
/// **同名不同形**（同 crate 两份 wire 形状）。上游只有一份：
/// `ImportMetadataCandidateResource`（`media_import.py:76-86`）——
/// **没有 `confidence`**（顺序由 provider 保证）、**没有 `date`**（是
/// `release_date`）、**没有 `preview_url`**（是 `cover_url`）。骨架多出来的
/// 三个字段客户端拿不到，缺的 `source_name` / `duration_minutes` 却要渲染候选卡片。
///
/// 定义留在这两处**唯一**的那份（`import_task`，随 retry 端点一起演进），
/// 这里只做转出口，避免 catalog 的调用方被迫 `use crate::transfers::…`。
pub use crate::transfers::import_task::{
    ImportMetadataSearchResponse, ImportMetadataSourceErrorResource, MetadataCandidate,
    MetadataCandidateSource,
};

/// 一个候选 id 解出来的**来源引用**。上游 `resolve_candidate_reference` 返回的 dict。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateReference {
    pub source: MetadataCandidateSource,
    /// 归一后的番号。
    pub movie_number: String,
    /// `source = javdb` 时的 JavDB id。
    pub javdb_id: Option<String>,
    /// `source = plugin` 时的插件 id。
    pub plugin_id: Option<String>,
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
    ) -> Result<ImportMetadataSearchResponse, ServiceError> {
        let _ = movie_number;
        todo!("骨架：查 JavDB + 已启用插件来源 -> 各下载封面到 24h 缓存 -> 按来源顺序返回")
    }

    /// 把 `candidate_id` 解成来源引用。上游 `resolve_candidate_reference(candidate_id) -> dict[str, str]`。
    ///
    /// 错误码：id 格式不对 → `422 invalid_metadata_candidate`。
    /// # `plugin_id` 是否启用要由调用方给
    ///
    /// 上游在这里查 `MetadataSourceService.is_plugin_enabled(plugin_id)`
    /// （`:250-256`）：插件可能**已经被卸载或停用**，而它的候选 id 还在客户端
    /// 手里（搜索结果缓存过、用户几个小时后再点重试）。不查就会放行一个
    /// **必然失败**的重试任务。
    ///
    /// 本仓把它做成参数（`plugin_enabled`）而不是在本模块里 import 那个注册表：
    /// 这样这条解码规则可以脱离插件栈单独测，也不给 catalog 引入对插件运行时
    /// 的依赖。
    pub fn resolve_candidate_reference(
        candidate_id: &str,
        plugin_enabled: impl Fn(&str) -> bool,
    ) -> Result<CandidateReference, ServiceError> {
        let parts: Vec<&str> = candidate_id.trim().split(':').collect();
        // ★ 三段**且**前缀与内容都非空。`len == 3` 不够 —— `javdb::x` 这种
        // 也能凑出三段，放过去会让下面拿一个空番号去查。
        if parts.len() == 3 {
            let (prefix, second, third) = (parts[0], parts[1], parts[2]);
            if prefix == "javdb" && !second.is_empty() && !third.is_empty() {
                return Ok(CandidateReference {
                    source: MetadataCandidateSource::Javdb,
                    movie_number: crate::movie_numbers::normalize_movie_number(second),
                    javdb_id: Some(third.to_owned()),
                    plugin_id: None,
                });
            }
            if prefix == "plugin"
                && !second.is_empty()
                && !third.is_empty()
                && plugin_enabled(second)
            {
                return Ok(CandidateReference {
                    source: MetadataCandidateSource::Plugin,
                    movie_number: crate::movie_numbers::normalize_movie_number(third),
                    javdb_id: None,
                    plugin_id: Some(second.to_owned()),
                });
            }
        }
        Err(invalid_candidate())
    }

    /// 候选 id 的编码（与 [`Self::resolve_candidate_reference`] 是一对）。
    ///
    /// 用 `:` 分隔三段。★ 插件 id / 番号里**不能有 `:`**，否则解出来会串段 ——
    /// 番号侧由归一函数保证（它只产出字母数字与连字符），插件 id 侧由注册规则
    /// 保证（`[a-z0-9_-]`）。
    pub fn javdb_candidate_id(movie_number: &str, javdb_id: &str) -> String {
        format!("javdb:{movie_number}:{javdb_id}")
    }

    /// 见 [`Self::javdb_candidate_id`]。
    pub fn plugin_candidate_id(plugin_id: &str, movie_number: &str) -> String {
        format!("plugin:{plugin_id}:{movie_number}")
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
    /// # 只删**目录**，且**不跟随符号链接**
    ///
    /// `root` 由调用方给（组合根知道图片根目录在哪）—— 本函数因此可以脱离
    /// 配置单独测。
    ///
    /// ★ 两道 `symlink` 判断都是**安全**要求，不是防御性编程：`metadata-search/`
    /// 下的条目名来自 uuid，但目录本身可能被替换成指向别处的链接；跟进去
    /// `remove_dir_all` 会删掉链接目标里的东西。
    pub fn cleanup_search_assets(root: &std::path::Path) -> Result<u64, ServiceError> {
        let metadata = match std::fs::symlink_metadata(root) {
            Ok(metadata) => metadata,
            // 不存在就是「没有可清理的」，不是错误。
            Err(_) => return Ok(0),
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Ok(0);
        }
        let now = now_seconds();
        let mut deleted = 0_u64;
        // 目录读不了（权限 / 被删）当作「没有可清理的」：这是一个**周期性
        // 清理任务**，它失败不该让整轮清理报错。
        let Ok(entries) = std::fs::read_dir(root) else {
            return Ok(0);
        };
        for entry in entries.filter_map(Result::ok) {
            let path = entry.path();
            let Ok(entry_meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !entry_meta.is_dir() || entry_meta.file_type().is_symlink() {
                continue;
            }
            let Ok(modified) = entry_meta.modified() else {
                continue;
            };
            let Ok(modified) = modified.duration_since(std::time::UNIX_EPOCH) else {
                // mtime 早于 epoch（时钟被改过）—— 当作不过期，宁可不删。
                continue;
            };
            if !is_stale(modified.as_secs() as i64, now) {
                continue;
            }
            if std::fs::remove_dir_all(&path).is_ok() {
                deleted += 1;
            }
        }
        // 目录空了就顺手删掉；非空（还有未过期的）时失败是**预期**的。
        let _ = std::fs::remove_dir(root);
        Ok(deleted)
    }
}

/// 缓存条目是否过期。**按 mtime 判**，不是按目录名里的 uuid（那不含时间）。
///
/// 边界：**恰好 24 小时算过期**（`>`，不是 `>=` —— 上游是 `now - mtime >
/// MAX_AGE`）。差一秒的语义在「每小时清一次」的调度下看不出来，但测试会。
fn is_stale(modified_seconds: i64, now_seconds: i64) -> bool {
    now_seconds.saturating_sub(modified_seconds) > SEARCH_ASSET_MAX_AGE_SECONDS
}

fn now_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or_default()
}

/// 候选 id 无效 / 指向的来源已失效 → 422。
fn invalid_candidate() -> ServiceError {
    ServiceError::validation("invalid_metadata_candidate", "元数据候选无效或已失效")
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

    /// 候选 id 现在是**上游那 10 个字段**（`media_import.py:76-86`）——
    /// 特别是**没有** `confidence`，客户端不能按它排序。
    #[test]
    fn the_candidate_wire_shape_has_no_confidence() {
        let candidate = MetadataCandidate {
            candidate_id: "javdb:ABC-123:xyz".to_owned(),
            source: MetadataCandidateSource::Javdb,
            source_name: "JavDB".to_owned(),
            source_id: None,
            javdb_id: Some("xyz".to_owned()),
            movie_number: "ABC-123".to_owned(),
            title: "t".to_owned(),
            cover_url: None,
            release_date: None,
            duration_minutes: 120,
        };
        let json = serde_json::to_value(&candidate).expect("序列化");
        assert!(
            json.get("confidence").is_none(),
            "confidence 是骨架期自造字段"
        );
        assert_eq!(json["source"], "javdb");
    }

    /// JavDB 候选 id 解出番号 + javdb id。
    #[test]
    fn a_javdb_candidate_resolves() {
        let reference =
            MovieMetadataSearchService::resolve_candidate_reference("javdb:abc-123:xyz789", |_| {
                false
            })
            .expect("合法 javdb id");
        assert_eq!(reference.source, MetadataCandidateSource::Javdb);
        assert_eq!(reference.javdb_id.as_deref(), Some("xyz789"));
        assert_eq!(reference.movie_number, "ABC-123", "番号要归一");
        assert!(reference.plugin_id.is_none());
    }

    /// 插件候选 id 只有**插件仍启用**时才解得出 —— 否则 422。
    ///
    /// ★ 这条是「必然失败的重试」的唯一拦截点：候选 id 会随搜索结果落到客户端
    /// 手里缓存很久，那时插件可能已经被卸载。
    #[test]
    fn a_plugin_candidate_requires_the_plugin_to_be_enabled() {
        let enabled = |plugin: &str| plugin == "javbus";
        let ok = MovieMetadataSearchService::resolve_candidate_reference(
            "plugin:javbus:abc-123",
            enabled,
        )
        .expect("插件已启用");
        assert_eq!(ok.source, MetadataCandidateSource::Plugin);
        assert_eq!(ok.plugin_id.as_deref(), Some("javbus"));

        let error = MovieMetadataSearchService::resolve_candidate_reference(
            "plugin:uninstalled:abc-123",
            enabled,
        )
        .expect_err("插件已卸载就该报错");
        assert_eq!(error.code(), "invalid_metadata_candidate");
    }

    /// 空段 / 段数不对 / 前缀不认识 —— 全 422。
    #[test]
    fn malformed_candidate_ids_are_rejected() {
        // ⚠️ 判据**只有两条**：段数 = 3、前缀是 `javdb` / `plugin`。
        // 上游（`:241-261`）不校验番号形状 —— 所以 `javdb:only:two` 是**合法**的
        // （真伪由后面 fetch 时的 `_ensure_candidate_number` 兜）。别在这里加
        // 自造的格式规则，那会把上游能接受的重试挡掉。
        for raw in [
            "",
            "javdb",
            "javdb::xyz",
            "javdb:abc-123:",
            "ffprobe:abc:xyz",
            "javdb:abc:xyz:extra",
        ] {
            let error = MovieMetadataSearchService::resolve_candidate_reference(raw, |_| true)
                .expect_err(&format!("{raw:?} 应该被拒"));
            assert_eq!(error.code(), "invalid_metadata_candidate");
        }
        assert!(
            MovieMetadataSearchService::resolve_candidate_reference("javdb:only:two", |_| true)
                .is_ok(),
            "上游只看段数与前缀"
        );
    }

    /// 编码与解码是一对（否则「搜索给的 id 重试时解不出来」）。
    #[test]
    fn candidate_ids_round_trip() {
        let id = MovieMetadataSearchService::javdb_candidate_id("ABC-123", "xyz");
        assert_eq!(id, "javdb:ABC-123:xyz");
        let reference =
            MovieMetadataSearchService::resolve_candidate_reference(&id, |_| false).expect("可解");
        assert_eq!(reference.movie_number, "ABC-123");
        assert_eq!(reference.javdb_id.as_deref(), Some("xyz"));

        let id = MovieMetadataSearchService::plugin_candidate_id("javbus", "ABC-123");
        assert_eq!(id, "plugin:javbus:ABC-123");
        let reference =
            MovieMetadataSearchService::resolve_candidate_reference(&id, |_| true).expect("可解");
        assert_eq!(reference.plugin_id.as_deref(), Some("javbus"));
    }

    /// 过期边界：**恰好 24 小时算过期**（上游是 `now - mtime > MAX_AGE`）。
    #[test]
    fn the_asset_expiry_boundary_is_exclusive() {
        assert!(is_stale(1_000, 1_000 + SEARCH_ASSET_MAX_AGE_SECONDS + 1));
        assert!(!is_stale(1_000, 1_000 + SEARCH_ASSET_MAX_AGE_SECONDS));
    }

    /// 一个空目录里没有可清理的东西，且**不报错**。
    #[test]
    fn cleaning_a_missing_root_is_not_an_error() {
        let root = std::path::Path::new("definitely-not-here-12345");
        assert_eq!(
            MovieMetadataSearchService::cleanup_search_assets(root).expect("不报错"),
            0
        );
    }
}
