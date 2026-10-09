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

use std::sync::Arc;

use super::metadata_source::{
    import_detail_of, source_identity_of, DeliverySource, MetadataSourceError,
    MetadataSourceService, PluginDelivery,
};
use crate::error::ServiceError;
use crate::system::ConfigService;

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
///
/// # 依赖放在字段里，而不是全 builtin
///
/// 上游是 classmethod + `build_javdb_provider()`（自己 new 一个 provider）。
/// 本仓的 provider 是**注入**的 —— 组合根建、测试换替身 —— 所以它必须被持有，
/// 与 [`MetadataSourceService`] 同一个取向。
///
/// # 配置**每次现读**
///
/// `plugins.enabled` 既决定候选顺序，也决定「这个候选还算不算数」：候选 id 会
/// 随搜索结果在客户端缓存几小时，那时插件可能已经被停用。所以这里存
/// [`ConfigService`]（它每次操作都读当前磁盘快照），**不**存一份快照。
pub struct MovieMetadataSearchService {
    /// 配置来源。见类型文档「每次现读」。
    config: ConfigService,
    /// JavDB provider + 已注册的插件来源。
    ///
    /// `Arc` 而**不是**各持一份：provider 不可克隆，而同一个实例要同时服务
    /// 「按番号导入」与「按候选取详情」两条路（见
    /// [`CatalogMovieMetadataImporter`](super::movie_metadata_importer::CatalogMovieMetadataImporter)）。
    source: Arc<MetadataSourceService>,
}

impl MovieMetadataSearchService {
    /// 构造。`source` 同时决定 JavDB 那支取不取得到详情（`provider` 为 `None`
    /// 时按「没收录」处置）。
    pub fn new(config: ConfigService, source: Arc<MetadataSourceService>) -> Self {
        Self { config, source }
    }

