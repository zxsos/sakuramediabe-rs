//! 缩略图生成任务（上游 `playback/thumbnails/task_service.py`，509 行）。
//!
//! # 任务键 `media_thumbnail_generation`，cron **每 30 分钟**
//!
//! 缩略图生成要调 provider（可能跑 ffprobe、抽帧），慢且重。所以每半小时一轮，
//! 每次只处理「到期的」那些。
//!
//! # 三条独立的重试轨道
//!
//! ```text
//!   pending ──生成──> 成功（succeeded）
//!     │
//!     ├─ 源没就绪 ─> retry_wait（最多 3 次，15 分钟 × 次数，**线性**）─> 超限 -> terminal
//!     ├─ 真失败 ───> retry_wait（最多 2 次，15 分钟 × 次数，**线性**）─> 超限 -> terminal
//!     └─ 成功但数量不够 ─> 仍算成功（见 [`minimum_acceptable_count`]）
//! ```
//!
//! 两个计数器**互不干扰**：`thumbnail_deferred_count` 与
//! `thumbnail_attempt_count` 是 `media` 表上两个独立列。
//!
//! ⚠️ 混用会导致「延迟 3 次后被当成失败 3 次而终态」—— 一个只是盘没挂载的
//! 媒体会因此永远拿不到缩略图。
//!
//! # 候选集**包含 `succeeded`**，这一点最容易被漏掉
//!
//! [`MediaThumbnailTaskService::count_pending_media`] 不是「状态为 `pending`
//! 的数量」。上游 `_candidate_query` 是：
//!
//! ```text
//!   valid = true
//!   且 该媒体**一张缩略图都没有**
//!   且 (状态 ∈ {pending, succeeded} 或 状态 = retry_wait 且已到期)
//! ```
//!
//! 里面的 `succeeded` 是**修复路径**：状态机说「做完了」而产物不在（包被删、
//! 磁盘换了、写库成功而落盘失败）时，必须重新扫到它 —— 否则它永久停在
//! `succeeded` 而永远没有图。而「已经有缩略图」的行会被第 2 条挡掉，
//! 所以正常的成功媒体不会反复重做。
//!
//! # 「数量不足」是**成功**，不是失败
//!
//! [`minimum_acceptable_count`]：provider 返回 4 张、我们期望 5 张，只要 ≥ 下限
//! 就算成功。短片可能只有几个有效抽帧点，硬要求「等于期望数」会让它永远失败。
//!
//! ⚠️ 下限是 **期望数的 85%（向下取整，且至少 1）**。骨架期写的是 60% 且向上
//! 取整 —— 这条「看着合理」的数值会让**每一部短片都被判成失败**，而编译器
//! 永远抓不到。见 `handoff.md`「写 numerically plausible 的值比缺符号更危险」。

use sm_db::playback::media::thumbnail_state;
use sm_db::repo::MediaRepository;
use sm_db::Db;

use super::contracts::ThumbnailDeferred;
use crate::error::ServiceError;
use crate::system::config::ConfigService;

/// 任务键。与 `cron_spec::builtin_jobs` 里的 `media_thumbnail_generation` 一致。
pub const TASK_KEY: &str = "media_thumbnail_generation";

/// 普通失败的最大重试次数。
pub const MAX_FAILURE_ATTEMPTS: u32 = 2;
/// 源未就绪的最大延迟次数。
pub const MAX_DEFERRED_ATTEMPTS: u32 = 3;
/// 源未就绪的退避基数（秒）。
pub const DEFERRED_BACKOFF_BASE_SECONDS: i64 = 15 * 60;
/// 失败退避基数（秒）。
pub const FAILURE_RETRY_BACKOFF_BASE_SECONDS: i64 = 15 * 60;
/// 退避上限（秒）。**24 小时** —— 再久就等下一天了。
pub const FAILURE_RETRY_BACKOFF_MAX_SECONDS: i64 = 24 * 3600;

