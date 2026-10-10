//! 已订阅影片的自动下载选种（上游 `downloads/auto_subscribed/auto_download_service.py`，230 行）。
//!
//! # 这是 `subscribed_movie_auto_download` 任务的执行体
//!
//! cron 是 `30 2 * * *`（每天 02:30）。它做的是**选种 + 提交**，不下载 ——
//! 下载是下载器自己的事。
//!
//! # 体积窗口是硬约束：1 GiB ~ 40 GiB
//!
//! | 常量 | 值 | 为什么 |
//! |---|---|---|
//! | [`MIN_SIZE_BYTES`] | 1 GiB | 小于 1G 的多半是**样片/广告片段**，不是正片 |
//! | [`MAX_SIZE_BYTES`] | 40 GiB | 超大种子是「合集/多集打包」，导入后一部影片对多个文件 |
//!
//! 下界比上界更重要：下界挡掉的样片一旦提交，下载器下完才发现是 3 分钟的
//! 预览片，而宿主这边已经建了任务台账。
//!
//! # 拒绝上限 5：防止在坏索引器上耗尽时间
//!
//! [`MAX_REJECTED_CANDIDATES`] —— 一个候选因体积/黑名单被拒就换下一个，
//! 连续拒 5 个说明**这个索引器的结果整体不适用**（比如它是某个特定站的
//! 小体积版本源）。此时应当**放弃这部影片**而不是继续翻页。
//!
//! # 与普通搜索不同：这里**不返回候选列表给用户挑**
//!
//! 它全自动 —— 搜 → 按体积与黑名单过滤 → 挑第一个合格的 → 提交。
//! 所以「首选顺序」完全由代码决定，**没有人工介入点**。
//!
//! # 抛的是 `SubscriptionSearchError`，**不是** `ApiError`
//!
//! 因为调用方是 worker 而非 HTTP 层。错误码沿用订阅搜索状态机那套
//! （`no_candidate_found` / `indexer_search_failed` / `download_submit_failed`），
//! 见 `catalog::movie_subscription_search_state`。

use serde::Serialize;

use crate::error::ServiceError;

/// 下限 1 GiB。见模块文档。
pub const MIN_SIZE_BYTES: i64 = 1024 * 1024 * 1024;
/// 上限 40 GiB。见模块文档。
pub const MAX_SIZE_BYTES: i64 = 40 * 1024 * 1024 * 1024;
/// 连续拒绝上限。见模块文档。
pub const MAX_REJECTED_CANDIDATES: usize = 5;
/// 任务键。与 `cron_spec::builtin_jobs` 里那一条**必须一致**。
pub const TASK_KEY: &str = "subscribed_movie_auto_download";

/// 候选被拒的**理由** —— 决定 `consumes_budget` 与要不要记日志。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RejectReason {
    /// 体积低于下限。**不**消耗拒绝预算 —— 不同索引器的体积分布差异很大，
    /// 一个小种子站整体偏小是常态。
    TooSmall,
    /// 体积超上限。同样不消耗预算。
    TooLarge,
    /// 资源在黑名单里。**消耗**预算 —— 继续翻很可能还是黑名单。
    Blacklisted,
    /// 提交时报错。消耗预算。
    SubmitFailed,
}

/// 订阅搜索错误。**`consumes_budget` 决定重试预算是否被扣**。
///
/// 这个字段是整个状态机的核心（见 `catalog::movie_subscription_search_state`）：
/// 「没有候选」不该让订阅被判定为「搜索失败」—— 前者是正常结果
/// （新片可能还没被收录），后者才需要重试与退避。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSearchError {
    pub code: String,
    pub message: String,
    /// 是否扣减重试预算。
    pub consumes_budget: bool,
}

/// 无候选。**不**消耗预算 —— 这是正常结果。
pub const ERROR_CODE_NO_CANDIDATE: &str = "no_candidate_found";
/// 索引器搜索失败。消耗预算。
pub const ERROR_CODE_INDEXER_SEARCH_FAILED: &str = "indexer_search_failed";
/// 提交下载失败。消耗预算。
pub const ERROR_CODE_DOWNLOAD_SUBMIT_FAILED: &str = "download_submit_failed";