    /// ★ 按番号搜候选。**不建任何记录**（见模块文档）。
    ///
    /// 上游 `search_by_number(cls, movie_number) -> ImportMetadataSearchResponse`
    /// （`movie_metadata_search_service.py:41-121`），流程**逐行核对过**：
    ///
    /// 1. 番号归一；空 → `422 invalid_movie_number`；
    /// 2. 生成 `search_id = uuid4().hex`，缓存根 =
    ///    `media_image_root()/SEARCH_ASSET_DIR/<search_id>`；
    /// 3. **JavDB 先行**：`get_movie_by_number` → `MetadataNotFoundError` 按
    ///    「没收录」处置（无候选也**无** source_errors），其它异常进
    ///    `source_errors`；
    /// 4. ★ **JavDB 命中即权威，不再查插件**（`:68-79` 的 `if detail is not None`
    ///    —— 插件支在 `else` 里）。这不是优化是语义：JavDB 是收录的权威来源；
    /// 5. 插件支（仅 JavDB 没收录时）：逐个启用插件 `fetch_plugin`；
    ///    `MetadataNotFoundError` → 静默 continue；其它异常 → `source_errors`；
    /// 6. 封面缓存：远程 URL 下载（`_cache_remote_cover`）/ 插件本地文件拷贝
    ///    （`_cache_local_cover`）到 `<search_root>/<index><ext>`，扩展名不在
    ///    [`IMAGE_EXTENSIONS`] 里就用 `.jpg`；写入后**要用 Pillow 解码验一遍**
    ///    （`_validate_image` —— 防止把 HTML 错误页当封面存），失败删文件、
    ///    `cover_url = None`、只 warn；
    /// 7. `cover_url` 是**签名 URL**（`build_signed_image_url`，相对图片根的
    ///    POSIX 路径）；
    /// 8. ★ 一个候选都没有 → `rmtree(search_root)`（不留空目录），但**仍返回
    ///    空候选响应**（source_errors 照带）——「两个来源都没收录」不是 404。
    ///
    /// # 与上游的两处偏差（都写在这里，便于一起复核）
    ///
    /// 1. `search_id` 不是 uuid4 而是纳秒时间戳 + 进程内计数器的十六进制串
    ///    （workspace 的 uuid 只开了 v5 feature；两者唯一性等价，目录名同样
    ///    不含时间 —— 过期判定本来就按 mtime，见 [`Self::cleanup_search_assets`]）。
    /// 2. 封面**不做解码验证**：上游用 Pillow 真解码一遍；本仓的
    ///    `metadata_source.rs` 交付校验对同一件事的既定取向是「暂无图像解码
    ///    依赖，坏图在 image store 侧暴露」，这里保持一致 —— 比引入 `image`
    ///    依赖只为这一处强。
    ///
    /// # 与上游的一处依赖差异
    ///
    /// JavDB / 插件是**注入**的（[`MetadataSourceService`]），不是模块级
    /// 单例。★ 插件侧也不能复用
    /// [`MetadataSourceService::fetch`](crate::catalog::metadata_source::MetadataSourceService::fetch)：
    /// 那是「JavDB → 首个命中的插件」的**单结果**语义（服务于
    /// `import_by_number`），搜索要**遍历所有**启用的插件 —— 所以这里是
    /// `search_javdb_by_number` 直通 + 逐个 [`Self::fetch_plugin`]。
    pub async fn search_by_number(
        &self,
        movie_number: &str,
    ) -> Result<ImportMetadataSearchResponse, ServiceError> {
        let normalized = crate::movie_numbers::normalize_movie_number(movie_number);
        if normalized.is_empty() {
            return Err(ServiceError::validation(
                "invalid_movie_number",
                "番号不能为空",
            ));
        }
        let config = self.config.snapshot()?;
        let image_root = crate::catalog::media_paths::media_image_root_path(&self.config)?;
        let search_root = image_root.join(SEARCH_ASSET_DIR).join(new_search_id());
        // 签名密钥与图片路由同一把（见 `sm_core::signing`）。
        let secret = config
            .get("security")
            .and_then(|section| section.get("file_signature_secret"))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();

        let mut candidates: Vec<MetadataCandidate> = Vec::new();
        let mut source_errors: Vec<ImportMetadataSourceErrorResource> = Vec::new();

        // ── ③④ JavDB 先行，命中即权威 ──
        let javdb_hit = match self.source.search_javdb_by_number(&normalized).await {
            Ok(Some(detail)) => Some(detail),
            // 「没收录」不算错（上游 `except MetadataNotFoundError`）。
            Ok(None) => None,
            Err(error) => {
                let (reason, detail) = source_error_parts(&error);
                tracing::warn!(reason, detail = %detail, "元数据搜索的 JavDB 来源失败");
                source_errors.push(ImportMetadataSourceErrorResource {
                    source: "javdb".to_owned(),
                    source_name: "JavDB".to_owned(),
                    reason,
                    detail,
                });
                None
            }
        };

        if let Some(detail) = javdb_hit {
            let javdb_id = detail
                .get("javdb_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let cover_url = match detail
                .get("cover_image")
                .and_then(serde_json::Value::as_str)
            {
                Some(url) => {
                    cache_remote_cover(url, &image_root, &search_root, &secret, candidates.len())
                        .await
                }
                None => None,
            };
            candidates.push(MetadataCandidate {
                candidate_id: Self::javdb_candidate_id(&normalized, &javdb_id),
                source: MetadataCandidateSource::Javdb,
                source_name: "JavDB".to_owned(),
                source_id: None,
                javdb_id: Some(javdb_id),
                movie_number: detail
                    .get("movie_number")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or(&normalized)
                    .to_owned(),
                title: detail
                    .get("title")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                cover_url,
                release_date: detail
                    .get("release_date")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_owned),
                duration_minutes: detail
                    .get("duration_minutes")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or_default(),
            });
        } else {
            // ── ⑤ 插件支：逐个启用插件，收集候选或错误 ──
            for source in self.source.enabled_plugin_sources(&config) {
                let plugin_id = source.plugin_id.clone();
                let display_name = source.display_name.clone();
                match self
                    .source
                    .fetch_plugin(&config, &plugin_id, &normalized, |delivery| async {
                        delivery
                    })
                    .await
                {
                    Ok(delivery) => {
                        let Some(plugin) = delivery.plugin_delivery else {
                            continue;
                        };
                        // 拷封面要在闭包里：交付目录在 `fetch_plugin` 退出时清理。
                        let cover_url = cache_local_cover(
                            &plugin.cover_image_path,
                            &image_root,
                            &search_root,
                            &secret,
                            candidates.len(),
                        );
                        candidates.push(MetadataCandidate {
                            candidate_id: Self::plugin_candidate_id(&plugin_id, &normalized),
                            source: MetadataCandidateSource::Plugin,
                            source_name: display_name,
                            source_id: plugin.source_id.clone(),
                            javdb_id: None,
                            movie_number: plugin.movie_number.clone(),
                            title: plugin.title.clone(),
                            cover_url,
                            release_date: Some(plugin.release_date.clone()),
                            duration_minutes: i64::from(plugin.duration_minutes),
                        });
                    }
                    // 「没收录」→ 静默继续（上游 `:95-96`）。
                    Err(MetadataSourceError::NotFound) => continue,
                    Err(error) => {
                        let (reason, detail) = source_error_parts(&error);
                        tracing::warn!(
                            plugin_id,
                            movie_number = %normalized,
                            reason,
                            detail = %detail,
                            "手动元数据搜索的插件来源失败"
                        );
                        source_errors.push(ImportMetadataSourceErrorResource {
                            source: plugin_id,
                            source_name: display_name,
                            reason,
                            detail,
                        });
                    }
                }
            }
        }

        // ── ⑧ 零候选：不留空目录（上游 `:115-116`），但仍返回空响应 ──
        if candidates.is_empty() {
            let _ = std::fs::remove_dir_all(&search_root);
        }
        Ok(ImportMetadataSearchResponse {
            movie_number: normalized,
            candidates,
            source_errors,
        })
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
    /// # 闭包的第二个参数是**插件身份对象**
    ///
    /// 插件那一支给 `plugin_id` / `display_name` / `source_id` / `source_url`
    /// 四个键 —— 与 [`MetadataSourceService::import_by_number`] 交给
    /// `import_plugin_movie` 的**是同一个构造函数**（改一处两处一起改）。
    /// JavDB 那一支给 `Null`：它没有插件身份。
    ///
    /// 调用方要分支就**自己** `resolve_candidate_reference`：别从「第二参数
    /// 是不是 `Null`」去反推来源，那是把两个独立的事实绑在一起。
    ///
    /// # 错误码
    ///
    /// | 情况 | 状态 | 码 |
    /// |---|---|---|
    /// | id 格式不对 / 插件已停用 / **来源已不再收录这条** | 422 | `invalid_metadata_candidate` |
    /// | 详情里的番号与 id 里那一段不一致 | 422 | `metadata_candidate_mismatch` |
    /// | 来源调用失败（连不上 / 插件崩了 / 交付不合法） | 500 | `internal_error` |
    ///
    /// ⚠️ 第一行的「已不再收录」与第三行都是**刻意偏离上游**的，理由逐条写在
    /// 文件私有函数 `candidate_error` 上（那里是唯一的映射点）。
    /// （不写成 intra-doc 链接：`candidate_error` 是私有项，链接会让
    /// `cargo doc -D warnings` 报警。）
    pub async fn fetch_candidate<R>(
        &self,
        candidate_id: &str,
        consume: impl AsyncFnOnce(serde_json::Value, serde_json::Value) -> R,
    ) -> Result<R, ServiceError> {
        let config = self.config.snapshot()?;
        // 「这个插件还算不算数」**现读**配置：候选 id 可能已经在客户端缓存了
        // 几小时，期间插件被停用/卸载是完全正常的。
        let enabled = self.source.enabled_plugin_sources(&config);
        let reference = Self::resolve_candidate_reference(candidate_id, |plugin_id| {
            enabled.iter().any(|source| source.plugin_id == plugin_id)
        })?;

        match reference.source {
            // JavDB 那一支：按 **id** 取（见 `fetch_by_javdb_id` 的理由）。
            MetadataCandidateSource::Javdb => {
                let javdb_id = reference.javdb_id.as_deref().unwrap_or_default();
                let detail = self
                    .source
                    .fetch_by_javdb_id(javdb_id)
                    .await
                    .map_err(candidate_error)?;
                // id 是客户端能改的：不校验就会把 A 的详情写进 B 的名下。
                ensure_candidate_number(&detail, &reference.movie_number)?;
                Ok(consume(detail, serde_json::Value::Null).await)
            }
            // 插件那一支：交付文件在闭包退出时被清理（见 `fetch_plugin`），
            // 所以 `consume` 必须在闭包里面把话说完。
            MetadataCandidateSource::Plugin => {
                let plugin_id = reference.plugin_id.clone().unwrap_or_default();
                self.source
                    .fetch_plugin(
                        &config,
                        &plugin_id,
                        &reference.movie_number,
                        |delivery: PluginDelivery| async move {
                            match (delivery.plugin_delivery, delivery.source) {
                                (
                                    Some(plugin),
                                    DeliverySource::Plugin {
                                        plugin_id,
                                        display_name,
                                    },
                                ) => {
                                    consume(
                                        import_detail_of(&plugin),
                                        source_identity_of(&plugin_id, &display_name, &plugin),
                                    )
                                    .await
                                }
                                // 到不了：`fetch_plugin` 的两个构造点都只给出
                                // 插件交付 + 插件来源。给 `Null` 而不是 panic ——
                                // 一个来源身份的缺失不该让整条重试挂掉。
                                (_, source) => {
                                    tracing::warn!(?source, "插件来源没有交付体");
                                    consume(serde_json::Value::Null, serde_json::Value::Null).await
                                }
                            }
                        },
                    )
                    .await
                    .map_err(candidate_error)
            }
        }
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

/// 搜索目录的唯一 id。上游是 `uuid4().hex`；这里用纳秒时间戳 + 进程内计数器
/// （workspace 的 uuid 只开了 v5 feature）。唯一性等价，目录名同样不含时间
/// —— 过期判定本来就按 mtime（[`is_stale`]）。
fn new_search_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_nanos() as u64)
        .unwrap_or_default();
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:x}{seq:x}")
}