/// 终态错误码集合。这些**不再重试**。
///
/// # 这七个码与骨架不同，逐字照抄上游
///
/// | 骨架 | 处置 |
/// |---|---|
/// | 少了 `thumbnail_generation_empty` / `_insufficient_count` / `_unparseable_filenames` | **补上**：它们是上游**自己**在生成侧抛的码，漏掉会让这三类确定性失败被反复重试 |
/// | 多了 `provider_not_installed` | **删掉**：provider 没装时应当**继续重试** —— 用户装好插件后它就该成功。列进终态等于「装完也不会再试」 |
/// | 多了 `media_not_found` | **删掉**：上游没有。媒体没了是下一轮扫不到，不是这条任务该判的死罪（而且它连候选都进不来）|
///
/// ⚠️ 上游的 `is_terminal` 还有一个条件：`not exc.retryable`。本仓的错误类型没有
/// `retryable` 这个字段，所以**只有这个集合**在起作用 —— 也就是说这个集合
/// 漏一个码，那一类失败就永远不会进终态（而不是「少一次短路」）。
///
/// ⚠️ `thumbnail_artifact_path_invalid` 看起来该加（路径非法是确定性的），
/// 但**上游没有** —— 照抄。要改先改上游。
pub const TERMINAL_ERROR_CODES: [&str; 7] = [
    "thumbnail_generation_empty",
    "thumbnail_generation_insufficient_count",
    "thumbnail_generation_unparseable_filenames",
    "thumbnail_offset_invalid",
    "thumbnail_artifact_empty",
    "thumbnail_artifact_not_webp",
    "thumbnail_artifact_invalid",
];

/// 一次生成的结果。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ThumbnailGenerationOutcome {
    /// `success` / `deferred` / `failed` / `terminal`。
    pub state: String,
    pub generated_count: u32,
    /// 失败或延迟时的原因码。`state = "success"` 时为 `None`。
    pub error_code: Option<String>,
}

/// 下限：期望条数的 **85%**（**向下取整**，且至少 1）。
///
/// 上游 `max(1, int(expected_count * 0.85))` —— `int()` 是**截断**，不是四舍五入。
///
/// 纯函数。`expected_count = 0` 时返回 **1** 而不是 0 —— 0 意味着「任何数量都
/// 不够」，那会让「期望 0 张」的媒体永远无法成功。
pub fn minimum_acceptable_count(expected_count: u32) -> u32 {
    if expected_count == 0 {
        return 1;
    }
    // 先乘再除：`expected * 85 / 100` 与 `int(expected * 0.85)` 在整数域等价，
    // 而写成浮点会引入 `4.999999` 这类边界（`int(5*0.85)` 在 IEEE754 下是 4，
    // 但同样的写法换个值就可能差 1）。
    (expected_count * 85 / 100).max(1)
}

/// 失败退避秒数：`min(基数 × 次数, 上限)`。
///
/// # 是**线性**，不是指数
///
/// 上游 `min(FAILURE_RETRY_BACKOFF_BASE_SECONDS * attempt_count, MAX)` —— 基数
/// 乘**次数**。骨架期写的是 `base << attempt`（指数）：0→900、1→1800 看着一样，
/// 从第 3 次起就分道扬镳（指数 3600 vs 线性 1800）。
///
/// ⚠️ `attempt_count` 是**本次尝试之后**的新计数（首次失败传 **1**），与
/// [`classify`] 收的「尝试前计数」不是同一个口径 —— 上游也是两处不同
/// （`_mark_failure` 先 `+1` 再算退避）。
pub fn failure_backoff_seconds(attempt_count: u32) -> i64 {
    let grown = FAILURE_RETRY_BACKOFF_BASE_SECONDS.saturating_mul(i64::from(attempt_count));
    grown.min(FAILURE_RETRY_BACKOFF_MAX_SECONDS)
}

/// 延迟退避秒数：`min(基数 × 次数, 上限)`。同 [`failure_backoff_seconds`]。
///
/// 基数来自 provider 抛出的 [`ThumbnailDeferred`]（不同 provider 的「源没就绪」
/// 恢复速度不同），上限共用 24 小时。
pub fn deferred_backoff_seconds(base_seconds: i64, deferred_count: u32) -> i64 {
    base_seconds
        .saturating_mul(i64::from(deferred_count))
        .min(FAILURE_RETRY_BACKOFF_MAX_SECONDS)
}

// 插件接缝与相关类型在 [`super::provider_helpers`] —— **整个 playback 域只有
// 那一个**（`sm-service` 不能依赖 `sm-plugins`：依赖方向会成环）。这里转出来
// 是为了让本模块的使用点不必写长路径。
pub use crate::playback::provider_helpers::{
    json_or_null, ProviderFailure, StorageGateway, ThumbnailJobArtifact, ThumbnailJobResult,
};

/// 缩略图任务服务。
///
/// # 形状与骨架不同
///
/// 骨架是 `pub struct MediaThumbnailTaskService;` —— 无状态单元结构体，查询方法
/// 都是关联函数，拿不到仓储，于是全是 `todo!()`。
pub struct MediaThumbnailTaskService {
    db: Db,
    /// 图片根目录 —— 产物落盘时要算绝对路径。
    config: Option<ConfigService>,
    gateway: Option<std::sync::Arc<dyn StorageGateway>>,
}

