//! 失败项列表（`GET /imports/{task_run_id}/failed-items`）的集成测试（真库）。
//!
//! # 为什么要连真库
//!
//! 这条路径只有一处逻辑（投影），但**两处判据是库给的**：
//!
//! - 「任务不存在」与「任务存在但没有失败项」必须能区分（404 vs 200 空列表）
//!   —— 而「存在」的判据里还有一条 `task_key` 必须等于 `library_import`
//!   （`/imports/{id}` 的 id 是裸整数，别的任务类型的 id 也能填进来）；
//! - 失败项**不在表里**，在 `background_task_run.result_summary` 这个
//!   JSON 文本列里（没有单独的失败项表）。
//!
//! 换成 mock，这两条都测不到。

use serde_json::json;
use sm_db::repo::{BackgroundTaskRunRepository, NewTaskRun};
use sm_db::testing::TestDb;
use sm_service::transfers::import_task::ImportTaskService;

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

fn svc(db: &TestDb) -> ImportTaskService {
    ImportTaskService::new(db.pool())
}

/// 建一条台账行，可选地写上 `result_summary`。
///
/// 直接写列而不是走 `complete_task_run`：这一步要造的只是**一行数据**，
/// 而收口路径会连带发通知（与本用例无关的副作用）。
async fn seed_run(db: &TestDb, task_key: &str, result_summary: Option<&str>) -> i32 {
    let run = BackgroundTaskRunRepository::new(db.pool().clone())
        .enqueue(&NewTaskRun {
            task_key: task_key.to_owned(),
            task_name: format!("{task_key} 任务"),
            trigger_type: "manual".to_owned(),
            // 互斥键唯一 —— 每次都用新的，免得撞上 UNIQUE(mutex_key)。
            mutex_key: Some(format!("failed-items-{}", n())),
            params: None,
            scheduled_at: Some(sm_db::common::time::now_utc()),
        })
        .await
        .expect("enqueue");
    if let Some(summary) = result_summary {
        sqlx::query("UPDATE background_task_run SET result_summary = $1 WHERE id = $2")
            .bind(summary)
            .bind(run.id)
            .execute(db.pool())
            .await
            .expect("写 result_summary");
    }
    run.id
}

/// 一条与上游 `_make_failure_item` 同形的失败项。
fn failed_file(id: &str, reason: &str, state: &str) -> serde_json::Value {
    json!({
        "id": id,
        "name": format!("{id}.mkv"),
        "relative_path": format!("人妻/{id}.mkv"),
        "size_bytes": 2048,
        "is_video": true,
        "source_ref": {"path": format!("人妻/{id}.mkv")},
        "library_id": 7,
        "media_kind": "jav",
        "source_disposition": "keep",
        "path": format!("人妻/{id}.mkv"),
        "reason": reason,
        "detail": "boom",
        "kind": "file",
        "state": state,
        "retry_task_run_id": null,
        "resolved_movie_id": null,
        "resolved_media_id": null,
        "last_retry_error": null,
    })
}

/// 任务不存在 → **404 `import_task_not_found`**（不是 200 空列表）。
#[tokio::test]
async fn a_missing_task_run_is_404() {
    let db = TestDb::require().await;
    let error = svc(&db)
        .list_failed_items(999_999)
        .await
        .expect_err("必须 404");
    assert_eq!(error.status, 404);
    assert_eq!(error.code(), "import_task_not_found");
}

/// 台账行存在、但**不是导入任务** → 同样 **404**。
///
/// 别的任务的 id 能填进 `/imports/{id}/...`，而它们的 `result_summary` 是
/// 别的形状。放行会让「拿错 id」看起来像「这次导入没有失败项」。
#[tokio::test]
async fn a_task_run_of_another_kind_is_404() {
    let db = TestDb::require().await;
    let run_id = seed_run(&db, "image_search_index", Some(r#"{"failed_files":[]}"#)).await;

    let error = svc(&db)
        .list_failed_items(run_id)
        .await
        .expect_err("必须 404");
    assert_eq!(error.status, 404);
    assert_eq!(error.code(), "import_task_not_found");
}

/// 存在、但**没有**失败项 → 200 + 空列表（与上一条的区别就是这个）。
#[tokio::test]
async fn a_run_without_failures_is_an_empty_list() {
    let db = TestDb::require().await;
    let run_id = seed_run(&db, "library_import", None).await;
    assert!(svc(&db)
        .list_failed_items(run_id)
        .await
        .expect("可读")
        .is_empty());

    // 摘要里没有这个键（比如只写了计数）同样是空列表。
    let run_id = seed_run(&db, "library_import", Some(r#"{"imported_count":3}"#)).await;
    assert!(svc(&db)
        .list_failed_items(run_id)
        .await
        .expect("可读")
        .is_empty());
}

/// 存进去的失败项按存储形状投影回来，顺序与存储一致；宿主内部字段不外发。
#[tokio::test]
async fn the_stored_failures_come_back_shaped() {
    let db = TestDb::require().await;
    let summary = json!({
        "imported_count": 1,
        "failed_files": [
            failed_file("a", "metadata_fetch_failed", "pending"),
            failed_file("b", "file_too_small", "resolved"),
        ],
    })
    .to_string();
    let run_id = seed_run(&db, "library_import", Some(&summary)).await;

    let items = svc(&db).list_failed_items(run_id).await.expect("可读");
    assert_eq!(items.len(), 2);

    // 顺序不能重排：那通常是失败发生的顺序，UI 直接照它渲染。
    assert_eq!(items[0].id, "a");
    assert_eq!(items[1].id, "b");

    assert_eq!(items[0].reason, "metadata_fetch_failed");
    assert_eq!(items[0].kind, "file");
    assert_eq!(items[0].size_bytes, 2048);
    assert!(items[0].is_video);
    assert!(
        items[0].can_manual_search,
        "pending + 视频 + jav + 可修原因 -> 可搜"
    );
    assert!(
        !items[1].can_manual_search,
        "已解决的条目没有搜索入口（而且原因也不在可修的两个里）"
    );

    // `media_kind` / `source_ref` / `library_id` 是宿主内部字段。
    let raw = serde_json::to_value(&items[0]).expect("可序列化");
    for hidden in ["media_kind", "source_ref", "library_id", "name", "path"] {
        assert!(raw.get(hidden).is_none(), "{hidden} 不该出现在响应里");
    }
}

/// 摘要形状读不出来 → **500**（不是「没有失败项」）。
///
/// 「这一趟全失败了」与「一切正常」长得一模一样是这条路径最坏的失效方向。
#[tokio::test]
async fn an_unreadable_summary_is_a_500() {
    let db = TestDb::require().await;
    let run_id = seed_run(&db, "library_import", Some(r#"{"failed_files":{}}"#)).await;

    let error = svc(&db)
        .list_failed_items(run_id)
        .await
        .expect_err("必须 500");
    assert_eq!(error.status, 500);
    assert_eq!(error.code(), "internal_error");
}