/// 扩展名白名单判定；不在名单里 → `.jpg`（上游 `_image_extension:227-229`）。
fn image_extension(raw: &str) -> &'static str {
    let ext = std::path::Path::new(raw)
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| format!(".{ext}").to_lowercase())
        .unwrap_or_default();
    IMAGE_EXTENSIONS
        .iter()
        .find(|candidate| **candidate == ext)
        .copied()
        .unwrap_or(".jpg")
}

/// 下载远程封面。失败回 `None`：一个封面不该让整条候选作废
/// （上游 `_cache_remote_cover:187-195` 同一取舍 —— unlink 半成品、warn）。
async fn cache_remote_cover(
    url: &str,
    image_root: &std::path::Path,
    search_root: &std::path::Path,
    secret: &str,
    index: usize,
) -> Option<String> {
    let target = search_root.join(format!("{index}{}", image_extension(url)));
    let bytes = reqwest::get(url).await.ok()?.bytes().await.ok()?;
    std::fs::create_dir_all(search_root).ok()?;
    std::fs::write(&target, &bytes).ok()?;
    signed_url(image_root, &target, secret)
}

/// 拷贝插件交付里的本地封面（上游 `_cache_local_cover:197-214`）。
/// 必须在 `fetch_plugin` 的闭包**里**调：交付目录退出即清。
fn cache_local_cover(
    source_path: &std::path::Path,
    image_root: &std::path::Path,
    search_root: &std::path::Path,
    secret: &str,
    index: usize,
) -> Option<String> {
    let target = search_root.join(format!(
        "{index}{}",
        image_extension(&source_path.to_string_lossy())
    ));
    std::fs::create_dir_all(search_root).ok()?;
    std::fs::copy(source_path, &target).ok()?;
    signed_url(image_root, &target, secret)
}