impl MediaThumbnailTaskService {
    /// 构造。**查询类方法只需它。**
    pub fn new(db: &Db) -> Self {
        Self {
            db: db.clone(),
            config: None,
            gateway: None,
        }
    }

    /// 注入配置服务（产物落盘要用）。由组合根调用。
    pub fn with_config(mut self, config: &ConfigService) -> Self {
        self.config = Some(config.clone());
        self
    }

    /// 注入 provider 数据面的调用能力。由组合根调用。
    pub fn with_gateway(mut self, gateway: std::sync::Arc<dyn StorageGateway>) -> Self {
        self.gateway = Some(gateway);
        self
    }

    /// 待生成的媒体数（上游 `count_pending_media`）。**口径见模块文档**。
    pub async fn count_pending_media(&self) -> Result<i64, ServiceError> {
        Ok(MediaRepository::new(self.db.clone())
            .count_thumbnail_candidates()
            .await?)
    }

    /// 在退避等待中的媒体数。
    pub async fn count_retry_wait_media(&self) -> Result<i64, ServiceError> {
        Ok(MediaRepository::new(self.db.clone())
            .count_thumbnail_state(thumbnail_state::RETRY_WAIT)
            .await?)
    }

    /// 已进终态（放弃）的媒体数。
    pub async fn count_terminal_failed_media(&self) -> Result<i64, ServiceError> {
        Ok(MediaRepository::new(self.db.clone())
            .count_thumbnail_state(thumbnail_state::TERMINAL)
            .await?)
    }

    /// 把指定媒体从终态**放回**待处理。返回受影响行数。
    ///
    /// 供「人工重试」用 —— 终态意味着自动重试已放弃，但用户换了个网络环境
    /// 之后可能就想重试了。
    ///
    /// 返回的是**真正被改的行数**：不满足条件的（没进终态、无效、已有产物）
    /// 会被跳过。调用方把它当「重置了几部」显示给用户 ——
    /// 报「请求了 10 部」而不报「重置了 3 部」会让人以为剩下 7 部也好了。
    pub async fn reset_terminal_media(&self, media_ids: &[i32]) -> Result<u64, ServiceError> {
        Ok(MediaRepository::new(self.db.clone())
            .reset_terminal_thumbnails(media_ids)
            .await?)
    }

    /// ★ 生成一轮。任务执行体。
    ///
    /// 逐个到期媒体：调 provider 生成 -> 校验产物 -> 落盘登记。
    ///
    /// 单个媒体失败/延迟**不中断**整批 —— 一个坏媒体不该让整轮 500 个白跑。
    ///
    /// 一轮最多处理多少部。**队列深度由它控制**，与 `updated_at` 退避一起决定
    /// 一轮的耗时上限。
    pub const BATCH_LIMIT: i64 = 200;

