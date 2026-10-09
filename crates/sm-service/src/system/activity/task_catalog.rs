//! `task_key` → 人类可读任务名，对应上游 `activity/task_catalog.py`（27 行）。
//!
//! # 为什么任务名要查表而不是直接用 `log_name`
//!
//! `background_task_run.task_name` 是**用户可见**的名字（任务中心列表显示它），
//! 而 `log_name`（`movie-heat-update`）是**日志**用的短横线标识。两者不是一回事：
//! 上游为每个任务单独给了中文名。
//!
//! 入队时若调用方没给 `task_name`，就回落到这里的查表结果；查不到再回落到
//! `task_key` 本身 —— 宁可显示一个英文键，也不要让任务在列表里没有名字。

/// 任务键 → 任务显示名。
///
/// 与上游 `TASK_NAME_REGISTRY` 逐条一致，含两个**非 cron** 的队列任务
/// （`library_import` / `media_storage_transfer`，它们是 `manual_only`，
/// 由 producer 入队而不是 cron 触发）。
///
/// # 键的来源
///
/// 19 条来自 `sm_scheduler::builtin_jobs()`（`cron_spec.rs`），另 2 条来自
/// `QUEUE_TASK_REGISTRY`（上游 `scheduler/queue_tasks.py`）。**这份表必须与
/// 那两处保持一致** —— 少一条的任务在任务中心会显示英文键，多一条则是死条目。
pub const TASK_NAME_REGISTRY: [(&str, &str); 21] = [
    ("actor_subscription_sync", "订阅演员影片同步"),
    ("subscribed_movie_auto_download", "已订阅缺失影片自动下载"),
    ("movie_heat_update", "影片热度更新"),
    ("movie_interaction_sync", "影片互动数同步"),
    ("movie_javdb_backfill", "JavDB 影片补录"),
    ("movie_similarity_recompute", "影片相似度重算"),
    ("download_task_sync", "下载任务状态同步"),
    ("download_task_auto_import", "已完成下载自动导入"),
    ("media_file_hash_backfill", "媒体文件哈希补算"),
    ("media_duration_backfill", "媒体时长回填"),
    ("media_video_info_backfill", "媒体信息回填"),
    ("media_resolution_backfill", "媒体分辨率回填"),
    ("media_file_scan", "媒体文件巡检"),
    ("media_thumbnail_generation", "媒体缩略图生成"),
    ("image_search_index", "图像搜索索引构建"),
    ("moment_recommendation_generate", "推荐时刻生成"),
    ("daily_recommendation_generate", "每日推荐快照生成"),
    ("gfriends_filetree_refresh", "GFriends 缓存刷新"),
    ("activity_record_cleanup", "任务记录清理"),
    ("library_import", "媒体库导入"),
    ("media_storage_transfer", "媒体存储迁移"),
];

/// 解析任务的显示名。
///
/// 对应上游 `TaskRunService.resolve_task_name`（`task_runs.py:84`）：
/// 显式传入的非空白名优先，否则查表，再否则回落 `task_key`。
pub fn resolve_task_name(task_key: &str, task_name: Option<&str>) -> String {
    if let Some(name) = task_name.map(str::trim).filter(|s| !s.is_empty()) {
        return name.to_owned();
    }
    lookup_task_name(task_key)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| task_key.to_owned())
}

/// 查表。`task_key` 区分大小写，与上游字典查找一致。
pub fn lookup_task_name(task_key: &str) -> Option<&'static str> {
    TASK_NAME_REGISTRY
        .iter()
        .find(|(key, _)| *key == task_key)
        .map(|(_, name)| *name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_explicit_non_blank_name_wins_over_the_registry() {
        assert_eq!(
            resolve_task_name("movie_heat_update", Some("  自定义名  ")),
            "自定义名"
        );
    }

    #[test]
    fn blank_and_none_names_fall_through_to_the_registry() {
        assert_eq!(
            resolve_task_name("movie_heat_update", None),
            "影片热度更新"
        );
        assert_eq!(
            resolve_task_name("movie_heat_update", Some("   ")),
            "影片热度更新"
        );
    }

    #[test]
    fn an_unknown_key_falls_back_to_itself_rather_than_an_empty_name() {
        // 宁可显示英文键，也不要让任务在列表里没有名字 —— 空名会让
        // 任务中心那一行看起来像加载失败。
        assert_eq!(resolve_task_name("brand_new_task", None), "brand_new_task");
    }

    #[test]
    fn the_registry_has_no_duplicate_keys() {
        // 重复键会让 `lookup_task_name` 静默返回第一条，而两条同名任务
        // 在任务中心里无法区分。
        let mut seen = std::collections::HashSet::new();
        let duplicates: Vec<&str> = TASK_NAME_REGISTRY
            .iter()
            .filter(|(key, _)| !seen.insert(*key))
            .map(|(key, _)| *key)
            .collect();
        assert!(duplicates.is_empty(), "注册表里有重复键：{duplicates:?}");
    }

    // 「与 `sm_scheduler::builtin_jobs()` 对拍」这条断言**不能**放在这里：
    // 依赖方向是 `sm-scheduler → sm-service`，反向依赖会成环。那条对拍
    // 放在 `sm-scheduler` 侧的测试里 —— 那边看得到本表。
}
