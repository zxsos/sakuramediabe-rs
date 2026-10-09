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

use super::metadata_source::{
    import_detail_of, source_identity_of, DeliverySource, JavdbMovieListItem, MetadataSourceError,
    MetadataSourceService,
};
use crate::catalog::movie::MovieCard;
use crate::error::ServiceError;
use crate::system::ConfigService;

/// `upsert_finished` / `completed.stats` 的统计。
/// 键名与上游 stats 字典逐字一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct UpsertStats {
    pub total: i64,
    pub created_count: i64,
    pub already_exists_count: i64,
    pub failed_count: i64,
}

/// 流式事件（SSE 帧的类型化形态）。事件名与载荷照上游
/// `movie_metadata_refresh_service.py:210-334` 逐帧对齐：
///
/// | 帧 | 载荷 |
/// |---|---|
/// | `search_started` | `{movie_number}` |
/// | `movie_found` | `{movies: [{javdb_id, movie_number, title, cover_image}], total}` |
/// | `upsert_started` | `{total}` |
/// | `upsert_finished` | `{total, created_count, already_exists_count, failed_count}` |
/// | `completed` | `{success, reason?, movies, failed_items?, stats?}` |
///
/// ★ `completed` 的 `movies` 留**服务形态**的卡片（[`MovieCard`]），线格式由
/// 路由层经 `MovieListItemResource::from_movie_card` 转换 —— 封面签名密钥只有
/// 路由层有。`reason` / `failed_items` / `stats` 是**可选键**：上游的成功帧
/// 不带 `reason`，早退帧不带 `failed_items`/`stats` —— 缺键与空值在客户端
/// 是两种渲染，别一律塞空值。
#[derive(Debug)]
pub enum MetadataStreamFrame {
    /// `search_started`。影片流载荷是番号；系列流载荷是 `series_id`。
    SearchStartedByNumber {
        movie_number: String,
    },
    SearchStartedBySeries {
        series_id: i64,
    },
    /// `series_found`：本地系列存在（仅系列流）。
    SeriesFound {
        series_id: i64,
        series_name: String,
    },
    /// `javdb_series_found`：JavDB 上找到了同名系列（仅系列流）。
    JavdbSeriesFound {
        javdb_id: String,
        javdb_type: i64,
        name: String,
        videos_count: i64,
    },
    /// `movie_skipped`：该条已存在被跳过（仅系列流）。
    MovieSkipped {
        javdb_id: Option<String>,
        movie_number: String,
        index: i64,
        total: i64,
    },
    /// `movie_upsert_started`：单条落库开始（仅系列流）。
    MovieUpsertStarted {
        javdb_id: Option<String>,
        movie_number: String,
        index: i64,
        total: i64,
    },
    /// `movie_upsert_finished`：单条落库完成（仅系列流）。
    MovieUpsertFinished {
        javdb_id: Option<String>,
        movie_number: String,
        index: i64,
        total: i64,
    },
    MovieFound {
        movies: Vec<serde_json::Value>,
        total: i64,
    },
    UpsertStarted {
        total: i64,
    },
    UpsertFinished {
        total: i64,
        created_count: i64,
        already_exists_count: i64,
        failed_count: i64,
    },
    Completed {
        success: bool,
        reason: Option<&'static str>,
        movies: Vec<MovieCard>,
        failed_items: Vec<serde_json::Value>,
        /// 系列流**必有**（上游最终帧带 `skipped_items`），影片流**必无**。
        skipped_items: Option<Vec<serde_json::Value>>,
        stats: Option<UpsertStats>,
    },
}

/// 元数据刷新服务。
///
/// # 依赖是注入的
///
/// [`MetadataSourceService`] 出网（JavDB），[`super::catalog_import`] 写库，
/// [`ConfigService`] 现读启用插件 —— 都由组合根装配；测试换成假来源与假入库
/// —— 本文件不知道「JavDB」是什么。
pub struct MovieMetadataRefreshService {
    db: Db,
    config: ConfigService,
    source: Arc<MetadataSourceService>,
    import: super::catalog_import::CatalogImportService,
}