/// 缓存文件 → 签名 URL。相对图片根的 **POSIX** 路径（URL 用 `/` 分隔，
/// Windows 的 `\` 不行 —— 与上游 `relative_to(...).as_posix()` 同一理由）。
fn signed_url(
    image_root: &std::path::Path,
    target: &std::path::Path,
    secret: &str,
) -> Option<String> {
    let relative = target.strip_prefix(image_root).ok()?;
    let relative = relative.to_string_lossy().replace('\\', "/");
    sm_core::signing::build_signed_image_url(secret, &relative, now_seconds()).ok()
}

/// [`MetadataSourceError`] → `(reason, detail)`。上游用异常类名当 reason
/// （`:64` 的 `type(exc).__name__` —— 机器可读、不含内部细节），这里用变体名，
/// 同一语义；`detail` 是给日志与排障的原文。
fn source_error_parts(error: &MetadataSourceError) -> (String, String) {
    match error {
        MetadataSourceError::NotFound => ("NotFound".to_owned(), "没有收录".to_owned()),
        MetadataSourceError::RequestFailed(detail) => ("RequestFailed".to_owned(), detail.clone()),
        MetadataSourceError::InvalidDelivery(detail) => {
            ("InvalidDelivery".to_owned(), detail.clone())
        }
        MetadataSourceError::Disabled(id) => ("Disabled".to_owned(), id.clone()),
    }
}