    /// 生成一轮。
    ///
    /// 三段（上游 `_generate_pending_thumbnails` / `_generate_one` /
    /// `_generate_artifacts`）：
    ///
    /// ```text
    ///   1. 取候选（与 count_pending_media 同一条件）
    ///   2. 逐条加**媒体级锁**后生成 —— 锁不到就跳过（另一轮/另一个进程在动它）
    ///   3. classify 分流 -> 写 media 状态机；产物经 artifacts::persist 落盘
    /// ```
    ///
    /// # 单个媒体失败**不中断**整批
    ///
    /// 一个坏媒体不该让整轮几百部白跑。失败只记进计数与 `failed_media_ids`。
    ///
    /// # 没有注入 generator 时**直接返回空结果**
    ///
    /// 不假造产物 —— 假造会让客户端拿到打不开的图，而状态机以为成功了。
    /// 见 `StorageGateway` 的说明（`sm-service` 不能依赖 `sm-plugins`）。
    pub async fn generate_pending_thumbnails(&self) -> Result<serde_json::Value, ServiceError> {
        let mut stats = RoundStats::default();
        let Some(generator) = self.gateway.as_deref() else {
            // 没有 provider：整轮跳过，但**不要把状态机写成失败** ——
            // 那会让「插件还没装」变成「每部媒体都失败两次后进终态」。
            stats.skipped_no_provider = true;
            return Ok(stats.to_value());
        };

        let media_repo = MediaRepository::new(self.db.clone());
        let candidates = media_repo
            .list_thumbnail_candidates(Self::BATCH_LIMIT)
            .await?;
        stats.pending_media = candidates.len();

        for (media_id, library_id, provider_key) in candidates {
            // 媒体级锁：同一条媒体同时只能有一个生成在跑。
            // 锁不到（另一个进程/另一轮正在动它）**不算失败** —— 跳过即可，
            // 下一轮还会扫到它（它仍在候选里：没产物 + 状态未终态）。
            let attempt = sm_db::common::advisory_lock::AdvisoryLock::try_acquire(
                &self.db,
                sm_db::common::advisory_lock::namespace::MEDIA,
                media_id,
            )
            .await;
            let _guard = match attempt {
                Ok(Some(guard)) => guard,
                Ok(None) | Err(_) => {
                    stats.skipped += 1;
                    continue;
                }
            };

            let outcome = self
                .generate_one(generator, &media_repo, media_id, library_id, &provider_key)
                .await;
            match outcome {
                Ok(generated) => {
                    stats.successful_media += 1;
                    stats.generated_thumbnails += generated;
                }
                Err(RoundFailure::Skipped) => stats.skipped += 1,
                Err(RoundFailure::Failed(code)) => {
                    stats.retryable_failed_media += 1;
                    stats.failed_media_ids.push(media_id);
                    let _ = code;
                }
                Err(RoundFailure::Deferred(code)) => {
                    stats.deferred_media += 1;
                    let _ = code;
                }
                Err(RoundFailure::Terminal(code)) => {
                    stats.terminal_failed_media += 1;
                    stats.terminal_failed_media_ids.push(media_id);
                    let _ = code;
                }
                Err(RoundFailure::Db(detail)) => {
                    // 宿主自己的问题。**不进任何一条轨道** —— 把它算成 provider
                    // 失败会让「数据库抖了一下」变成「这部片子的缩略图失败了」。
                    stats.db_errors += 1;
                    tracing::warn!(media_id, %detail, "处理该媒体时数据库出错，已跳过");
                }
            }
        }

        Ok(stats.to_value())
    }

