//! 终态 CAS 与通知去重的集成测试。
//!
//! 覆盖的是**静态分析证明不了**的东西：
//!
//! | 验证内容 | 为什么必须连真库 |
//! |---|---|
//! | `pending` 也能收口 | 与队列路径 `finish`（只认 `running`）的语义差 |
//! | 摘要是合并而非覆盖 | 读-改-写必须在行锁内，否则并发 reporter 互相丢键位 |
//! | 输掉竞争时读到既有终态 | `FOR UPDATE` 的真实阻塞行为 |
//! | `create_once` 只留一条 | 依赖 `dedupe_key` 唯一索引 + `ON CONFLICT` |
//! | 释放键后可再次提醒 | 置空键而不是删行 |
//!
//! 没有数据库时**失败**（`TestDb::require()` 直接 panic）—— 静默跳过会让
//! 「CAS 范围写错」这类问题一路混进 worker。

use serde_json::json;
use sm_db::repo::{
    BackgroundTaskRunRepository, NewNotification, NewTaskRun, SystemNotificationRepository,
    TaskProgress,
};
use sm_db::system::activity::{
    notification_category, result_summary::format_text, task_state, SystemNotification,
};
use sm_db::testing::TestDb;

mod fixtures {
    use super::*;

    pub fn task(key: &str) -> NewTaskRun {
        NewTaskRun {
            task_key: key.to_owned(),
            task_name: format!("{key} 任务"),
            trigger_type: "manual".to_owned(),
            mutex_key: None,
            params: None,
            scheduled_at: None,
        }
    }

    /// 一条任务终态通知。`resource_type` 与 `resource_id` **必须成对** ——
    /// 仓储的 `validate` 会拒掉只给一个的写法。
    pub fn task_result_notification(run_id: i32, dedupe: &str) -> NewNotification {
        NewNotification {
            category: notification_category::INFO.to_owned(),
            title: "标题".to_owned(),
            content: "内容".to_owned(),
            event_type: Some("task_run_result".to_owned()),
            dedupe_key: Some(dedupe.to_owned()),
            resource_type: Some("task_run".to_owned()),
            resource_id: Some(run_id),
            related_task_run_id: Some(run_id),
            related_resource_type: None,
            related_resource_id: None,
        }
    }
}

use fixtures::{task, task_result_notification};

/// 便捷：建一行并返回其 id。
async fn seed(repo: &BackgroundTaskRunRepository, key: &str) -> i32 {
    repo.enqueue(&task(key)).await.expect("入队").id
}

#[tokio::test]
async fn mark_running_only_moves_pending_and_never_rewrites_started_at() {
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let id = seed(&repo, "mark_running").await;

    let first = repo
        .mark_running(id)
        .await
        .expect("第一次")
        .expect("行存在");
    assert_eq!(first.state, task_state::RUNNING);
    let started_at = first.started_at;
    assert!(started_at.is_some(), "running 行必须有 started_at");

    // 幂等：第二次不匹配 pending，返回原行且不改写 started_at。
    let second = repo
        .mark_running(id)
        .await
        .expect("第二次")
        .expect("行存在");
    assert_eq!(second.state, task_state::RUNNING);
    assert_eq!(
        second.started_at, started_at,
        "重复 mark_running 不得改写 started_at"
    );

    // 终态行原样返回，且不产生写入。
    repo.complete_active(id, Some(&json!({"ok": 1})), None)
        .await
        .expect("收口");
    let after = repo
        .mark_running(id)
        .await
        .expect("终态后再调")
        .expect("行存在");
    assert_eq!(
        after.state,
        task_state::COMPLETED,
        "终态行不得被拉回 running"
    );
}

#[tokio::test]
async fn complete_active_accepts_pending_which_the_queue_path_refuses() {
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let id = seed(&repo, "pending_complete").await;

    // 队列路径的 `finish` 只认 running，对 pending 直接报 business 错误。
    // 服务层的 `complete_active` 必须接受 —— 未领取就判定「功能停用」而跳过
    // 的路径走的就是这条。
    let queued = repo.finish(id, &Default::default()).await;
    assert!(queued.is_err(), "finish 应当拒绝 pending 行（对照组）");

    let (run, won) = repo
        .complete_active(id, None, Some("skipped"))
        .await
        .expect("complete_active 应当接受 pending")
        .expect("行存在");
    assert!(won, "从 pending 收口算赢得转移");
    assert_eq!(run.state, task_state::COMPLETED);
    assert_eq!(run.result_text.as_deref(), Some("skipped"));
}

