//! 影片订阅搜索的状态与重试预算（上游
//! `catalog/movie_subscription_search_state_service.py`，136 行）。
//!
//! # 「订阅一部影片」是**长周期可重试**的过程
//!
//! 新片常常几天后才被收录，所以订阅不是一次查询，而是一个会被打断、会重试、
//! 会最终放弃的状态机。
//!
//! ```text
//!   pending ──begin_attempt──> running ──┬─ mark_succeeded ─> succeeded（终态）
//!      ↑                                 ├─ 可重试失败 ─> failed_retryable
//!      └──── reset / 预算耗尽 ───────────┴─ 预算耗尽 ─> exhausted（终态）
//!
//!   running ──（进程崩溃后 recover_interrupted_running）──> failed_retryable
//! ```
//!
//! ⚠️ 崩溃恢复落在 **`failed_retryable`**，不是 `pending`。两者对用户是
//! 两件事：「从没搜过」与「搜到一半被打断，等着重试」。
//!
//! # 支点：`consumes_budget` × 是否「新鲜」
//!
//! | 失败原因 | 消耗预算 |
//! |---|---|
//! | `no_candidate_found` | **否** —— 正常结果，新片可能还没被收录 |
//! | 索引器搜索失败 | 是（**且影片已过新鲜期**，见下） |
//! | 提交下载失败 | 同上 |
//!
//! ★ **两个条件缺一不可**：`mark_failed` 只在
//! 「`consumes_budget` **且** 不新鲜」时才 `+1`。刚上映的影片（`release_date`
//! 在过去 [`MovieSubscriptionSearchStateService::fresh_days`] 天内）**失败多少次
//! 都不扣预算** —— 它在等片源，惩罚它没有意义。
//!
//! **把「没找到」算成失败是最容易写错的地方**：刚订阅的新片因「还没人发片」
//! 被判失败 → 几次后预算耗尽 → 那部影片**永久不再搜索**，用户却毫不知情。
//!
//! # `failed_retryable` 与 `exhausted` 必须分开
//!
//! 「还在等」与「已放弃」合成一个状态，会让放弃看起来像还在等 —— 任务中心
//! 里永远转圈。
//!
//! # 刻意**不用**通用任务台账
//!
//! 它的粒度是**影片**，而任务台账是 `task_key`。借用会让「2000 部影片各搜
//! 一次」在任务中心显示成 2000 条任务。
//!
//! # `running` 会因进程崩溃卡死
//!
//! 所以启动时要跑 [`MovieSubscriptionSearchStateService::recover_interrupted_running`]，
//! 否则那些影片永远停在「正在搜索」而不再被领取 —— 候选判据把 `running`
//! 排除在外，于是它们**不会再被任何一轮搜到**，且没有任何报错。

use chrono::NaiveDateTime;
use sm_db::common::time::now_utc;
use sm_db::repo::MovieRepository;
use sm_db::Db;

use crate::error::ServiceError;
use crate::system::config::ConfigService;

/// 待处理（初始状态，也是重试落点）。
pub const STATE_PENDING: &str = "pending";
/// 正在搜索。
pub const STATE_RUNNING: &str = "running";
/// 成功（终态）。
pub const STATE_SUCCEEDED: &str = "succeeded";
/// 失败但可重试。
pub const STATE_FAILED_RETRYABLE: &str = "failed_retryable";
/// 预算耗尽（终态，已放弃）。
pub const STATE_EXHAUSTED: &str = "exhausted";

/// 「没找到候选」的错误码。
pub const ERROR_CODE_NO_CANDIDATE: &str = "no_candidate_found";
/// 崩溃中断的错误码（`recover_interrupted_running` 落库的那个）。
pub const ERROR_CODE_TASK_INTERRUPTED: &str = "task_interrupted";
/// 崩溃中断的固定文案（落库并显示给用户）。
///
/// 逐字取自上游 `:18`。改它会让**存量库里已有的文案**与新库不一致，
/// 而按文案过滤/展示的地方（任务中心）会同时看到两种。
pub const INTERRUPTED_ERROR_MESSAGE: &str = "订阅影片资源查询任务中断，等待重试";

