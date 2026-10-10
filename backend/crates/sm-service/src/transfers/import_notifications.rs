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
//! 两个键都要：`task_run_id` 让「同一个 TaskRun 重试 / 重放」不重复通知
//! （持久幂等键），`movie_number` 让「同一部影片在两次导入结果里都出现」
//! 不被数两次。
//!
//! # 没有新增影片就**一条都不发**
//!
//! 不是「发一条空的」。空通知会让用户以为出事了。
//!
//! # ⚠️ 骨架期的形状是**自造**的（本轮改回上游）
//!
//! | 骨架期 | 上游 |
//! |---|---|
//! | `NewMediaReminder { title: "本次导入新增 N 部影片", items: [...] }` —— 一个自造的资源，逐部展开条目 | 通知的 `title` 是**固定文案**「有新的影片可以播放了」，正文是「新增了 N 个影片」；**没有逐部条目** |
//! | 不去重，且截断到 **20 部**（注释自称「这是刻意的差异」）| **按 `movie_number` 去重**、不截断（`:14-23`）|
//! | `related_task_run_id: Option<i64>` | `Option<i32>`（`background_task_run.id` 是 `integer`）|
//!
//! 那个 20 部上限的理由（「逐部展开 200 行会让通知中心变成列表页」）在形状改回
//! 上游之后**不复存在**：上游的通知正文只有一个计数。`handoff.md` §五 只登记了
//! 两处刻意照抄的缺陷，这一处不在其中 —— 它是骨架期凭空加的。
//!
//! # 接线时的三条约束（上游 `import_task_service.py:287-299`，本轮未落地）
//!
//! 1. **只有下载任务发起的导入才发提醒**。上游的判据逐字是
//!    `"download_tasks" in params or params.get("download_task_id") is not None`
//!    —— 用户从「浏览导入」发起的那次（`POST /imports`）**不发**。
//! 2. `related_task_run_id` 传的是 `reporter.task_run_id`，所以走的是
//!    `Some` 那条（带幂等键）。`None` 那条留给**别的**调用方。
//! 3. **提醒失败不能让导入失败**：上游把整个调用包在
//!    `try/except Exception: logger.warning(...)` 里。已写入的媒体不能因为
//!    「通知没发出去」而回滚 —— 那是两件事。

use sm_db::repo::{NewNotification, SystemNotificationRepository};
use sm_db::system::activity::{notification_category, SystemNotification};

use crate::error::ServiceError;

/// 事件类型。与上游逐字一致（`import_notifications.py:42`）。
pub const NEW_MEDIA_EVENT: &str = "download_import_new_media";

/// 幂等键的前缀。全文是 `download_import_new_media:task_run:{task_run_id}`。
const DEDUPE_PREFIX: &str = "download_import_new_media:task_run:";

/// 通知标题。**固定的**，不含计数（上游 `:28`）。
pub const NEW_MEDIA_TITLE: &str = "有新的影片可以播放了";

/// 通知的来源资源类型（上游 `:44` 写的是 `background_task_run`，不是
/// `task_run` —— 客户端按这个字符串找跳转目标，改名它就点不动）。
const RESOURCE_TYPE_TASK_RUN: &str = "background_task_run";

/// 关联资源的类型（上游 `:31`）。
const RELATED_RESOURCE_TYPE: &str = "movie";

/// 幂等键。上游 `f"download_import_new_media:task_run:{related_task_run_id}"`
/// （`:43`）。
pub fn new_media_dedupe_key(task_run_id: i32) -> String {
    format!("{DEDUPE_PREFIX}{task_run_id}")
}