/// 候选 id 无效 / 指向的来源已失效 → 422。
fn invalid_candidate() -> ServiceError {
    ServiceError::validation("invalid_metadata_candidate", "元数据候选无效或已失效")
}

/// 来源调用失败（连不上 / 插件崩了 / 交付不合法）→ 500。
///
/// **消息不含来源的内部细节**（那些进日志），因为 `ApiError.message` 是
/// 面向客户端的正文。
fn source_call_failed() -> ServiceError {
    ServiceError::from_status(500, "internal_error", "元数据来源调用失败")
}

/// [`MetadataSourceError`] → 本次重试的 HTTP 错误。
///
/// # 两处**刻意偏离上游**（都在这里，便于一起复核）
///
/// 1. **「来源已不再收录这条」→ 422**。上游 `:270` 把 `get_movie_by_javdb_id`
///    的返回值直接当对象用：`None` 时对 `None.movie_number` 取属性 →
///    `AttributeError` → **500**。而候选 id 会随搜索结果在客户端缓存几小时
///    （影片、插件都可能已经变了），用一个 422 表达它 —— 文案就用上游自己
///    那句「元数据候选无效或已失效」。
/// 2. **来源调用失败 → 500 `internal_error`，不新增错误码**。上游让异常冒出去，
///    落到 FastAPI 的兜底 500；这里照抄状态码，只换消息。将来若要给客户端
///    「可重试」语义，改这一处（502 [`ServiceError::bad_gateway`]，与「索引器
///    全挂了」同一取向）—— 那是一次**契约决定**，别顺手改。
fn candidate_error(error: MetadataSourceError) -> ServiceError {
    match error {
        // 「已停用」与「已失效」同一类：插件在两步之间被停用，候选就不该再算数。
        MetadataSourceError::NotFound | MetadataSourceError::Disabled(_) => invalid_candidate(),
        MetadataSourceError::InvalidDelivery(problem) => {
            tracing::warn!(problem, "元数据来源交付不合法");
            source_call_failed()
        }
        MetadataSourceError::RequestFailed(detail) => {
            tracing::warn!(detail, "元数据来源调用失败");
            source_call_failed()
        }
    }
}