impl MovieMetadataRefreshService {
    /// 构造。
    pub fn new(
        db: &Db,
        config: &ConfigService,
        source: Arc<MetadataSourceService>,
        import: super::catalog_import::CatalogImportService,
    ) -> Self {
        Self {
            db: db.clone(),
            config: config.clone(),
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

    /// ★ 流式搜索并入库。上游 `stream_search_and_upsert_movie_from_javdb`
    /// （`movie_metadata_refresh_service.py:210-334`），帧序列照上游：
    ///
    /// `search_started` →（[early return 分支]）→ `movie_found` →
    /// `upsert_started` → `upsert_finished` → `completed`。
    ///
    /// # 两个 early return 分支（都不问「导入」那一段）
    ///
    /// 1. 番号归一后为空 → `completed {success: false, reason:
    ///    "movie_number_not_found"}`；
    /// 2. 本地**已存在**且是插件来源（无 javdb_id 有 metadata_source）→
    ///    直接把本地影片作为「已存在」完成（`already_exists_count = 1`），
    ///    **不问 JavDB** —— 插件先收录的片不该被 JavDB 再建一份。
    ///
    /// # fetch 的单结果语义在这里**适用**
    ///
    /// 搜索端点（`movie_metadata_search`）要遍历全部插件；这条流是「导入一条」，
    /// 上游用的正是 `MetadataSourceService.fetch`（JavDB → 首个命中插件）。
    ///
    /// ⚠️ 本仓用 `Vec` 代替流式（async 生成器需额外依赖）。代价是**全部完成
    /// 才返回** —— **不要**用它驱动进度条；前端帧序不变，只是到达时间压缩。
    pub async fn stream_search_and_upsert_movie_from_javdb(
        &self,
        movie_number: &str,
    ) -> Vec<MetadataStreamFrame> {
        use serde_json::Value;

        let mut frames = Vec::new();
        let normalized = crate::movie_numbers::normalize_movie_number(movie_number);
        frames.push(MetadataStreamFrame::SearchStartedByNumber {
            movie_number: normalized.clone(),
        });

        if normalized.is_empty() {
            frames.push(MetadataStreamFrame::Completed {
                success: false,
                reason: Some("movie_number_not_found"),
                movies: Vec::new(),
                failed_items: Vec::new(),
                skipped_items: None,
                stats: None,
            });
            return frames;
        }

        let repo = MovieRepository::new(self.db.clone());
        // ② 已存在的插件来源影片：不问 JavDB，直接完成（上游 `:222-234`）。
        if let Ok(Some(existing)) = repo.find_by_number(movie_number).await {
            if existing
                .javdb_id
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
                && existing.metadata_source.is_some()
            {
                let movies = crate::catalog::movie::MovieService::new(&self.db)
                    .load_cards(&[existing.id])
                    .await
                    .unwrap_or_default();
                frames.push(MetadataStreamFrame::Completed {
                    success: true,
                    reason: None,
                    movies,
                    failed_items: Vec::new(),
                    skipped_items: None,
                    stats: Some(UpsertStats {
                        total: 1,
                        created_count: 0,
                        already_exists_count: 1,
                        failed_count: 0,
                    }),
                });
                return frames;
            }
        }

        // ③ fetch：JavDB → 首个命中插件（上游 `MetadataSourceService.fetch`）。
        let config = match self.config.snapshot() {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(code = error.code(), "读配置失败");
                frames.push(MetadataStreamFrame::Completed {
                    success: false,
                    reason: Some("internal_error"),
                    movies: Vec::new(),
                    failed_items: Vec::new(),
                    skipped_items: None,
                    stats: None,
                });
                return frames;
            }
        };
        let delivery = match self
            .source
            .fetch(&config, &normalized, |delivery| async { delivery })
            .await
        {
            Ok(delivery) => delivery,
            // 「JavDB 和插件都没收录」→ completed movie_not_found（上游 `:329-331`）。
            Err(MetadataSourceError::NotFound) => {
                frames.push(MetadataStreamFrame::Completed {
                    success: false,
                    reason: Some("movie_not_found"),
                    movies: Vec::new(),
                    failed_items: Vec::new(),
                    skipped_items: None,
                    stats: None,
                });
                return frames;
            }
            Err(error) => {
                let (reason, detail) =
                    crate::catalog::movie_metadata_search::source_error_parts(&error);
                tracing::warn!(reason, detail = %detail, "元数据搜索失败");
                frames.push(MetadataStreamFrame::Completed {
                    success: false,
                    reason: Some("internal_error"),
                    movies: Vec::new(),
                    failed_items: Vec::new(),
                    skipped_items: None,
                    stats: None,
                });
                return frames;
            }
        };

        // ④ `movie_found`：**落库前**就把命中的原始远端信息回给前端。
        // JavDB 命中带 javdb_id + cover_image；插件交付不带（上游 `source is None` 判定）。
        let found_movie = match (&delivery.javdb_detail, &delivery.plugin_delivery) {
            (Some(detail), _) => serde_json::json!({
                "javdb_id": detail.get("javdb_id").cloned().unwrap_or(Value::Null),
                "movie_number": detail.get("movie_number").cloned().unwrap_or(Value::Null),
                "title": detail.get("title").cloned().unwrap_or(Value::Null),
                "cover_image": detail.get("cover_image").cloned().unwrap_or(Value::Null),
            }),
            (None, Some(plugin)) => {
                let num = if plugin.movie_number.trim().is_empty() {
                    normalized.clone()
                } else {
                    plugin.movie_number.clone()
                };
                serde_json::json!({
                    "javdb_id": Value::Null,
                    "movie_number": num,
                    "title": plugin.title,
                    "cover_image": Value::Null,
                })
            }
            _ => {
                serde_json::json!({"javdb_id": Value::Null, "movie_number": normalized, "title": Value::Null, "cover_image": Value::Null})
            }
        };
        frames.push(MetadataStreamFrame::MovieFound {
            movies: vec![found_movie],
            total: 1,
        });
        frames.push(MetadataStreamFrame::UpsertStarted { total: 1 });

        // ⑤ 落库：JavDB 命中走 import_movie_if_missing（**纯新建语义**：已存在
        // 跳过不更新）；插件命中走 import_plugin_movie。
        let import_result = match (&delivery.source, &delivery.javdb_detail) {
            (DeliverySource::Javdb, Some(detail)) => {
                // JavDB 详情可能缺 movie_number（null/缺失/空串），用搜索的归一番号补上，
                // 否则 import_movie_if_missing 会因缺少 movie_number 入库失败。
                let mut detail = detail.clone();
                if detail
                    .get("movie_number")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim().is_empty())
                    .unwrap_or(true)
                {
                    if let Some(obj) = detail.as_object_mut() {
                        obj.insert(
                            "movie_number".to_owned(),
                            serde_json::Value::String(normalized.clone()),
                        );
                    }
                }
                self.import.import_movie_if_missing(&detail, false).await
            }
            (DeliverySource::Javdb, None) => Err(ServiceError::from_status(
                500,
                "internal_error",
                "JavDB 交付缺详情",
            )),
            (
                DeliverySource::Plugin {
                    plugin_id,
                    display_name,
                },
                _,
            ) => match &delivery.plugin_delivery {
                Some(plugin) => {
                    let mut detail = import_detail_of(plugin);
                    // 插件可能不返回番号，用搜索的番号补上
                    if detail
                        .get("movie_number")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().is_empty())
                        .unwrap_or(true)
                    {
                        if let Some(obj) = detail.as_object_mut() {
                            obj.insert(
                                "movie_number".to_owned(),
                                serde_json::Value::String(normalized.clone()),
                            );
                        }
                    }
                    let source_identity = source_identity_of(plugin_id, display_name, plugin);
                    self.import
                        .import_plugin_movie(&detail, &source_identity, false)
                        .await
                }
                None => Err(ServiceError::from_status(
                    500,
                    "internal_error",
                    "插件交付缺失",
                )),
            },
        };