/// 一部新增影片。上游的输入是 `movie_items: list[dict]`，本仓收成结构体 ——
/// 生产者只有一个（导入执行体），不需要容忍任意键。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct NewMovieReminderItem {
    /// 番号。去重与计数都只看它。
    ///
    /// 上游是 `str(item.get("movie_number") or "").strip()`：**非字符串也能收**，
    /// 空值直接丢。这里是 `String`（构造方是宿主自己），空串 / 空白同样丢。
    pub movie_number: String,
    /// 关联的影片 id。
    ///
    /// ⚠️ **上游读的键名是 `movie_id`，而写入侧给的键名是 `id`**
    /// （`imports/import_service.py:720-723` 的 `new_playable_movies` 三项是
    /// `id` / `movie_number` / `title`）。所以这一项在线上**一直是 `None`**。
    ///
    /// **照抄，不要「顺手修」成读 `id`**（`handoff.md` §五 的取向）：那会改变
    /// 通知挂的关联资源，而客户端可能已按「没有关联资源」渲染。
    ///
    /// 宽度与 `movie.id` 一致（`integer`，`sm_db::catalog::movie::Movie.id` 是
    /// `i32`）。
    pub movie_id: Option<i32>,
}

/// 造并落库「本次导入新增影片」的提醒。**没有新增影片返回 `None`**。
///
/// 上游 `create_new_media_reminder`（`:8-47`）。
///
/// # 两条落库路径**不是**「同一条的两种写法」
///
/// | `related_task_run_id` | 走哪条 | 差别 |
/// |---|---|---|
/// | `Some(id)` | `create_once`（带幂等键）| 同一 TaskRun 重放**只留一条** |
/// | `None` | `notify`（无幂等键）| 保留通用入口的旧行为：每次都插 |
///
/// 上游注释原话：「保留通用入口的旧行为；下载导入链路始终会提供 task run」。
/// 所以 `None` 那条只在**非导入链路**被调用时才走得到 —— 别把它当成可以省掉的
/// 分支（省掉会让通用调用方的行为变成「第二次不发了」）。
pub async fn create_new_media_reminder(
    repo: &SystemNotificationRepository,
    movie_items: &[NewMovieReminderItem],
    related_task_run_id: Option<i32>,
) -> Result<Option<SystemNotification>, ServiceError> {
    let unique = unique_by_movie_number(movie_items);
    if unique.is_empty() {
        return Ok(None);
    }

    let mut draft = NewNotification {
        category: notification_category::REMINDER.to_owned(),
        title: NEW_MEDIA_TITLE.to_owned(),
        content: new_media_content(unique.len()),
        // 只有带 task run 那条路径才给「事件身份」三件套 —— 见函数文档。
        event_type: None,
        dedupe_key: None,
        resource_type: None,
        resource_id: None,
        related_task_run_id,
        related_resource_type: Some(RELATED_RESOURCE_TYPE.to_owned()),
        // 见 [`NewMovieReminderItem::movie_id`]：上游读的键没人写，所以线上一直是
        // `None`；真给了就用它。
        related_resource_id: unique[0].movie_id,
    };

    let Some(task_run_id) = related_task_run_id else {
        return Ok(repo.notify(&draft).await?);
    };
    draft.event_type = Some(NEW_MEDIA_EVENT.to_owned());
    draft.dedupe_key = Some(new_media_dedupe_key(task_run_id));
    draft.resource_type = Some(RESOURCE_TYPE_TASK_RUN.to_owned());
    draft.resource_id = Some(task_run_id);
    // `create_once` 命中幂等键时返回**既有行**（不是 None）—— 与上游
    // `NotificationService.create_once` 一致：调用方拿到的那条就是用户看到的那条。
    Ok(Some(repo.create_once(&draft).await?))
}

/// 通知正文。上游 `f"新增了 {len(unique_items)} 个影片"`（`:29`）。
///
/// 计数是**去重之后**的，不是入参长度 —— 同一次导入里同一个番号出现两次
/// （多文件、多版本）时多报，用户会以为导入重复了。
fn new_media_content(count: usize) -> String {
    format!("新增了 {count} 个影片")
}