    /// 处理一条媒体。**锁由调用方持有。**
    async fn generate_one(
        &self,
        gateway: &dyn StorageGateway,
        media_repo: &MediaRepository,
        media_id: i32,
        library_id: i32,
        provider_key: &str,
    ) -> Result<u32, RoundFailure> {
        let media = media_repo
            .find_by_id(media_id)
            .await?
            .ok_or(RoundFailure::Skipped)?;

        // 宿主提供的临时工作目录。provider 把产物写进来，`persist` 随后搬走。
        let workspace = std::env::temp_dir().join(format!(
            "media-thumbnails-{media_id}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&workspace).map_err(|_| RoundFailure::Skipped)?;

        // 句柄要带**库的** provider_config —— 所以候选行里的 `library_id` 不是
        // 摆设，还得回查一次库记录（`MediaRecord.provider_config` 的注释原话）。
        let library = sm_db::repo::MediaLibraryRepository::new(self.db.clone())
            .find_by_id(library_id)
            .await?
            .ok_or(RoundFailure::Skipped)?;
        if library.provider_key != provider_key {
            // 候选行的 provider_key 来自 `media_library`，库记录的也是 ——
            // 不一致说明有人改了库却没重扫，这时候**以库记录为准**（句柄是照它发的）。
            tracing::warn!(
                media_id,
                candidate = %provider_key,
                library = %library.provider_key,
                "候选行的 provider_key 与库记录不一致，以库记录为准"
            );
        }
        let handle = crate::playback::provider_helpers::media_handle_for(
            &crate::playback::provider_helpers::MediaRecord {
                id: i64::from(media.id),
                library_id: i64::from(media.library_id),
                storage_ref: json_or_null(media.storage_ref.as_deref()),
                provider_config: json_or_null(library.provider_config.as_deref()),
                provider_key: library.provider_key.clone(),
                account_key: library.account_key.clone(),
                // 这两个**不是装饰**：`plugin-ref-local` 用 `duration_seconds`
                // 决定生成几张、用 `file_name` 决定产物叫什么。
                // 骨架期把它们落在 `ThumbnailJobRequest` 上，合并进
                // `MediaHandle` 时漏过一次 —— 表现是「provider 报成功但 0 张」。
                file_name: media.file_name.clone(),
                file_size_bytes: media.file_size_bytes,
                duration_seconds: media.duration_seconds,
            },
        );

        let attempt_count = u32::try_from(media.thumbnail_attempt_count).unwrap_or(0);
        let deferred_count = u32::try_from(media.thumbnail_deferred_count).unwrap_or(0);

        let (result, failure) = match gateway.generate_thumbnails(&handle, &workspace).await {
            Ok(result) => (Some(result), None),
            Err(failure) => (None, Some(failure)),
        };

        // 产物校验 + 落盘。
        let (generated_count, expected_count, first_error) = match result {
            Some(result) => {
                let (valid, error) = validate_artifacts(&workspace, &result);
                // 先落盘再写状态机：`persist` 失败时状态机不能说成功。
                match self.persist_validated(&media, &valid).await {
                    Ok(count) => (count, result.expected_count, error),
                    Err(_) => (0, result.expected_count, error),
                }
            }
            None => (0, 0, None),
        };

        let outcome = match failure {
            Some(failure) => {
                // 延迟轨：`unavailable` 且 provider 说可以重试。
                // 上游 `_mark_failure` 判的是 `exc.retryable` + `code == unavailable`。
                if failure.retryable && failure.code == "unavailable" {
                    // `error_code` 用**默认那个**而不是 provider 的码 ——
                    // 它落进 `thumbnail_last_error_code`，任务中心靠它显示
                    // 「稍后重试」而不是「失败」（`contracts::THUMBNAIL_SOURCE_DEFERRED`
                    // 的注释原话）。
                    let deferred = ThumbnailDeferred::new(
                        &failure.safe_message,
                        MAX_DEFERRED_ATTEMPTS,
                        DEFERRED_BACKOFF_BASE_SECONDS,
                    )
                    .unwrap_or_else(|_| ThumbnailDeferred {
                        message: failure.safe_message.clone(),
                        error_code: super::contracts::THUMBNAIL_SOURCE_DEFERRED.to_owned(),
                        max_deferred_attempts: MAX_DEFERRED_ATTEMPTS,
                        deferred_backoff_base_seconds: DEFERRED_BACKOFF_BASE_SECONDS,
                    });
                    classify(
                        0,
                        expected_count,
                        Some(&deferred),
                        attempt_count,
                        deferred_count,
                        Some(&failure.code),
                    )
                } else {
                    classify(
                        generated_count,
                        expected_count,
                        None,
                        attempt_count,
                        deferred_count,
                        Some(&failure.code),
                    )
                }
            }
            None => classify(
                generated_count,
                expected_count,
                None,
                attempt_count,
                deferred_count,
                first_error.as_deref(),
            ),
        };

        // 清理临时目录 —— 产物已经搬进正式位置了。
        std::fs::remove_dir_all(&workspace).ok();

        self.write_outcome(media_repo, media_id, &outcome, deferred_count)
            .await?;
        Ok(generated_count)
    }

    /// 把 [`classify`] 的结果写进 `media` 的状态机。
    ///
    /// `deferred_count` 是**尝试前**的延迟计数（算退避用）。
    async fn write_outcome(
        &self,
        media_repo: &MediaRepository,
        media_id: i32,
        outcome: &ThumbnailGenerationOutcome,
        deferred_count: u32,
    ) -> Result<(), RoundFailure> {
        let code = outcome.error_code.clone().unwrap_or_default();
        match outcome.state.as_str() {
            "success" => {
                media_repo.record_thumbnail_success(media_id).await?;
                Ok(())
            }
            "deferred" => {
                // 退避按**新的**延迟计数算（`classify` 收的是尝试前的值）。
                let next = sm_db::common::time::now_utc()
                    + chrono::Duration::seconds(deferred_backoff_seconds(
                        DEFERRED_BACKOFF_BASE_SECONDS,
                        deferred_count + 1,
                    ));
                media_repo
                    .record_thumbnail_deferred(media_id, &code, next)
                    .await?;
                Err(RoundFailure::Deferred(code))
            }
            "failed" => {
                let next = sm_db::common::time::now_utc()
                    + chrono::Duration::seconds(failure_backoff_seconds(1));
                media_repo
                    .record_thumbnail_failure(media_id, &code, next)
                    .await?;
                Err(RoundFailure::Failed(code))
            }
            _ => {
                media_repo
                    .record_thumbnail_terminal(media_id, &code)
                    .await?;
                Err(RoundFailure::Terminal(code))
            }
        }
    }

    /// 落盘已校验的产物。
    async fn persist_validated(
        &self,
        media: &sm_db::Media,
        valid: &[(super::artifacts::ThumbnailArtifact, std::path::PathBuf)],
    ) -> Result<u32, ServiceError> {
        let Some(config) = self.config.as_ref() else {
            // 组合根忘了注入配置。**不做「用默认值凑一个」** —— 那会把产物写到
            // 一个谁也想不到的目录，而状态机随后还会标记成功。
            return Err(ServiceError::from(sm_db::DbError::business(
                "ThumbnailTask",
                "未注入配置服务：算不出图片根目录，产物无法落盘",
            )));
        };
        super::artifacts::ThumbnailArtifactService::new(&self.db, config)
            .persist(media, valid)
            .await
    }
}

/// 一轮的统计。**与上游返回的那份 dict 对齐**（键名一致，便于任务中心复用）。
#[derive(Debug, Clone, Default)]
pub struct RoundStats {
    pub pending_media: usize,
    pub successful_media: usize,
    pub generated_thumbnails: u32,
    pub deferred_media: usize,
    pub retryable_failed_media: usize,
    pub terminal_failed_media: usize,
    pub skipped: usize,
    /// 宿主侧数据库出错的条数。**与 provider 失败分开计。**
    pub db_errors: usize,
    pub failed_media_ids: Vec<i32>,
    pub terminal_failed_media_ids: Vec<i32>,
    /// **没有注入 provider** 导致整轮未跑。
    ///
    /// 与 `pending_media == 0` 是两回事：前者是「没装插件」，后者是「队列空」。
    /// 混在一起会让运维看不出「任务其实一次都没跑」。
    pub skipped_no_provider: bool,
}

impl RoundStats {
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "pending_media": self.pending_media,
            "successful_media": self.successful_media,
            "generated_thumbnails": self.generated_thumbnails,
            "deferred_media": self.deferred_media,
            "retryable_failed_media": self.retryable_failed_media,
            "terminal_failed_media": self.terminal_failed_media,
            "skipped_media": self.skipped,
            "failed_media_ids": self.failed_media_ids,
            "terminal_failed_media_ids": self.terminal_failed_media_ids,
            "skipped_no_provider": self.skipped_no_provider,
        })
    }
}