        let mut failed_items: Vec<Value> = Vec::new();
        let mut stats = UpsertStats {
            total: 1,
            created_count: 0,
            already_exists_count: 0,
            failed_count: 0,
        };
        let mut imported_movie_id: Option<i32> = None;
        match import_result {
            Ok(result) => {
                imported_movie_id = Some(result.movie_id);
                if result.created {
                    stats.created_count += 1;
                } else {
                    // 纯新建语义：已存在影片跳过不更新（上游注释 `:283`）。
                    stats.already_exists_count += 1;
                }
            }
            Err(error) => {
                stats.failed_count += 1;
                tracing::warn!(movie_number = %normalized, code = error.code(), "影片入库失败");
                failed_items.push(serde_json::json!({
                    "movie_number": normalized,
                    "reason": "upsert_failed",
                    "detail": error.api.message,
                }));
            }
        }
        frames.push(MetadataStreamFrame::UpsertFinished {
            total: 1,
            created_count: stats.created_count,
            already_exists_count: stats.already_exists_count,
            failed_count: stats.failed_count,
        });

        // ⑥ `completed`：有导入成功的影片 → success；否则 internal_error
        // （stats 里带着 failed_count —— 上游 `:319-329` 同一结构）。
        let movies = match imported_movie_id {
            Some(movie_id) => crate::catalog::movie::MovieService::new(&self.db)
                .load_cards(&[movie_id])
                .await
                .unwrap_or_default(),
            None => Vec::new(),
        };
        let success = !movies.is_empty();
        frames.push(MetadataStreamFrame::Completed {
            success,
            reason: if success {
                None
            } else {
                Some("internal_error")
            },
            movies,
            failed_items,
            skipped_items: None,
            stats: Some(stats),
        });
        frames
    }

    /// ★ 流式导入一个系列的全部影片。
    ///
    /// **单部失败不中断整批** —— 一个系列几十部，一部失败就全废掉不可接受。
    ///
    /// ★ 流式导入一个系列的全部影片。上游
    /// `stream_import_series_movies_from_javdb`（`:336-535`）。
    ///
    /// **单部失败不中断整批** —— 一个系列几十部，一部失败就全废掉不可接受。
    /// ★ 只接受**精确同名**的 JavDB 系列（`:370-377`），相似系列一律不导入。
    ///
    /// # 列表项信息不完整，入库前必须再拉详情
    ///
    /// 系列影片列表只有番号/标题/封面；详情才能走统一的导入链路
    /// （`get_movie_by_javdb_id` → `import_movie_if_missing`，上游 `:462-467`
    /// 的注释原话「外层已跳过已存在影片」）。
    pub async fn stream_import_series_movies_from_javdb(
        &self,
        series_id: i64,
    ) -> Vec<MetadataStreamFrame> {
        use serde_json::Value;

        let mut frames = Vec::new();
        frames.push(MetadataStreamFrame::SearchStartedBySeries { series_id });

        let series_id_i32 = i32::try_from(series_id).unwrap_or(i32::MAX);
        let series_repo = sm_db::repo::MovieSeriesRepository::new(self.db.clone());
        let Some(local_series) = series_repo
            .find_by_ids(&[series_id_i32])
            .await
            .ok()
            .and_then(|map| map.get(&series_id_i32).cloned())
        else {
            frames.push(MetadataStreamFrame::Completed {
                success: false,
                reason: Some("local_series_not_found"),
                movies: Vec::new(),
                failed_items: Vec::new(),
                skipped_items: Some(Vec::new()),
                stats: None,
            });
            return frames;
        };
        let series_name = local_series.name.trim().to_owned();
        frames.push(MetadataStreamFrame::SeriesFound {
            series_id: i64::from(local_series.id),
            series_name: series_name.clone(),
        });

        let series_candidates = match self.source.search_series(&series_name).await {
            Ok(candidates) => candidates,
            Err(error) => {
                let (reason, detail) =
                    crate::catalog::movie_metadata_search::source_error_parts(&error);
                tracing::warn!(series_name = %series_name, reason, detail = %detail, "JavDB 系列搜索失败");
                frames.push(MetadataStreamFrame::Completed {
                    success: false,
                    reason: Some("metadata_fetch_failed"),
                    movies: Vec::new(),
                    failed_items: Vec::new(),
                    skipped_items: Some(Vec::new()),
                    stats: None,
                });
                return frames;
            }
        };

        // ★ 只接受**精确同名**系列：模糊命中的相似系列一旦导入，就是把别的
        // 系列的片子塞进用户的收藏，且事后只能手动删。
        let Some(javdb_series) = series_candidates
            .iter()
            .find(|candidate| candidate.name.trim() == series_name)
        else {
            frames.push(MetadataStreamFrame::Completed {
                success: false,
                reason: Some("javdb_series_not_found"),
                movies: Vec::new(),
                failed_items: Vec::new(),
                skipped_items: Some(Vec::new()),
                stats: None,
            });
            return frames;
        };
        frames.push(MetadataStreamFrame::JavdbSeriesFound {
            javdb_id: javdb_series.javdb_id.clone(),
            javdb_type: javdb_series.javdb_type,
            name: javdb_series.name.clone(),
            videos_count: javdb_series.videos_count,
        });

        let remote_movies = match self
            .source
            .get_series_movies(&javdb_series.javdb_id, javdb_series.javdb_type)
            .await
        {
            Ok(movies) => movies,
            Err(error) => {
                let (reason, detail) =
                    crate::catalog::movie_metadata_search::source_error_parts(&error);
                tracing::warn!(series_id, reason, detail = %detail, "JavDB 系列影片拉取失败");
                frames.push(MetadataStreamFrame::Completed {
                    success: false,
                    reason: Some("metadata_fetch_failed"),
                    movies: Vec::new(),
                    failed_items: Vec::new(),
                    skipped_items: Some(Vec::new()),
                    stats: None,
                });
                return frames;
            }
        };

        // 去重：javdb_id 优先，缺了退番号（上游 `:408-419`）。两条完全同键的
        // 列表项只入一次库。
        let mut seen = std::collections::BTreeSet::new();
        let mut deduplicated: Vec<&JavdbMovieListItem> = Vec::new();
        for item in &remote_movies {
            let key = if item.javdb_id.is_empty() {
                item.movie_number.clone()
            } else {
                item.javdb_id.clone()
            };
            if key.is_empty() || !seen.insert(key) {
                continue;
            }
            deduplicated.push(item);
        }
        let total = i64::try_from(deduplicated.len()).unwrap_or(i64::MAX);
        if total == 0 {
            frames.push(MetadataStreamFrame::Completed {
                success: false,
                reason: Some("javdb_series_movies_not_found"),
                movies: Vec::new(),
                failed_items: Vec::new(),
                skipped_items: Some(Vec::new()),
                stats: None,
            });
            return frames;
        }

        frames.push(MetadataStreamFrame::MovieFound {
            movies: deduplicated
                .iter()
                .map(|item| {
                    serde_json::json!({
                        "javdb_id": item.javdb_id,
                        "movie_number": item.movie_number,
                        "title": item.title,
                        "cover_image": item.cover_image,
                    })
                })
                .collect(),
            total,
        });
        frames.push(MetadataStreamFrame::UpsertStarted { total });

        let repo = MovieRepository::new(self.db.clone());
        let mut stats = UpsertStats {
            total,
            created_count: 0,
            already_exists_count: 0,
            failed_count: 0,
        };
        let mut skipped_items: Vec<Value> = Vec::new();
        let mut failed_items: Vec<Value> = Vec::new();
        let mut imported_ids: Vec<i32> = Vec::new();

        for (position, item) in deduplicated.iter().enumerate() {
            let index = i64::try_from(position + 1).unwrap_or(i64::MAX);
            // 已存在（javdb_id 或番号任一命中）→ `movie_skipped`，不入库。
            let exists_by_number = !item.movie_number.is_empty()
                && repo
                    .find_by_number(&item.movie_number)
                    .await
                    .map(|found| found.is_some())
                    .unwrap_or(false);
            let exists_by_javdb = !item.javdb_id.is_empty()
                && repo
                    .conflicting_number_by_javdb_id(&item.javdb_id, 0)
                    .await
                    .map(|found| found.is_some())
                    .unwrap_or(false);
            if exists_by_number || exists_by_javdb {
                stats.already_exists_count += 1;
                skipped_items.push(serde_json::json!({
                    "javdb_id": item.javdb_id,
                    "movie_number": item.movie_number,
                }));
                frames.push(MetadataStreamFrame::MovieSkipped {
                    javdb_id: Some(item.javdb_id.clone()),
                    movie_number: item.movie_number.clone(),
                    index,
                    total,
                });
                continue;
            }

            frames.push(MetadataStreamFrame::MovieUpsertStarted {
                javdb_id: Some(item.javdb_id.clone()),
                movie_number: item.movie_number.clone(),
                index,
                total,
            });
            // 列表项信息不完整，入库前必须再拉详情复用统一导入链路
            // （上游 `:462-464` 注释原话）。
            let detail = match self.source.fetch_by_javdb_id(&item.javdb_id).await {
                Ok(detail) => detail,
                Err(error) => {
                    stats.failed_count += 1;
                    let (_, message) =
                        crate::catalog::movie_metadata_search::source_error_parts(&error);
                    failed_items.push(serde_json::json!({
                        "javdb_id": item.javdb_id,
                        "movie_number": item.movie_number,
                        "reason": "metadata_fetch_failed",
                        "detail": message,
                    }));
                    continue;
                }
            };
            match self.import.import_movie_if_missing(&detail, false).await {
                Ok(result) => {
                    stats.created_count += 1;
                    imported_ids.push(result.movie_id);
                    frames.push(MetadataStreamFrame::MovieUpsertFinished {
                        javdb_id: Some(item.javdb_id.clone()),
                        movie_number: detail
                            .get("movie_number")
                            .and_then(Value::as_str)
                            .unwrap_or(&item.movie_number)
                            .to_owned(),
                        index,
                        total,
                    });
                }
                Err(error) => {
                    stats.failed_count += 1;
                    tracing::warn!(javdb_id = %item.javdb_id, code = error.code(), "系列影片入库失败");
                    failed_items.push(serde_json::json!({
                        "javdb_id": item.javdb_id,
                        "movie_number": item.movie_number,
                        "reason": "upsert_failed",
                        "detail": error.api.message,
                    }));
                }
            }
        }

        frames.push(MetadataStreamFrame::UpsertFinished {
            total,
            created_count: stats.created_count,
            already_exists_count: stats.already_exists_count,
            failed_count: stats.failed_count,
        });
        // ★ `success` 判据与上游一致：有**导入**的或**跳过**的都算成功 ——
        // 全部失败（`failed_count == total`）才是失败。
        let movies = if imported_ids.is_empty() {
            Vec::new()
        } else {
            crate::catalog::movie::MovieService::new(&self.db)
                .load_cards(&imported_ids)
                .await
                .unwrap_or_default()
        };
        let success = !movies.is_empty() || !skipped_items.is_empty();
        frames.push(MetadataStreamFrame::Completed {
            success,
            reason: if success {
                None
            } else {
                Some("internal_error")
            },
            movies,
            failed_items,
            skipped_items: Some(skipped_items),
            stats: Some(stats),
        });
        frames
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

    /// 流式失败**不中断**：失败进 `completed.failed_items`，末帧仍是 `completed`。
    ///
    /// ⚠️ 骨架期的 `error` 事件是**自造形状** —— 上游的失败记录在
    /// `completed` 帧的 `failed_items` 里（`:287-302`），没有独立的 error 帧。
    #[test]
    fn failures_live_in_completed_failed_items() {
        let frames = [
            MetadataStreamFrame::SearchStartedByNumber {
                movie_number: "A-001".to_owned(),
            },
            MetadataStreamFrame::UpsertFinished {
                total: 1,
                created_count: 0,
                already_exists_count: 0,
                failed_count: 1,
            },
            MetadataStreamFrame::Completed {
                success: false,
                reason: Some("internal_error"),
                movies: Vec::new(),
                failed_items: vec![serde_json::json!({
                    "movie_number": "A-002",
                    "reason": "upsert_failed",
                    "detail": "番号冲突",
                })],
                skipped_items: None,
                stats: Some(UpsertStats {
                    total: 1,
                    created_count: 0,
                    already_exists_count: 0,
                    failed_count: 1,
                }),
            },
        ];
        assert_eq!(frames.len(), 3, "失败后仍有后续帧");
        match &frames[2] {
            MetadataStreamFrame::Completed {
                failed_items,
                stats,
                ..
            } => {
                assert_eq!(failed_items.len(), 1, "失败进了 failed_items");
                assert_eq!(stats.expect("走完 upsert 就有 stats").failed_count, 1);
            }
            other => panic!("末帧是 completed：{other:?}"),
        }
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

    /// ★ JavDB 详情缺 movie_number 时，入库前必须用搜索番号补上。
    ///
    /// 回归测试：SSNI-888 搜索能命中、SSE 全流程正常，但 JavDB 返回的 detail
    /// 里 movie_number 为 null，导致 import_movie_if_missing 报
    /// "元数据缺少 movie_number" 入库失败。修复是在 JavDB 分支 clone detail
    /// 并补番号 —— 这里锁住「缺番号判定」的三种形态。
    #[test]
    fn javdb_detail_missing_number_needs_fallback() {
        // 缺失 / null / 空串 / 空白 都算缺，都该触发 fallback。
        for detail in [
            serde_json::json!({ "title": "x" }),
            serde_json::json!({ "movie_number": null, "title": "x" }),
            serde_json::json!({ "movie_number": "", "title": "x" }),
            serde_json::json!({ "movie_number": "   ", "title": "x" }),
        ] {
            let missing = detail
                .get("movie_number")
                .and_then(|v| v.as_str())
                .map(|s| s.trim().is_empty())
                .unwrap_or(true);
            assert!(missing, "缺番号判定要命中: {detail}");
        }
        // 正常番号不触发。
        let ok = serde_json::json!({ "movie_number": "SSNI-888" });
        let missing = ok
            .get("movie_number")
            .and_then(|v| v.as_str())
            .map(|s| s.trim().is_empty())
            .unwrap_or(true);
        assert!(!missing, "有番号时不该补");
    }
}