/// 搜索错误。见模块文档的 `consumes_budget` × 新鲜度 表。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubscriptionSearchError {
    pub code: String,
    pub message: String,
    /// 是否扣减重试预算。**「没找到」必须是 `false`。**
    ///
    /// ⚠️ 它**不是**唯一条件：`mark_failed` 还要看影片是否新鲜（见模块文档）。
    pub consumes_budget: bool,
}

impl SubscriptionSearchError {
    /// 「没找到候选」—— **不**消耗预算。
    pub fn no_candidate() -> Self {
        Self {
            code: ERROR_CODE_NO_CANDIDATE.to_owned(),
            message: "未找到符合条件的资源".to_owned(),
            consumes_budget: false,
        }
    }

    /// 索引器搜索失败。消耗预算。
    pub fn indexer_failed(detail: &str) -> Self {
        Self {
            code: "indexer_search_failed".to_owned(),
            message: detail.to_owned(),
            consumes_budget: true,
        }
    }
}

/// 该检索状态现在**能不能被搜**（上游 `candidate_condition` 的判据）。
///
/// 上游的 SQL 是两句（`:46-53`）：
///
/// ```sql
/// subscription_search_state NOT IN ('running', 'exhausted')
/// AND (subscription_search_state <> 'failed_retryable'
///      OR subscription_search_next_retry_at IS NULL
///      OR subscription_search_next_retry_at <= :now)
/// ```
///
/// # 纯函数，但 SQL 由调用方拼
///
/// 完整候选查询属于 `transfers` 域的自动下载（上游那边还叠了「已订阅」「库里
/// 没有媒体」「没有活跃下载任务」三个条件），不在本文件。这里只定义**本域
/// 的那半条**，免得两边各写一份口径。
///
/// # ★ `succeeded` 也满足第一条
///
/// 上游就是 `NOT IN (running, exhausted)`，没有排除 `succeeded`。看着反直觉
/// （已经搜到了还搜？），但「这次要不要搜」由调用方的其它条件决定（已订阅、
/// 库里没有媒体、没有活跃任务），这里只管这半句。照抄，别自作主张收紧。
///
/// ⚠️ 收紧成 `state IN ('pending', 'failed_retryable')` 曾出现在本文件的骨架
/// 里。它不是「更安全」：`failed_retryable` 的 `next_retry_at` 已经由这里判了，
/// 而多排除一个状态会让某些影片**永远不进候选**。
pub fn is_search_candidate(
    state: &str,
    next_retry_at: Option<NaiveDateTime>,
    now: NaiveDateTime,
) -> bool {
    if state == STATE_RUNNING || state == STATE_EXHAUSTED {
        return false;
    }
    if state != STATE_FAILED_RETRYABLE {
        return true;
    }
    // `failed_retryable`：等退避到点。NULL 视作「立刻可搜」（上游
    // `next_retry_at.is_null(True)` 那一路）。
    next_retry_at.is_none_or(|at| at <= now)
}

/// 订阅搜索状态服务。
///
/// # 为什么要有状态（骨架期是单元结构体）
///
/// `stale_attempt_limit` / `fresh_days` **从配置读**（上游 `settings.downloads.*`），
/// 而写库要走仓储 —— 两个都拿不到就没法落地。这与
/// [`MovieHeatService`](super::movie_heat::MovieHeatService) 同一个取向。
pub struct MovieSubscriptionSearchStateService {
    movies: MovieRepository,
    config: ConfigService,
}

impl MovieSubscriptionSearchStateService {
    /// 构造。
    pub fn new(db: &Db, config: &ConfigService) -> Self {
        Self {
            movies: MovieRepository::new(db.clone()),
            config: config.clone(),
        }
    }

    /// 失败多少次后放弃。配置 `downloads.subscription_search_stale_attempt_limit`。
    ///
    /// 缺省值**不在这里写** —— 它在 `sm-core::config_schema` 的表里，
    /// `View::int_or_default` 会去那儿取。写死会让运维失去调整能力，
    /// 而抄一份数字过来会让它漂移。
    pub fn stale_attempt_limit(&self) -> Result<i64, ServiceError> {
        self.downloads_int("subscription_search_stale_attempt_limit")
    }