#[tokio::test]
async fn complete_active_merges_summary_and_derives_text_from_it() {
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let id = seed(&repo, "merge").await;
    repo.mark_running(id)
        .await
        .expect("标记运行")
        .expect("行存在");

    // 先写一次进度摘要。
    repo.report_progress_active(id, &TaskProgress::default(), Some(&json!({"processed": 3})))
        .await
        .expect("写进度")
        .expect("行存在");

    // 收口时只带一个键 —— 之前的 `processed` 必须还在。
    let (run, won) = repo
        .complete_active(id, Some(&json!({"imported": 2})), None)
        .await
        .expect("收口")
        .expect("行存在");
    assert!(won);
    let summary: serde_json::Value =
        serde_json::from_str(run.result_summary.as_deref().expect("摘要非空")).expect("摘要合法");
    assert_eq!(summary["processed"], 3, "旧键不得被覆盖");
    assert_eq!(summary["imported"], 2, "新键必须写入");

    // text 缺省时由摘要格式化兜底，键序即插入序。
    assert_eq!(
        run.result_text.as_deref(),
        Some("processed=3 imported=2"),
        "result_text 应由摘要按插入序格式化"
    );
}

#[tokio::test]
async fn losing_the_race_yields_the_persisted_terminal_state() {
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let id = seed(&repo, "race").await;
    repo.mark_running(id)
        .await
        .expect("标记运行")
        .expect("行存在");

    let (_, first_won) = repo
        .fail_active(id, "先到的失败", None)
        .await
        .expect("先收口")
        .expect("行存在");
    assert!(first_won, "先到的一方赢得转移");

    // 后到的「成功」必须服从已持久化的终态，而不是覆盖它。
    let (late, late_won) = repo
        .complete_active(id, Some(&json!({"late": true})), Some("迟到"))
        .await
        .expect("后收口")
        .expect("行存在");
    assert!(!late_won, "迟到的一方必须知道自己输了");
    assert_eq!(
        late.state,
        task_state::FAILED,
        "迟到的一方不得把 failed 改写成 completed"
    );
    assert_eq!(late.error_message.as_deref(), Some("先到的失败"));
    assert!(
        !late
            .result_summary
            .as_deref()
            .unwrap_or("{}")
            .contains("late"),
        "迟到的一方连摘要都不该写进去"
    );
}

#[tokio::test]
async fn fail_active_also_releases_the_mutex_key() {
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let mut draft = task("mutex_release");
    draft.mutex_key = Some("aps:mutex_release".to_owned());
    let run = repo.enqueue(&draft).await.expect("入队");
    repo.mark_running(run.id)
        .await
        .expect("标记运行")
        .expect("行存在");

    let (failed, won) = repo
        .fail_active(run.id, "boom", None)
        .await
        .expect("收口失败")
        .expect("行存在");
    assert!(won);
    assert!(
        !failed.is_mutex_guarded(),
        "失败也必须释放互斥键，否则重试永远撞唯一约束"
    );

    // 同键的下一个任务必须插得进来。
    let again = repo.enqueue(&draft).await.expect("同键再次入队");
    assert_eq!(again.mutex_key.as_deref(), Some("aps:mutex_release"));
}