/// 一条媒体的处置结果。**只用于这一轮内部** —— 它不是对外契约
/// （对外的契约是 `media` 表上的状态机列）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RoundFailure {
    /// 锁不到 / 媒体没了 / 工作目录建不出来。**不计入失败**。
    Skipped,
    /// 进了失败轨（还有重试额度）。
    Failed(String),
    /// 进了延迟轨（源没就绪）。
    Deferred(String),
    /// 进了终态。
    Terminal(String),
    /// 数据库出问题。**不计入任何一条轨道** —— 它是宿主的问题，不是 provider 的。
    Db(String),
}

impl From<sm_db::DbError> for RoundFailure {
    fn from(error: sm_db::DbError) -> Self {
        Self::Db(error.to_string())
    }
}

// 见 `provider_helpers::json_or_null` 的文档：宿主**不解释**这两个字段的内容，
// 解析不出来就原样传 `Null`，让 provider 自己处置。

/// 校验一组产物，返回「可用的那些」与第一条校验错误码。
///
/// 上游 `_generate_artifacts` 里这一段做了三件事，这里照做：
///
/// 1. **同一偏移去重**（provider 可能重复报同一帧）；
/// 2. 逐件走 [`artifacts::ThumbnailArtifactService::validate_artifact`]
///    （路径在 workspace 内 / 是 WebP / 非空 / 偏移非负）；
/// 3. 校验不过的**跳过**而不是整条失败 —— 一张坏图不该让整部片子拿不到缩略图。
///
/// 返回的第一条错误码只在「一张都没通过」时才有意义（那时要进失败轨）。
fn validate_artifacts(
    workspace: &std::path::Path,
    result: &ThumbnailJobResult,
) -> (
    Vec<(super::artifacts::ThumbnailArtifact, std::path::PathBuf)>,
    Option<String>,
) {
    let mut valid = Vec::new();
    let mut seen_offsets = std::collections::HashSet::new();
    let mut first_error: Option<String> = None;

    for artifact in &result.artifacts {
        if !seen_offsets.insert(artifact.offset_seconds) {
            continue;
        }
        let candidate = super::artifacts::ThumbnailArtifact {
            thumbnail_id: 0,
            offset_seconds: i64::from(artifact.offset_seconds),
            path: workspace.join(&artifact.relative_path),
        };
        match super::artifacts::ThumbnailArtifactService::validate_artifact(workspace, &candidate) {
            Ok(source) => valid.push((candidate, source)),
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(error.to_string());
                }
            }
        }
    }
    (valid, first_error)
}