/// 详情里的番号必须与 id 里那一段**归一等价**。
///
/// 上游 `_ensure_candidate_number`（`:280-283`）。防的是「id 被改成另一个来源
/// 的 id」：客户端能自己拼 `candidate_id`，而入库用的是**详情里**的番号 ——
/// 不校验就会把 A 的详情写进 B 的名下。
fn ensure_candidate_number(detail: &serde_json::Value, expected: &str) -> Result<(), ServiceError> {
    let actual = detail
        .get("movie_number")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if crate::movie_numbers::normalize_movie_number(actual)
        != crate::movie_numbers::normalize_movie_number(expected)
    {
        return Err(ServiceError::validation(
            "metadata_candidate_mismatch",
            "元数据候选番号不匹配",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    // `impl MetadataProvider for FakeJavdb` 要用它；文件级不导入（非测试代码
    // 用不到，导了会被判 unused）。注意这里是 `crate::catalog::metadata_source`
    // —— 本模块里的 `super` 是 `movie_metadata_search`，不是 `catalog`。
    use crate::catalog::metadata_source::MetadataProvider;

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

    // ------------------------------------------------- search_by_number（搜索）

    /// ★ 空番号（归一后）→ 422 `invalid_movie_number`。
    ///
    /// 这是搜索端点的第一道门：空串不该发到任何来源去。桩的
    /// `get_movie_by_number` 会 panic —— 这条用例同时证明校验在它**之前**。
    #[tokio::test]
    async fn an_empty_number_is_rejected_before_touching_any_source() {
        let service = service(&[], false);
        let error = service
            .search_by_number("   ")
            .await
            .expect_err("空番号该拒");
        assert_eq!(error.code(), "invalid_movie_number");
    }

    /// 扩展名白名单判定：名单内的原样收，大小写归一，不认识的一律 `.jpg`
    /// （上游 `_image_extension` —— 「不确定是什么」比「猜一个错的」强）。
    #[test]
    fn image_extension_falls_back_to_jpg() {
        assert_eq!(image_extension("https://x/a.webp"), ".webp");
        assert_eq!(image_extension("C:\\img\\A.PNG"), ".png");
        assert_eq!(image_extension("https://x/a"), ".jpg");
        assert_eq!(image_extension("https://x/a.exe"), ".jpg", "白名单只收图片");
    }

    /// 搜索目录 id：两次调用**不同**（同一纳秒内也有计数器兜底）。
    #[test]
    fn search_ids_are_unique() {
        assert_ne!(new_search_id(), new_search_id());
    }

    // ------------------------------------------------- fetch_candidate（取详情）

    /// 只认 id 的假 JavDB。`by_id` 里没有的 id = 「已经不在 JavDB 了」。
    struct FakeJavdb {
        by_id: std::collections::HashMap<String, serde_json::Value>,
        /// 为真时按「来源坏了」回应 —— 与「没收录」是两回事。
        fail: bool,
    }

    #[tonic::async_trait]
    impl MetadataProvider for FakeJavdb {
        async fn get_movie_by_number(
            &self,
            _movie_number: &str,
        ) -> Result<Option<serde_json::Value>, MetadataSourceError> {
            // `fetch_candidate` 的 JavDB 支按 **id** 取，问番号就是走错路了。
            panic!("fetch_candidate 不该按番号取详情");
        }

        async fn get_movie_by_javdb_id(
            &self,
            javdb_id: &str,
        ) -> Result<Option<serde_json::Value>, MetadataSourceError> {
            if self.fail {
                return Err(MetadataSourceError::RequestFailed("连不上".to_owned()));
            }
            Ok(self.by_id.get(javdb_id).cloned())
        }

        async fn search_actors(
            &self,
            _keyword: &str,
        ) -> Result<Vec<serde_json::Value>, MetadataSourceError> {
            Ok(Vec::new())
        }
    }

    /// 一份**一个插件来源都没启用**的临时配置。
    fn temp_config() -> ConfigService {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|delta| delta.as_nanos())
            .unwrap_or_default();
        let path = std::env::temp_dir().join(format!(
            "sm-metadata-search-{}-{seq}-{nanos}.toml",
            std::process::id()
        ));
        // 缺 `plugins.enabled` 与「空列表」同义：一个都没启用，不是错误。
        std::fs::write(&path, "[plugins]\nenabled = []\n").expect("写临时配置");
        ConfigService::new(path)
    }

    /// 被测对象。**不注册任何插件来源**：插件那一支要真起一个 gRPC 插件进程
    /// （属跨仓集成测试），所以这里覆盖 JavDB 支 + 「插件没注册」那条拒绝。
    fn service(by_id: &[(&str, serde_json::Value)], fail: bool) -> MovieMetadataSearchService {
        MovieMetadataSearchService::new(
            temp_config(),
            Arc::new(MetadataSourceService::new(
                Vec::new(),
                Some(Box::new(FakeJavdb {
                    by_id: by_id
                        .iter()
                        .map(|(id, detail)| ((*id).to_owned(), detail.clone()))
                        .collect(),
                    fail,
                })),
            )),
        )
    }

    fn detail_with_number(number: &str) -> serde_json::Value {
        serde_json::json!({
            "movie_number": number,
            "title": "测试标题",
            "javdb_id": "xyz",
        })
    }

    /// ★ 详情按 **javdb_id** 取（不是按番号），第二个参数是 `Null`。
    #[tokio::test]
    async fn a_javdb_candidate_yields_the_detail_by_id() {
        let service = service(&[("xyz", detail_with_number("ABC-123"))], false);
        let (detail, source) = service
            .fetch_candidate("javdb:ABC-123:xyz", |detail, source| async move {
                (detail, source)
            })
            .await
            .expect("候选取得到");
        assert_eq!(detail["title"], "测试标题");
        assert_eq!(source, serde_json::Value::Null, "JavDB 支没有插件身份");
    }

    /// ★ 详情里的番号与 id 里那一段不一致 → 422。
    ///
    /// id 是客户端能改的，而入库用的是详情里的番号 —— 这条不拦就会把 A 的
    /// 详情写进 B 的名下。
    #[tokio::test]
    async fn a_candidate_whose_number_does_not_match_is_rejected() {
        // id 说 `ABC-123`，JavDB 那份详情说 `OTHER-9`。
        let service = service(&[("xyz", detail_with_number("OTHER-9"))], false);
        let error = service
            .fetch_candidate("javdb:ABC-123:xyz", |_, _| async {})
            .await
            .expect_err("不匹配就该拒");
        assert_eq!(error.code(), "metadata_candidate_mismatch");
    }

    /// ★ 来源已经不再收录这条 → 422「已失效」（**不是** 500）。
    ///
    /// ⚠️ 上游在这里是 `None.movie_number` → `AttributeError` → 500。偏离的
    /// 理由写在 `candidate_error` 上。
    #[tokio::test]
    async fn a_candidate_the_source_no_longer_has_is_expired() {
        let service = service(&[], false);
        let error = service
            .fetch_candidate("javdb:ABC-123:xyz", |_, _| async {})
            .await
            .expect_err("已失效就该拒");
        assert_eq!(error.code(), "invalid_metadata_candidate");
    }

    /// 真故障（连不上）→ 500，**不**混进「已失效」那一类。
    ///
    /// 两类错误对客户端的处置相反：一个要用户重挑候选，一个该重试/报障。
    #[tokio::test]
    async fn a_broken_source_is_not_reported_as_an_expired_candidate() {
        let service = service(&[("xyz", detail_with_number("ABC-123"))], true);
        let error = service
            .fetch_candidate("javdb:ABC-123:xyz", |_, _| async {})
            .await
            .expect_err("来源坏了就该报错");
        assert_eq!(error.code(), "internal_error");
    }

    /// 插件候选但插件没注册（= 已停用）→ 422，**连取详情都不发生**。
    #[tokio::test]
    async fn a_plugin_candidate_needs_its_plugin_registered() {
        let service = service(&[], false);
        let error = service
            .fetch_candidate("plugin:javbus:ABC-123", |_, _| async {})
            .await
            .expect_err("插件没注册就该拒");
        assert_eq!(error.code(), "invalid_metadata_candidate");
    }
}