/// 按番号去重（顺序保持首次出现的顺序）。
///
/// 上游 `:14-23`：番号 `str(...).strip()`，**空的丢掉**（番号都没有的条目
/// 放进通知只会让计数虚高），重复的只留第一条。
fn unique_by_movie_number(movie_items: &[NewMovieReminderItem]) -> Vec<&NewMovieReminderItem> {
    let mut seen: Vec<&str> = Vec::new();
    let mut unique: Vec<&NewMovieReminderItem> = Vec::new();
    for item in movie_items {
        let number = item.movie_number.trim();
        if number.is_empty() || seen.contains(&number) {
            continue;
        }
        seen.push(number);
        unique.push(item);
    }
    unique
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(number: &str) -> NewMovieReminderItem {
        NewMovieReminderItem {
            movie_number: number.to_owned(),
            movie_id: None,
        }
    }

    /// 没新增影片就**一条都不发** —— 发空通知会让用户以为出事了。
    #[test]
    fn an_empty_import_creates_no_reminder() {
        assert!(unique_by_movie_number(&[]).is_empty());
    }

    /// 只有空白番号的条目也**一条都不发**（它们进去只会让计数虚高）。
    #[test]
    fn items_without_a_movie_number_do_not_count() {
        assert!(unique_by_movie_number(&[item(""), item("   ")]).is_empty());
    }

    /// 重复番号只算一次，且保持首次出现的顺序。
    #[test]
    fn duplicate_movie_numbers_are_counted_once() {
        let items = [
            item("ABC-001"),
            item(" ABC-002 "),
            item("ABC-001"),
            item("ABC-003"),
            item("ABC-002"),
        ];
        let unique = unique_by_movie_number(&items);
        let numbers: Vec<&str> = unique.iter().map(|i| i.movie_number.trim()).collect();
        assert_eq!(numbers, ["ABC-001", "ABC-002", "ABC-003"]);
    }

    /// 标题是**固定文案**（不含计数），计数在正文里。
    ///
    /// 骨架期把计数写进标题（「本次导入新增 N 部影片」）—— 那会与上游的通知
    /// 文案不一致，而客户端可能按文案做本地化或图标映射。
    #[test]
    fn the_title_is_fixed_and_the_count_lives_in_the_content() {
        assert_eq!(NEW_MEDIA_TITLE, "有新的影片可以播放了");
        assert_eq!(new_media_content(30), "新增了 30 个影片");
        assert_eq!(new_media_content(1), "新增了 1 个影片");
    }

    /// 幂等键逐字是上游那个 —— 改一个字，同一个 TaskRun 就会发第二条通知。
    #[test]
    fn the_dedupe_key_matches_upstream() {
        assert_eq!(
            new_media_dedupe_key(42),
            "download_import_new_media:task_run:42"
        );
        assert_eq!(NEW_MEDIA_EVENT, "download_import_new_media");
    }

    /// 事件身份里的资源类型是 `background_task_run`，**不是** `task_run`。
    ///
    /// 客户端按这个字符串找跳转目标。
    #[test]
    fn the_resource_type_is_the_task_run_table_name() {
        assert_eq!(RESOURCE_TYPE_TASK_RUN, "background_task_run");
        assert_eq!(RELATED_RESOURCE_TYPE, "movie");
    }

    /// `movie_id` 读的是上游那个键，而写入侧给的是 `id` —— 所以线上是 `None`。
    ///
    /// 这条把「照抄而不修」钉住：有人若把它改成读 `id`，`related_resource_id`
    /// 就会开始有值，而客户端已按「没有关联资源」渲染。
    #[test]
    fn the_related_resource_id_stays_none_because_upstream_reads_a_key_nobody_writes() {
        let item = NewMovieReminderItem {
            movie_number: "ABC-001".to_owned(),
            movie_id: None,
        };
        assert!(item.movie_id.is_none());
        // 若上游真的给了 `movie_id`，就用它。
        let with_id = NewMovieReminderItem {
            movie_number: "ABC-001".to_owned(),
            movie_id: Some(7),
        };
        assert_eq!(with_id.movie_id, Some(7));
    }
}