    /// 上映日在多少天内算「新鲜」。配置 `downloads.subscription_search_fresh_days`。
    ///
    /// 新鲜期内失败**不消耗预算**（见模块文档）。
    pub fn fresh_days(&self) -> Result<i64, ServiceError> {
        self.downloads_int("subscription_search_fresh_days")
    }

    /// 读一个 `downloads.*` 整数配置。
    fn downloads_int(&self, field: &str) -> Result<i64, ServiceError> {
        let snapshot = self.config.snapshot()?;
        let values = snapshot.as_object().ok_or_else(|| {
            ServiceError::from(sm_db::DbError::business(
                "Config",
                "配置快照不是对象，无法读取 downloads 段",
            ))
        })?;
        sm_core::config_schema::View::new(values)
            .int_or_default("downloads", field)
            .ok_or_else(|| {
                // 键名不在模式里 = 代码与 schema 漂移了。报 500 而不是编个默认值：
                // 编出来的值与「配置没生效」长得一模一样。
                ServiceError::from(sm_db::DbError::business(
                    "Config",
                    format!("配置模式里没有 downloads.{field}"),
                ))
            })
    }

    /// 该影片是否**新鲜**（上映日在 `fresh_days` 以内）。
    ///
    /// 上游 `is_fresh(movie, *, now)`（`:36-41`）：
    /// `release_date is not None and release_date > now - fresh_days`。
    ///
    /// ⚠️ 判据是**上映日期**，不是「最近尝试时间」。骨架期这里的签名是
    /// `is_fresh(last_attempt_at, state)` —— 那是另一回事，而且上游没有它：
    /// 「刚上映的影片不扣预算」与「刚搜过的不再搜」是两条独立的规则，
    /// 后者由 [`is_search_candidate`] 的退避条件表达。
    ///
    /// 没有上映日期 → **不新鲜**（上游 `release_date is None` 时返回 False）：
    /// 不知道它新不新时按普通的算，别给它无限预算。
    pub fn is_fresh(
        release_date: Option<NaiveDateTime>,
        fresh_days: i64,
        now: NaiveDateTime,
    ) -> bool {
        match release_date {
            Some(release) => release > now - chrono::Duration::days(fresh_days),
            None => false,
        }
    }

    /// 领取一次搜索：`→ running` 并记下尝试时刻。**`None` = 没领到**。
    ///
    /// 上游 `begin_attempt(movie_id) -> Movie | None`。返回 `Some(movie_id)`
    /// 表示这次归你了。
    ///
    /// # ⚠️ `WHERE` 里没有状态条件（上游如此）
    ///
    /// 上游是 `Movie.update(...).where(Movie.id == movie_id)`，只按 id。
    /// 看着像漏了「只有 `pending` 才能领」，但候选查询已经筛过状态，而那里
    /// 放行的**不止 `pending`** —— 还有 `failed_retryable`（重试就是这么来的）。
    /// 见 [`is_search_candidate`] 与 `sm_db` 那个方法的文档。
    pub async fn begin_attempt(&self, movie_id: i32) -> Result<Option<i32>, ServiceError> {
        if self
            .movies
            .begin_subscription_search_attempt(movie_id)
            .await?
        {
            Ok(Some(movie_id))
        } else {
            Ok(None)
        }
    }

    /// 标记成功。**终态**，清空错误与尝试计数。
    pub async fn mark_succeeded(&self, movie_id: i32) -> Result<(), ServiceError> {
        self.movies
            .mark_subscription_search_succeeded(movie_id)
            .await?;
        Ok(())
    }

