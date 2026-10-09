//! 「新增影片」提醒的集成测试（真库），对应上游
//! `shared/import_notifications.py`。
//!
//! # 为什么必须连真库
//!
//! 两条关键保证都是**数据库**给的：
//!
//! | 保证 | 靠什么 |
//! |---|---|
//! | 同一个 TaskRun 只留一条通知 | `UNIQUE(dedupe_key)` + `ON CONFLICT DO NOTHING` |
//! | 没有 task run 时**不去重** | `dedupe_key` 为 NULL（NULL 不参与唯一约束）|
//!
//! 用 mock 或内存实现时，「第二次调用不插入」看起来也对 —— 但那是因为内存里
//! 恰好没有并发/重放。这两个判据只有真的落库才算数。

use sm_db::repo::SystemNotificationRepository;
use sm_db::testing::TestDb;
use sm_service::transfers::import_notifications::{
    create_new_media_reminder, NewMovieReminderItem, NEW_MEDIA_EVENT, NEW_MEDIA_TITLE,
};

fn item(number: &str) -> NewMovieReminderItem {
    NewMovieReminderItem {
        movie_number: number.to_owned(),
        movie_id: None,
    }
}

fn repo(db: &TestDb) -> SystemNotificationRepository {
    SystemNotificationRepository::new(db.pool().clone())
}

async fn notification_count(db: &TestDb) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM system_notification")
        .fetch_one(db.pool())
        .await
        .expect("计数")
}

/// 一次没有新增影片的导入**一条都不发**。
#[tokio::test]
async fn an_empty_import_creates_no_notification() {
    let db = TestDb::require().await;
    let created = create_new_media_reminder(&repo(&db), &[], Some(1))
        .await
        .expect("不该出错");
    assert!(created.is_none());
    assert_eq!(notification_count(&db).await, 0);

    // 只有空白番号的条目同样不发（它们只会让计数虚高）。
    let created = create_new_media_reminder(&repo(&db), &[item("   ")], Some(1))
        .await
        .expect("不该出错");
    assert!(created.is_none());
    assert_eq!(notification_count(&db).await, 0);
}

/// 汇总成**一条**：番号去重、计数是去重后的、事件身份字段齐全。
#[tokio::test]
async fn the_reminder_is_a_single_aggregated_row() {
    let db = TestDb::require().await;
    let items = [item("ABC-001"), item(" ABC-002 "), item("ABC-001")];

    let row = create_new_media_reminder(&repo(&db), &items, Some(17))
        .await
        .expect("可落库")
        .expect("有新增就该发");

    assert_eq!(notification_count(&db).await, 1, "汇总成一条");
    assert_eq!(row.category, "reminder");
    assert_eq!(row.title, NEW_MEDIA_TITLE);
    assert_eq!(
        row.content, "新增了 2 个影片",
        "计数是**去重后**的：重复番号只算一次"
    );
    assert_eq!(row.event_type.as_deref(), Some(NEW_MEDIA_EVENT));
    assert_eq!(
        row.dedupe_key.as_deref(),
        Some("download_import_new_media:task_run:17")
    );
    assert_eq!(row.resource_type.as_deref(), Some("background_task_run"));
    assert_eq!(row.resource_id, Some(17));
    assert_eq!(row.related_task_run_id, Some(17));
    assert_eq!(row.related_resource_type.as_deref(), Some("movie"));
    assert_eq!(
        row.related_resource_id, None,
        "上游读的键是 movie_id，而写入侧给的是 id —— 线上一直是 None（照抄）"
    );
}

/// 同一个 TaskRun 重放（工作进程重启、任务重试）**不会**产生第二条。
#[tokio::test]
async fn the_same_task_run_does_not_get_a_second_notification() {
    let db = TestDb::require().await;
    let items = [item("ABC-001")];

    let first = create_new_media_reminder(&repo(&db), &items, Some(23))
        .await
        .expect("可落库")
        .expect("第一条");
    let second = create_new_media_reminder(&repo(&db), &items, Some(23))
        .await
        .expect("可落库")
        .expect("幂等键命中时返回**既有行**，不是 None");

    assert_eq!(
        first.id, second.id,
        "第二次拿到的必须是用户已经看到的那一条"
    );
    assert_eq!(notification_count(&db).await, 1);
}

/// 没有 task run 时走**不去重**的那条路（通用入口的旧行为）。
///
/// 上游注释：「保留通用入口的旧行为；下载导入链路始终会提供 task run」。
/// 这条把「两条路径的差别」钉住 —— 有人若把 `None` 也改成 `create_once`，
/// 通用调用方的第二次通知就会静默消失。
#[tokio::test]
async fn without_a_task_run_every_call_inserts() {
    let db = TestDb::require().await;
    let items = [item("ABC-001")];

    let first = create_new_media_reminder(&repo(&db), &items, None)
        .await
        .expect("可落库")
        .expect("第一条");
    let second = create_new_media_reminder(&repo(&db), &items, None)
        .await
        .expect("可落库")
        .expect("第二条");

    assert_ne!(first.id, second.id);
    assert_eq!(notification_count(&db).await, 2);
    assert!(first.dedupe_key.is_none(), "没有 task run 就没有幂等键");
    assert!(first.event_type.is_none());
    assert!(first.resource_type.is_none());
    assert!(first.resource_id.is_none());
    assert_eq!(
        first.related_resource_type.as_deref(),
        Some("movie"),
        "关联资源与幂等身份是两套字段，前者两条路径都有"
    );
}