#[tokio::test]
async fn report_progress_active_touches_only_the_fields_it_was_given() {
    let db = TestDb::require().await;
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let id = seed(&repo, "progress").await;
    repo.mark_running(id)
        .await
        .expect("标记运行")
        .expect("行存在");

    repo.report_progress_active(
        id,
        &TaskProgress {
            current: Some(1),
            total: Some(10),
            text: Some("第一层".to_owned()),
        },
        None,
    )
    .await
    .expect("写进度")
    .expect("行存在");

    // 只带 text：数值进度必须保持原值（与 `report_progress` 的三件套清空不同）。
    let after = repo
        .report_progress_active(
            id,
            &TaskProgress {
                text: Some("第二层".to_owned()),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("再写")
        .expect("行存在");
    assert_eq!(after.progress_current, Some(1), "未传入的字段不得被清空");
    assert_eq!(after.progress_total, Some(10));
    assert_eq!(after.progress_text.as_deref(), Some("第二层"));

    // 终态行零写入。
    repo.complete_active(id, None, None)
        .await
        .expect("收口")
        .expect("行");
    let terminal = repo
        .report_progress_active(
            id,
            &TaskProgress {
                current: Some(9),
                ..Default::default()
            },
            None,
        )
        .await
        .expect("终态后写进度")
        .expect("行存在");
    assert_eq!(terminal.state, task_state::COMPLETED);
    assert_ne!(terminal.progress_current, Some(9), "终态行不得再被写进度");
}

#[tokio::test]
async fn create_once_keeps_exactly_one_notification_per_dedupe_key() {
    let db = TestDb::require().await;
    let repo = SystemNotificationRepository::new(db.pool().clone());
    // `related_task_run_id` 有外键指向 `background_task_run`，必须给一个
    // 真实存在的行 —— 这正是「通知挂在任务上」这条契约的 enforcement。
    let run_id = seed(
        &BackgroundTaskRunRepository::new(db.pool().clone()),
        "notify_target",
    )
    .await;

    let first = repo
        .create_once(&task_result_notification(run_id, "task_run_result:1"))
        .await
        .expect("首次创建");
    let second = repo
        .create_once(&task_result_notification(run_id, "task_run_result:1"))
        .await
        .expect("重复创建");
    assert_eq!(first.id, second.id, "重复调用必须返回既有行");
    assert_eq!(repo.count_unread().await.expect("计数"), 1);
}

#[tokio::test]
async fn create_once_rejects_a_blank_dedupe_key_instead_of_skipping_dedupe() {
    let db = TestDb::require().await;
    let repo = SystemNotificationRepository::new(db.pool().clone());

    let mut blank = task_result_notification(1, "x");
    blank.dedupe_key = Some("   ".to_owned());
    let err = repo
        .create_once(&blank)
        .await
        .expect_err("空白 dedupe_key 必须报错");
    assert!(
        err.to_string().contains("dedupe_key"),
        "错误信息应指向 dedupe_key，实际：{err}"
    );
}

#[tokio::test]
async fn releasing_the_dedupe_key_allows_the_next_reminder() {
    let db = TestDb::require().await;
    let repo = SystemNotificationRepository::new(db.pool().clone());
    let run_id = seed(
        &BackgroundTaskRunRepository::new(db.pool().clone()),
        "release_target",
    )
    .await;

    let first = repo
        .create_once(&task_result_notification(run_id, "task_run_result:2"))
        .await
        .expect("首次创建");
    assert_eq!(
        repo.release_dedupe_key("task_run_result:2")
            .await
            .expect("释放"),
        1
    );

    // 键被置空后可以再插一条 —— 历史保留，提醒重新触发。
    let second = repo
        .create_once(&task_result_notification(run_id, "task_run_result:2"))
        .await
        .expect("再次提醒");
    assert_ne!(second.id, first.id, "释放后应产生新行");
    assert_eq!(
        repo.count_unread().await.expect("计数"),
        2,
        "两条都在，未被删"
    );

    let by_key: SystemNotification = repo
        .find_by_dedupe_key("task_run_result:2")
        .await
        .expect("回读")
        .expect("新行有键");
    assert_eq!(by_key.id, second.id, "旧行的键已置空，回读只能命中新行");
}

#[test]
fn category_whitelist_is_enforced_and_normalization_falls_back_to_info() {
    let mut draft = task_result_notification(1, "k");

    // 仓储层拒绝未知分类。
    draft.category = "nonsense".to_owned();
    assert!(draft.validate().is_err(), "未知分类必须被拒");
    assert_eq!(draft.normalized_category(), notification_category::DEFAULT);

    // 两侧空白先裁掉再判白名单。
    draft.category = " warning ".to_owned();
    assert!(draft.validate().is_ok());
    assert_eq!(draft.normalized_category(), notification_category::WARNING);
}

#[test]
fn resource_type_and_id_must_be_paired() {
    let mut draft = task_result_notification(7, "k");
    draft.resource_id = None;
    assert!(
        draft.validate().is_err(),
        "只给 resource_type 无法定位资源，必须报错"
    );
}

#[test]
fn result_text_skips_containers_and_nulls() {
    // 上游 `format_result_text`：`isinstance(value, (dict, list)) or value is None`
    // 一律跳过 —— 那些值塞进一行文本读不出来。
    let json = r#"{"a":1,"b":true,"c":null,"d":[1],"e":{"f":2},"g":"x"}"#;
    assert_eq!(
        format_text(Some(json)).as_deref(),
        Some("a=1 b=true g=x"),
        "键序应与插入序一致"
    );
    assert_eq!(format_text(Some("{}")), None, "空摘要没有可读文本");
    assert_eq!(format_text(Some("not json")), None, "非法 JSON 不应 panic");
    assert_eq!(format_text(None), None);
}