    /// 标记失败。**按 `consumes_budget` × 新鲜度决定落哪个状态**，返回落定的状态。
    ///
    /// ```text
    /// consumes_budget = false               -> failed_retryable（计数**不涨**）
    /// consumes_budget = true 且影片新鲜      -> failed_retryable（计数**不涨**）
    /// consumes_budget = true 且影片不新鲜    -> 计数 +1；达到上限则 exhausted
    /// ```
    ///
    /// 「影片新鲜」= 上映日在 `fresh_days` 内；**没有上映日期不算新鲜**。
    ///
    /// 返回状态是必要的：调用方要写进摘要，而「这次是否变成 exhausted」
    /// 只有这里知道。
    /// # ⚠️ 影片不存在 → `404 movie_not_found`（本仓与上游的唯一分歧）
    ///
    /// 上游收的是**已经读出来的** `Movie` 对象，所以它不可能「读不到」；
    /// 而它随后的 `UPDATE ... WHERE id = ...` 在影片已被删掉时命中 0 行、
    /// **静默返回**。
    ///
    /// 本仓的签名收 id（调用方是检索任务，手里只有一个 id），于是要自己读一次
    /// 当前计数与上映日期 —— 读不到就报 404。理由：按本仓的约定，
    /// 「让你操作的那一行不存在」是 404 而不是静默成功；而且调用方需要看见它
    /// （影片在检索期间被删掉，是值得记一笔的事，不是「什么都没发生」）。
    pub async fn mark_failed(
        &self,
        movie_id: i32,
        error: &SubscriptionSearchError,
    ) -> Result<String, ServiceError> {
        let Some(movie) = self.movies.find_by_id(movie_id).await? else {
            return Err(ServiceError::not_found(
                "movie_not_found",
                "Movie not found",
                "movie_id",
                movie_id,
            ));
        };
        let now = now_utc();

        let mut attempt_count = movie.subscription_search_attempt_count;
        let mut state = STATE_FAILED_RETRYABLE;
        let fresh = Self::is_fresh(movie.release_date, self.fresh_days()?, now);
        if error.consumes_budget && !fresh {
            attempt_count += 1;
            if i64::from(attempt_count) >= self.stale_attempt_limit()? {
                state = STATE_EXHAUSTED;
            }
        }

        self.movies
            .mark_subscription_search_failed(
                movie_id,
                state,
                attempt_count,
                &error.code,
                // 上游写的是 `str(error)` —— 异常消息本身，不是「已尝试 N 次」。
                // 耗尽态的解释文案由展示层给（它拿得到 attempted/limit）。
                &error.message,
            )
            .await?;
        Ok(state.to_owned())
    }

    /// 重置搜索状态。`movie_ids = None`（或空数组）时只重开**已放弃**的。
    ///
    /// 三条口径（都在 [`MovieRepository::reset_subscription_search`] 的文档里）：
    /// 只动已订阅的影片、空数组与 `None` 等价、`retry_round` 加一而不是清零。
    pub async fn reset(&self, movie_ids: Option<&[i32]>) -> Result<u64, ServiceError> {
        Ok(self.movies.reset_subscription_search(movie_ids).await?)
    }

