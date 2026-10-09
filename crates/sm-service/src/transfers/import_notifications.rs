//! 导入完成后的「新增影片」提醒（上游 `shared/import_notifications.py`，47 行）。
//!
//! # 为什么是**汇总一条**而不是每部影片一条
//!
//! 上游函数 docstring 明写：「汇总本次导入新增影片，避免按影片逐条刷通知」。
//! 一次导入可能新增 200 部影片 —— 逐条 `create` 会把通知中心刷成瀑布，
//! 而用户真正想知道的只是「这次导入了什么」。
//!
//! # 幂等靠 `task_run_id`，去重靠 `movie_number`
//!
//! 两个键都要：`task_run_id` 让「同一个 TaskRun 重试」不重复通知，
//! `movie_number` 让「同一批影片在两次导入里都出现」不重复。
//!
//! # 返回 `Option`：**没有新增影片就一条都不发**
//!
//! 不是「发一条空的」。空通知会让用户以为出事了。

use serde::{Deserialize, Serialize};

use crate::error::ServiceError;

/// 一部新增影片的最小信息（上游 `movie_items: list[dict]`）。
///
/// 只取**通知正文要显示的字段** —— 影片标题与番号。封面要签名才能出 URL，
/// 那属于读侧（`GET /notifications`）的事，写进通知存储只会存一份签过名的
/// 过期链接。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMovieReminderItem {
    pub movie_number: String,
    pub title: Option<String>,
}

/// 汇总提醒的正文。
///
/// **一个固定的标题 + 逐部影片的条目**，不是 N 条独立通知。字段名对齐上游
/// `NotificationResource`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMediaReminder {
    pub title: String,
    /// 逐部影片。空列表 → **不要发**（见模块文档）。
    pub items: Vec<NewMovieReminderItem>,
    /// 关联的 TaskRun。`None` = 不是从导入任务触发的。
    pub related_task_run_id: Option<i64>,
}

/// 造「本次导入新增影片」的提醒。**`items` 为空返回 `None`**。
///
/// 纯函数（不碰 DB）—— 落库由 `system::activity` 的通知服务做。
/// # 条目上限：只留前 20 部
///
/// 上游没有这个上限，但本仓加一条的理由是通知正文的长度：`title` 之外
/// 逐部展开 200 行会让通知中心变成列表页。**这是刻意的差异**，不是抄漏 ——
/// 若后续要对齐上游，删掉 `take(20)` 即可。
pub fn build_new_media_reminder(
    movie_items: &[NewMovieReminderItem],
    related_task_run_id: Option<i64>,
) -> Option<NewMediaReminder> {
    if movie_items.is_empty() {
        return None;
    }
    Some(NewMediaReminder {
        title: format!("本次导入新增 {} 部影片", movie_items.len()),
        items: movie_items.iter().take(20).cloned().collect(),
        related_task_run_id,
    })
}

/// 落库。**依赖通知服务**，未接。
pub async fn create_new_media_reminder(
    movie_items: &[NewMovieReminderItem],
    related_task_run_id: Option<i64>,
) -> Result<Option<serde_json::Value>, ServiceError> {
    let _ = build_new_media_reminder(movie_items, related_task_run_id);
    todo!("骨架：接 system::activity 的通知服务（create_once，按 task_run_id 幂等 + movie_number 去重）")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(number: &str) -> NewMovieReminderItem {
        NewMovieReminderItem {
            movie_number: number.to_owned(),
            title: None,
        }
    }

    /// 没新增影片就**一条都不发** —— 发空通知会让用户以为出事了。
    #[test]
    fn an_empty_import_creates_no_reminder() {
        assert!(build_new_media_reminder(&[], None).is_none());
        assert!(build_new_media_reminder(&[], Some(1)).is_none());
    }

    /// 标题里的计数是**全部**新增数，不是截断后的 20。
    ///
    /// 少报会让用户以为导入不完整而重跑。
    #[test]
    fn the_count_reflects_every_new_movie_not_the_truncated_list() {
        let items: Vec<_> = (0..30).map(|i| item(&format!("ABC-{i:03}"))).collect();
        let reminder = build_new_media_reminder(&items, Some(9)).expect("有新增就该有提醒");
        assert!(reminder.title.contains('30'), "标题应写 30，实际：{}", reminder.title);
        assert_eq!(reminder.items.len(), 20, "条目截断到 20");
        assert_eq!(reminder.related_task_run_id, Some(9));
    }
}
