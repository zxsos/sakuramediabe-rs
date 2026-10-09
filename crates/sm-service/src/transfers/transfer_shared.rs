//! 目录订阅状态与 transfers 之间的共享 SQL 表达式（上游 `shared/common.py`，29 行）。
//!
//! # 为什么这两个 EXISTS 表达式住在 `shared` 而不是各自的服务里
//!
//! 它们被**两侧**同时用到：
//!
//! | 表达式 | 用在 |
//! |---|---|
//! | [`active_download_task_exists`] | 订阅搜索的候选条件（catalog 域）+ 自动下载的候选筛选（transfers 域） |
//! | [`unfinished_import_download_task_exists`] | 同上，但「未完成的导入」 |
//!
//! 写成两份 SQL 的话，两侧的口径会**独立漂移** —— 表现为「界面上显示可下载，
//! 但点了说已有任务」这类前后端不一致。抽出来是为了让它们**逐字相同**。
//!
//! # 这两个表达式**不含过滤条件**，由调用方加
//!
//! 上游是 `fn.EXISTS(...)` 片段工厂，不是完整查询 —— 因为候选筛选还要
//! 叠「按热度取前 N」「按发行窗口」等条件。**别**在这里加那些条件：
//! 两侧的排序策略不同，混进来就毁了「同一口径」的保证。
//!
//! # 任务状态集合的语义：**不是**「已入库」
//!
//! `active` 指「还在进行中」（`queued` / `downloading`）。已完成的**不在**
//! 里面 —— 否则影片下载完就再也不会被重新纳入候选。这是正确行为：
//! 已经拿到文件的影片不需要再下一份。

/// 下载任务里表示「进行中」的状态。
///
/// 对应上游 `DOWNLOAD_STATES` 里被 `is_download_complete` 判为**未**完成的
/// 那些。取值集合由下载器 provider 决定，**不要建模成 enum** —— 新增一个
/// 下载器就可能带来新状态。
pub const ACTIVE_DOWNLOAD_STATES: [&str; 2] = ["queued", "downloading"];

/// 是否仍在进行中的下载任务（按影片番号匹配）。
///
/// **匹配口径是「影片番号」而不是 `movie_id`** —— `DownloadTask` 存的是
/// `movie_number`（见 `schema.sql` 的 `movie_number varchar`），番号才是
/// 下载侧唯一可靠的键（下载器返回的种子标题里没有库内 id）。
///
/// 返回可直接拼进 `WHERE` 的 SQL 片段 + 绑定参数。**参数顺序是
/// `movie_number` 在前、`states` 在后**，与片段里的占位符一一对应；用
/// `QueryBuilder` 拼时注意别让两处顺序错开。
pub fn active_download_task_exists(movie_number: &str) -> (String, Vec<String>) {
    (
        "EXISTS (SELECT 1 FROM download_task dt \
         WHERE dt.movie_number = $1 AND dt.state = ANY($2))"
            .to_owned(),
        vec![movie_number.to_owned(), format!("{{{}}}", ACTIVE_DOWNLOAD_STATES.join(","))],
    )
}

/// 未完成的**导入**任务所关联的下载任务。
///
/// 与 [`active_download_task_exists`] 的区别：这里判的是「下载已完成、
/// 但导入还没做完」。导入失败的任务要能被重新捞出来，所以**不能**只判
/// 下载状态 —— 那样失败的导入会永远卡住，影片再也导不进来。
pub fn unfinished_import_download_task_exists(movie_number: &str) -> (String, Vec<String>) {
    (
        "EXISTS (SELECT 1 FROM download_task dt \
         JOIN background_task_run tr ON tr.download_task_id = dt.id \
         WHERE dt.movie_number = $1 AND tr.status <> 'succeeded')"
            .to_owned(),
        vec![movie_number.to_owned()],
    )
}

/// 任务状态是否属于「已完成」。
///
/// 上游 `is_download_complete(state)`。**完成态是白名单**，不是「非进行中」
/// —— 将来 provider 加一个 `failed` 状态时，它既不是完成也不是进行中，
/// 按「非进行中即完成」判会让失败的任务被当成已入库。
pub fn is_download_complete(state: &str) -> bool {
    matches!(state, "completed" | "done" | "seeding" | "finished")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 片段里的占位符数量必须与参数个数一致 —— 少了会运行期报错，
    /// 多了会让 `QueryBuilder` 静默错位。
    #[test]
    fn placeholder_count_matches_parameters() {
        let (sql, params) = active_download_task_exists("ABC-123");
        assert_eq!(sql.matches('$').count(), params.len());
        assert!(sql.contains("dt.movie_number = $1"));
    }

    /// 「已完成」是白名单：`failed` 不得被判成完成。
    ///
    /// 这条是本文件最容易写错的地方 —— 写成「非进行中即完成」会让失败任务
    /// 被当作已入库，影片从此不再被纳入下载候选。
    #[test]
    fn only_known_finished_states_count_as_complete() {
        assert!(is_download_complete("completed"));
        assert!(!is_download_complete("failed"));
        assert!(!is_download_complete("error"));
        assert!(!is_download_complete("queued"));
    }

    /// 进行中与已完成**不得重叠**。
    #[test]
    fn active_and_complete_states_are_disjoint() {
        for state in ACTIVE_DOWNLOAD_STATES {
            assert!(
                !is_download_complete(state),
                "{state} 同时出现在进行中与已完成里"
            );
        }
    }
}