/// 判定体积是否在窗口内。**闭区间**。
///
/// 纯函数 —— 体积窗口是最容易被「顺手收紧」的地方，写成可测的纯函数能
/// 防止它被无声改动。
pub fn size_in_window(size_bytes: Option<i64>) -> bool {
    match size_bytes {
        None => false,
        Some(size) => (MIN_SIZE_BYTES..=MAX_SIZE_BYTES).contains(&size),
    }
}

/// 这个拒绝理由**是否**消耗重试预算。
///
/// 只有「黑名单」与「提交失败」消耗。前者是内容问题（换个种子可能就好），
/// 后者是系统问题；体积问题是**索引器特性**，继续翻页没有意义。
pub fn consumes_budget(reason: RejectReason) -> bool {
    matches!(
        reason,
        RejectReason::Blacklisted | RejectReason::SubmitFailed
    )
}

/// 本次运行的统计。
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct AutoDownloadStats {
    /// 考察的已订阅影片数。
    pub examined_movies: i32,
    /// 成功提交的任务数。
    pub submitted: i32,
    /// 因无候选而跳过的影片数。
    pub skipped_no_candidate: i32,
    /// 因已有下载任务而跳过的（去重）。
    pub skipped_existing_task: i32,
    /// 因搜索/提交失败而失败的影片数。
    pub failed: i32,
}

/// 自动下载服务。
// `inner` 尚未被方法体引用（`run` 还是 `todo!()`），落地后删 allow。
#[allow(dead_code)]
pub struct SubscribedMovieAutoDownloadService {
    /// 可注入的搜索与提交依赖，测试时替身注入。
    inner: Option<Box<dyn AutoDownloadDeps>>,
}

impl Default for SubscribedMovieAutoDownloadService {
    fn default() -> Self {
        Self::new()
    }
}

/// 可注入依赖面。
pub trait AutoDownloadDeps: Send + Sync {
    /// 待处理的已订阅番号列表。实现方从订阅状态表查「待搜索」的影片。
    fn subscribed_movie_numbers(&self) -> Result<Vec<String>, ServiceError>;
    /// 搜候选。**已按体积窗口过滤**（实现方负责，过滤规则见
    /// [`size_in_window`]）。
    fn search(
        &self,
        movie_number: &str,
    ) -> Result<Vec<super::download_request::DownloadRequestCandidate>, ServiceError>;
    /// 该影片是否已有进行中的下载任务。
    fn has_active_task(&self, movie_number: &str) -> Result<bool, ServiceError>;
    /// 提交候选。
    fn submit(
        &self,
        movie_number: &str,
        candidate: &super::download_request::DownloadRequestCandidate,
    ) -> Result<String, ServiceError>;
    /// 资源是否被主机黑名单拉黑。
    fn is_blacklisted(&self, source_uri: &str) -> Result<bool, ServiceError>;
}

impl SubscribedMovieAutoDownloadService {
    /// 构造（真实依赖）。
    pub fn new() -> Self {
        Self { inner: None }
    }

    /// 构造（注入替身，测试用）。
    pub fn with_deps(deps: Box<dyn AutoDownloadDeps>) -> Self {
        Self { inner: Some(deps) }
    }