/// 判定一次生成的结果该落成什么状态。**纯函数** —— 三条轨道的分流都在这里。
///
/// ```text
/// generated_count >= minimum_acceptable_count(expected) -> success
/// error_code 在 TERMINAL_ERROR_CODES 里                    -> terminal（不等重试次数）
/// 源未就绪 且 deferred_count + 1 > max_deferred_attempts    -> terminal
/// 源未就绪                                                 -> deferred
/// 真失败 且 attempt_count + 1 >= MAX_FAILURE_ATTEMPTS       -> terminal
/// 真失败                                                   -> failed
/// ```
///
/// # `attempt_count` / `deferred_count` 是**尝试前**的计数
///
/// 也就是 `media.thumbnail_*_count` 的当前值（上游 `int(media.... or 0)`）。
/// 上游在 `_mark_*` 里先 `+1` 再判上限，所以：
///
/// - 失败轨：`new = old + 1 >= 2` → 第 **2** 次失败即终态（`old = 1`）；
/// - 延迟轨：`new = old + 1 > 3` → 第 **4** 次延迟才终态（`old = 3`）。
///
/// ⚠️ 骨架期失败轨写的是 `old + 1 > 2`，等于**多送一次重试**（3 次才终态）。
/// 两个轨道的边界本来就不同，写反了只会表现为「重试次数比预期多一次」——
/// 不会有任何报错。
pub fn classify(
    generated_count: u32,
    expected_count: u32,
    deferred: Option<&ThumbnailDeferred>,
    attempt_count: u32,
    deferred_count: u32,
    error_code: Option<&str>,
) -> ThumbnailGenerationOutcome {
    if generated_count >= minimum_acceptable_count(expected_count) {
        return ThumbnailGenerationOutcome {
            state: "success".to_owned(),
            generated_count,
            error_code: None,
        };
    }
    // 终态码**先于**轨道判定：一个确定性失败不该因为「还剩重试额度」而重试。
    if error_code.is_some_and(|code| TERMINAL_ERROR_CODES.contains(&code)) {
        return ThumbnailGenerationOutcome {
            state: "terminal".to_owned(),
            generated_count,
            error_code: error_code.map(str::to_owned),
        };
    }
    // 轨道由 `deferred` 是否为 `Some` 决定 —— 两个计数互不干扰。
    if let Some(deferred_err) = deferred {
        let state = if deferred_count + 1 > deferred_err.max_deferred_attempts {
            "terminal"
        } else {
            "deferred"
        };
        return ThumbnailGenerationOutcome {
            state: state.to_owned(),
            generated_count,
            error_code: Some(deferred_err.error_code.clone()),
        };
    }
    let state = if attempt_count + 1 >= MAX_FAILURE_ATTEMPTS {
        "terminal"
    } else {
        "failed"
    };
    ThumbnailGenerationOutcome {
        state: state.to_owned(),
        generated_count,
        error_code: error_code.map(str::to_owned),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 三条轨道的上限互不相同，别弄混。
    #[test]
    fn the_three_tracks_have_different_limits() {
        assert_eq!(MAX_DEFERRED_ATTEMPTS, 3);
        assert_eq!(MAX_FAILURE_ATTEMPTS, 2);
        // 两个计数列是独立的 —— 混用会让「盘没挂载」变成「失败两次后终态」。
        assert_ne!(MAX_DEFERRED_ATTEMPTS, MAX_FAILURE_ATTEMPTS);
    }

    /// ★ 下限是期望数的 **85%**（**向下取整**），不是 60%。
    ///
    /// 这条是「写了一个看着合理的数值」的典型：60% 会让每一部短片都被判失败，
    /// 而编译器永远抓不到。期望值取自上游 `max(1, int(expected * 0.85))`。
    #[test]
    fn the_minimum_acceptable_count_is_eighty_five_percent() {
        assert_eq!(minimum_acceptable_count(0), 1);
        assert_eq!(
            minimum_acceptable_count(1),
            1,
            "0.85 截断成 0，但下限至少 1"
        );
        assert_eq!(minimum_acceptable_count(5), 4, "int(4.25) = 4，向下取整");
        assert_eq!(minimum_acceptable_count(10), 8);
        assert_eq!(minimum_acceptable_count(20), 17, "int(17.0) = 17");
        assert_eq!(minimum_acceptable_count(100), 85);
    }

    /// 数量不足时仍算**成功**（达到下限即成功）。
    #[test]
    fn a_partial_result_still_counts_as_success() {
        // 期望 5、下限 4：4 张就够。
        assert_eq!(classify(4, 5, None, 0, 0, None).state, "success");
        // 3 张不够（< 4），且没有错误码 → 走失败轨。
        assert_eq!(classify(3, 5, None, 0, 0, None).state, "failed");
    }

    /// 期望 0 张时下限是 **1** 而不是 0 —— 否则永远无法成功。
    #[test]
    fn zero_expected_still_requires_one() {
        assert_eq!(minimum_acceptable_count(0), 1);
    }

    /// 延迟轨与失败轨**互不干扰**：延迟 2 次后仍是 deferred，不是 terminal。
    #[test]
    fn deferred_attempts_do_not_consume_the_failure_budget() {
        let deferred = ThumbnailDeferred::new("盘没挂载", 3, 900).expect("参数合法");
        for deferred_count in 0..3 {
            let outcome = classify(
                0,
                5,
                Some(&deferred),
                /*attempt_count=*/ 99,
                deferred_count,
                None,
            );
            assert_eq!(
                outcome.state, "deferred",
                "延迟 {deferred_count} 次后不该变终态"
            );
        }
        // 第 4 次（旧的 deferred_count = 3）才终态。
        assert_eq!(
            classify(0, 5, Some(&deferred), 99, 3, None).state,
            "terminal"
        );
    }

    /// ★ 失败轨：**第 2 次失败**即终态（旧的 `attempt_count = 1`）。
    ///
    /// 骨架期写成 `old + 1 > 2`，等于多送一次重试（第 3 次才终态）。
    #[test]
    fn failure_reaches_terminal_at_the_second_attempt() {
        assert_eq!(classify(0, 5, None, 0, 0, Some("boom")).state, "failed");
        assert_eq!(classify(0, 5, None, 1, 0, Some("boom")).state, "terminal");
    }

    /// 终态错误码**立刻**终态，不等重试次数。
    #[test]
    fn terminal_error_codes_skip_retry_entirely() {
        for code in TERMINAL_ERROR_CODES {
            let outcome = classify(0, 5, None, 0, 0, Some(code));
            assert_eq!(outcome.state, "terminal", "{code} 应直接终态");
        }
    }

    /// ★ `provider_not_installed` **不在**终态集合里。
    ///
    /// 装好插件后它就该成功 —— 列进终态等于「装完也不会再试」。骨架期把它
    /// 列了进去（还有上游没有的 `media_not_found`）。
    #[test]
    fn an_uninstalled_provider_is_not_terminal() {
        assert!(!TERMINAL_ERROR_CODES.contains(&"provider_not_installed"));
        assert!(!TERMINAL_ERROR_CODES.contains(&"media_not_found"));
        assert_eq!(
            classify(0, 5, None, 0, 0, Some("provider_not_installed")).state,
            "failed",
            "它应当按普通失败重试"
        );
    }

    /// ★ 退避是**线性**的（基数 × 次数），不是指数。
    #[test]
    fn failure_backoff_is_linear_then_capped() {
        assert_eq!(failure_backoff_seconds(1), 900, "首次失败 = 基数 × 1");
        assert_eq!(failure_backoff_seconds(2), 1800, "第二次 = 基数 × 2");
        // 指数版本在这里会给 3600 —— 差一倍。
        assert_eq!(failure_backoff_seconds(3), 2700);
        assert_eq!(
            failure_backoff_seconds(1000),
            FAILURE_RETRY_BACKOFF_MAX_SECONDS,
            "线性增长必须封顶，否则一次失败能退避几个月"
        );
    }

    /// 延迟轨的退避用自己的基数（provider 给的），上限共用 24 小时。
    #[test]
    fn deferred_backoff_uses_the_provider_base() {
        assert_eq!(deferred_backoff_seconds(900, 1), 900);
        assert_eq!(deferred_backoff_seconds(900, 3), 2700);
        assert_eq!(deferred_backoff_seconds(60, 2), 120);
        assert_eq!(
            deferred_backoff_seconds(3600, 100),
            FAILURE_RETRY_BACKOFF_MAX_SECONDS
        );
    }

    /// 终态集合恰好七项，且都来自上游。
    #[test]
    fn the_terminal_set_matches_upstream() {
        assert_eq!(TERMINAL_ERROR_CODES.len(), 7);
        for code in [
            "thumbnail_generation_empty",
            "thumbnail_generation_insufficient_count",
            "thumbnail_generation_unparseable_filenames",
            "thumbnail_offset_invalid",
            "thumbnail_artifact_empty",
            "thumbnail_artifact_not_webp",
            "thumbnail_artifact_invalid",
        ] {
            assert!(TERMINAL_ERROR_CODES.contains(&code), "{code} 漏了");
        }
    }
}