    /// ★ 启动时把卡在 `running` 的改回 `failed_retryable`，返回修复数量。
    ///
    /// 不做这一步，那些影片永远显示「正在搜索」而不再被领取。
    pub async fn recover_interrupted_running(&self) -> Result<u64, ServiceError> {
        Ok(self
            .movies
            .recover_interrupted_subscription_searches(INTERRUPTED_ERROR_MESSAGE)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::details_of;
    use chrono::NaiveDate;

    fn at(text: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").expect("时间")
    }

    /// ★「没找到候选」**不**消耗预算。
    ///
    /// 写成 true 会让刚订阅的新片因「还没被收录」被判失败，几次后永久耗尽
    /// 预算，那部影片再也搜不到。
    #[test]
    fn having_no_candidate_is_not_a_failure() {
        assert!(!SubscriptionSearchError::no_candidate().consumes_budget);
        assert!(SubscriptionSearchError::indexer_failed("超时").consumes_budget);
    }

    /// ★ 候选判据照抄上游：排除 `running` / `exhausted`，其余按退避时间。
    ///
    /// 尤其 `exhausted` 不能再进候选 —— 放它回去会导致「放弃 → 又搜 → 又放弃」
    /// 的无限循环；而 `running` 进去会让同一部影片被两个 worker 同时搜。
    #[test]
    fn only_retryable_states_are_candidates() {
        let now = at("2026-10-05T00:00:00");
        assert!(is_search_candidate(STATE_PENDING, None, now));
        assert!(!is_search_candidate(STATE_RUNNING, None, now));
        assert!(!is_search_candidate(STATE_EXHAUSTED, None, now));

        // `succeeded` **是**候选（上游 `NOT IN (running, exhausted)`）——
        // 是否该搜由调用方的其它条件决定（已订阅 / 库里没有媒体 / 无活跃任务）。
        assert!(
            is_search_candidate(STATE_SUCCEEDED, None, now),
            "别自作主张把 succeeded 排除：上游没有排除它"
        );
    }

    /// `failed_retryable` 要等退避到点；`next_retry_at` 为空表示立刻可搜。
    #[test]
    fn a_retryable_failure_waits_for_its_backoff() {
        let now = at("2026-10-05T00:00:00");
        assert!(
            is_search_candidate(STATE_FAILED_RETRYABLE, None, now),
            "没有退避时间 = 立刻可搜"
        );
        assert!(is_search_candidate(
            STATE_FAILED_RETRYABLE,
            Some(at("2026-10-04T00:00:00")),
            now
        ));
        assert!(
            !is_search_candidate(STATE_FAILED_RETRYABLE, Some(at("2026-10-06T00:00:00")), now),
            "还没到点"
        );
        // 边界包含：恰好等于 now 时算到点（与 `<=` 一致）。
        assert!(is_search_candidate(STATE_FAILED_RETRYABLE, Some(now), now));
    }

    /// ★ 「新鲜」看的是**上映日期**，不是最近尝试时间。
    ///
    /// 这条钉住一个骨架期写错的签名：当时是
    /// `is_fresh(last_attempt_at, state)`，而上游收的是 `movie` 并只看
    /// `release_date`。两者的区别在「刚上映但很久没搜」的影片上会给出相反的答案。
    #[test]
    fn freshness_comes_from_the_release_date() {
        let now = at("2026-10-05T00:00:00");
        let fresh = NaiveDate::from_ymd_opt(2026, 9, 1)
            .expect("日期")
            .and_hms_opt(0, 0, 0)
            .expect("时刻");
        let stale = NaiveDate::from_ymd_opt(2025, 1, 1)
            .expect("日期")
            .and_hms_opt(0, 0, 0)
            .expect("时刻");

        assert!(MovieSubscriptionSearchStateService::is_fresh(
            Some(fresh),
            90,
            now
        ));
        assert!(!MovieSubscriptionSearchStateService::is_fresh(
            Some(stale),
            90,
            now
        ));
        // 没有上映日期 = 不新鲜（按普通影片算，不给无限预算）。
        assert!(!MovieSubscriptionSearchStateService::is_fresh(
            None, 90, now
        ));
    }

    /// 五个状态**互不相同** —— 拼错状态名会让影片卡在无人认领的中间态。
    #[test]
    fn the_five_states_are_distinct() {
        let states = [
            STATE_PENDING,
            STATE_RUNNING,
            STATE_SUCCEEDED,
            STATE_FAILED_RETRYABLE,
            STATE_EXHAUSTED,
        ];
        let mut sorted = states.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), 5);
    }

    /// 「没找到」的错误码是**稳定契约**（会落库并进国际化映射）。
    #[test]
    fn the_no_candidate_code_is_pinned() {
        assert_eq!(ERROR_CODE_NO_CANDIDATE, "no_candidate_found");
        assert_eq!(
            SubscriptionSearchError::no_candidate().code,
            "no_candidate_found"
        );
    }

    /// 崩溃恢复的错误码与文案是**落库的稳定契约**（任务中心按它显示）。
    #[test]
    fn the_interrupted_marker_is_pinned() {
        assert_eq!(ERROR_CODE_TASK_INTERRUPTED, "task_interrupted");
        assert_eq!(
            INTERRUPTED_ERROR_MESSAGE,
            "订阅影片资源查询任务中断，等待重试"
        );
    }

    /// 404 的详情键是 `movie_id`（客户端按它定位）。
    #[test]
    fn the_not_found_detail_key_is_movie_id() {
        let error = ServiceError::not_found_with(
            "movie_not_found",
            "Movie not found",
            details_of("movie_id", 7_i64),
        );
        assert_eq!(
            error
                .api
                .details
                .as_ref()
                .and_then(|map| map.get("movie_id")),
            Some(&serde_json::json!(7))
        );
    }
}