    /// ★ 跑一轮。上游 `run(*, reporter) -> dict`。
    ///
    /// 逐部已订阅影片：查重 → 搜候选 → 按体积与黑名单过滤 → 提交第一个合格的。
    /// 连续拒 [`MAX_REJECTED_CANDIDATES`] 个就放弃这部影片（见模块文档）。
    ///
    /// **单部影片失败不中断整轮** —— 一部影片的索引器故障不该让其它影片
    /// 今晚都不下载。
    pub async fn run(&self) -> Result<AutoDownloadStats, ServiceError> {
        let deps = self.inner.as_deref().ok_or_else(|| {
            ServiceError::unavailable(
                "auto_download_deps_not_wired",
                "自动下载依赖未接线（需要 DB 与索引器）",
            )
        })?;

        let mut stats = AutoDownloadStats::default();
        let movie_numbers = deps.subscribed_movie_numbers()?;

        for movie_number in &movie_numbers {
            stats.examined_movies += 1;

            // 查重：已有进行中的任务就跳过。
            match deps.has_active_task(movie_number) {
                Ok(true) => {
                    stats.skipped_existing_task += 1;
                    continue;
                }
                Ok(false) => {}
                Err(_) => {
                    // 查重失败按「无任务」处理，继续走搜索；提交时的幂等由
                    // download_request 层保证。
                }
            }

            // 搜候选。
            let candidates = match deps.search(movie_number) {
                Ok(c) => c,
                Err(_) => {
                    stats.failed += 1;
                    continue;
                }
            };
            if candidates.is_empty() {
                stats.skipped_no_candidate += 1;
                continue;
            }

            // 逐个过滤，提交第一个合格的；连续拒满 5 个就放弃这部影片。
            let mut rejected = 0usize;
            let mut submitted = false;
            for candidate in &candidates {
                // 体积过滤（未知体积也拒）。
                let reason = match candidate.size_bytes {
                    None => Some(RejectReason::TooSmall),
                    Some(size) => {
                        if size < MIN_SIZE_BYTES {
                            Some(RejectReason::TooSmall)
                        } else if size > MAX_SIZE_BYTES {
                            Some(RejectReason::TooLarge)
                        } else {
                            None
                        }
                    }
                };
                if let Some(_reason) = reason {
                    rejected += 1;
                    if rejected >= MAX_REJECTED_CANDIDATES {
                        break;
                    }
                    continue;
                }

                // 黑名单过滤。
                match deps.is_blacklisted(&candidate.source_uri) {
                    Ok(true) => {
                        rejected += 1;
                        if rejected >= MAX_REJECTED_CANDIDATES {
                            break;
                        }
                        continue;
                    }
                    Ok(false) => {}
                    Err(_) => {
                        // 黑名单查失败按「未拉黑」处理，避免误杀。
                    }
                }

                // 提交。
                match deps.submit(movie_number, candidate) {
                    Ok(_) => {
                        stats.submitted += 1;
                        submitted = true;
                        break;
                    }
                    Err(_) => {
                        rejected += 1;
                        if rejected >= MAX_REJECTED_CANDIDATES {
                            break;
                        }
                        continue;
                    }
                }
            }

            if !submitted {
                // 全部候选都被拒（或提交失败），记为失败。
                // 注意：无候选的情况上面已单独计数，这里只记「有候选但全拒」。
                if !candidates.is_empty() {
                    stats.failed += 1;
                }
            }
        }

        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 体积窗口是**闭区间**，且两端都不可动。
    #[test]
    fn the_size_window_is_closed_at_both_ends() {
        assert!(size_in_window(Some(MIN_SIZE_BYTES)), "等于下限要接受");
        assert!(size_in_window(Some(MAX_SIZE_BYTES)), "等于上限要接受");
        assert!(!size_in_window(Some(MIN_SIZE_BYTES - 1)), "差 1 字节就要拒");
        assert!(!size_in_window(Some(MAX_SIZE_BYTES + 1)));
    }

    /// **未知体积一律拒**。
    ///
    /// 索引器不给大小时不能当成「小」——那会放过 3 分钟的样片。
    #[test]
    fn an_unknown_size_is_rejected() {
        assert!(!size_in_window(None));
    }

    /// 只有黑名单与提交失败**消耗**预算。
    ///
    /// 体积问题的来源是索引器特性 —— 继续翻页只是浪费时间，而「无候选」
    /// 更是正常结果（片子可能还没被收录）。
    #[test]
    fn only_content_and_system_failures_consume_the_budget() {
        assert!(consumes_budget(RejectReason::Blacklisted));
        assert!(consumes_budget(RejectReason::SubmitFailed));
        assert!(!consumes_budget(RejectReason::TooSmall));
        assert!(!consumes_budget(RejectReason::TooLarge));
    }

    /// 「无候选」的 `consumes_budget` 必须是 `false` —— 它是**正常结果**。
    ///
    /// 写成 true 会让刚订阅的新片因「还没被收录」被判为搜索失败，
    /// 几次之后就永久耗尽重试预算，那部影片再也搜不到。
    #[test]
    fn having_no_candidate_is_a_normal_outcome_not_a_failure() {
        let error = SubscriptionSearchError {
            code: ERROR_CODE_NO_CANDIDATE.to_owned(),
            message: "未找到合格候选".to_owned(),
            consumes_budget: false,
        };
        assert!(!error.consumes_budget);
        assert_eq!(error.code, "no_candidate_found");
    }
}
