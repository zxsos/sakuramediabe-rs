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

use std::sync::Arc;

use serde_json::json;
use sm_db::repo::{BackgroundTaskRunRepository, NewTaskRun};
use sm_db::testing::TestDb;
use sm_service::catalog::metadata_source::MetadataSourceService;
use sm_service::catalog::movie_metadata_search::MovieMetadataSearchService;
use sm_service::system::config::ConfigService;
use sm_service::transfers::import_task::{ImportFailedItemRetryRequest, ImportTaskService};

/// 一个**不存在**的配置文件路径：`ConfigService::snapshot` 对不存在的路径返回
/// 默认配置，所以候选校验（只需要「哪些插件启用」= 空）能离线跑。
fn temp_config() -> ConfigService {
    ConfigService::new(std::env::temp_dir().join(format!("sm-retry-cfg-{}.toml", n())))
}

/// 校验候选的搜索服务。**没有配 JavDB provider** —— `javdb:` 候选不需要它
/// （插件那一支才走闭包），而重试路径只做**格式 + 插件启用**校验。
fn search() -> MovieMetadataSearchService {
    MovieMetadataSearchService::new(
        temp_config(),
        Arc::new(MetadataSourceService::new(Vec::new(), None)),
    )
}

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

// ================================================================ 重试

/// 台账行 + `state` + `result_summary`：重试路径要的是**终态**任务。
///
/// 与 [`seed_run`] 同样直写列（收口路径会发通知，与本用例无关）。状态写
/// `completed`：上游 `_ensure_retryable_task` 的白名单只有 `completed` /
/// `failed`。
async fn seed_finished_run(db: &TestDb, result_summary: &str) -> i32 {
    let run_id = seed_run(db, "library_import", Some(result_summary)).await;
    sqlx::query("UPDATE background_task_run SET state = $1 WHERE id = $2")
        .bind(sm_db::system::task_state::COMPLETED)
        .bind(run_id)
        .execute(db.pool())
        .await
        .expect("置终态");
    run_id
}

/// 一个**有 provider_key** 的媒体库（重试要求库配了 storage provider）。
async fn seed_library(db: &TestDb, provider_key: &str) -> i32 {
    let (id,): (i32,) = sqlx::query_as(
        "INSERT INTO media_library (name, provider_key) VALUES ($1, $2) RETURNING id",
    )
    .bind(format!("重试测试库-{}", n()))
    .bind(provider_key)
    .fetch_one(db.pool())
    .await
    .expect("insert media_library");
    id
}

/// 一条失败项 + 它所属的库，返回 `(任务行 id, 失败项 id, 库 id)`。
async fn seed_retry_case(db: &TestDb, provider_key: &str) -> (i32, String, i32) {
    let library_id = seed_library(db, provider_key).await;
    let item_id = "item-1".to_owned();
    let mut item = failed_file(&item_id, "movie_number_not_found", "pending");
    item["library_id"] = json!(library_id);
    let summary = json!({ "failed_files": [item] }).to_string();
    let run_id = seed_finished_run(db, &summary).await;
    (run_id, item_id, library_id)
}

/// 读回某条失败项（原始存储形状）。
async fn stored_item(db: &TestDb, run_id: i32, item_id: &str) -> serde_json::Value {
    let (summary,): (Option<String>,) =
        sqlx::query_as("SELECT result_summary FROM background_task_run WHERE id = $1")
            .bind(run_id)
            .fetch_one(db.pool())
            .await
            .expect("读 result_summary");
    let parsed: serde_json::Value =
        serde_json::from_str(summary.as_deref().expect("有摘要")).expect("摘要可解析");
    parsed["failed_files"]
        .as_array()
        .expect("是数组")
        .iter()
        .find(|item| item["id"] == json!(item_id))
        .cloned()
        .expect("那条失败项还在")
}

fn retry_request(candidate_id: &str) -> ImportFailedItemRetryRequest {
    ImportFailedItemRetryRequest {
        candidate_id: candidate_id.to_owned(),
    }
}

/// 合法候选：`javdb:<番号>:<javdb id>`。
fn candidate(movie_number: &str) -> String {
    MovieMetadataSearchService::javdb_candidate_id(movie_number, "javdb-id-1")
}

/// ★ 主路径：入队一条重试 + 把那一条标成 `queued` 并指回新任务。
#[tokio::test]
async fn a_pending_item_is_requeued_and_marked_queued() {
    let db = TestDb::require().await;
    let (run_id, item_id, library_id) = seed_retry_case(&db, "local").await;

    let accepted = svc(&db)
        .enqueue_failed_item_retry(
            &search(),
            run_id,
            &item_id,
            &retry_request(&candidate("ABP-123")),
        )
        .await
        .expect("应当入队");

    assert_ne!(accepted.task_run_id, run_id, "重试是**另一条**任务");
    assert_eq!(accepted.task_key, "library_import");
    assert_eq!(accepted.state, sm_db::system::task_state::PENDING);

    // 新任务：同库互斥、手动触发、自己的名字、参数里带着整条失败项。
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());
    let retry_run = repo
        .find_by_id(accepted.task_run_id)
        .await
        .expect("查得到")
        .expect("新任务存在");
    assert_eq!(retry_run.task_key, "library_import");
    assert_eq!(retry_run.task_name, "JAV失败项重试导入");
    assert_eq!(retry_run.trigger_type, "manual");
    assert_eq!(
        retry_run.mutex_key.as_deref(),
        Some(format!("library_import:{library_id}").as_str()),
        "互斥键按库 —— 同库不能两个导入并行"
    );
    let params: serde_json::Value =
        serde_json::from_str(retry_run.params.as_deref().expect("有参数")).expect("参数可解析");
    assert_eq!(params["mode"], "retry_failed_file");
    assert_eq!(params["original_task_run_id"], json!(run_id));
    assert_eq!(params["failure_item_id"], json!(item_id));
    assert_eq!(params["candidate_id"], json!(candidate("ABP-123")));
    assert_eq!(
        params["failure_item"]["source_ref"]["path"],
        json!("人妻/item-1.mkv"),
        "源信息随任务走，worker 不回头读原任务"
    );

    // 回写：那一条变成 queued 并指向新任务。
    let item = stored_item(&db, run_id, &item_id).await;
    assert_eq!(item["state"], "queued");
    assert_eq!(item["retry_task_run_id"], json!(accepted.task_run_id));
    assert_eq!(item["last_retry_error"], serde_json::Value::Null);
    // 同一份摘要里的**其它键**不能被这次回写抹掉。
    assert_eq!(item["reason"], json!("movie_number_not_found"));
}

/// 任务还没跑完 → **409 `import_task_not_finished`**。
///
/// 与「失败项不在待处理状态」是两回事：这里连失败项都还没定下来。
#[tokio::test]
async fn an_unfinished_task_run_cannot_retry() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db, "local").await;
    let mut item = failed_file("item-1", "movie_number_not_found", "pending");
    item["library_id"] = json!(library_id);
    let summary = json!({ "failed_files": [item] }).to_string();
    // `seed_run` 落的是默认态（pending）。
    let run_id = seed_run(&db, "library_import", Some(&summary)).await;

    let error = svc(&db)
        .enqueue_failed_item_retry(
            &search(),
            run_id,
            "item-1",
            &retry_request(&candidate("ABP-1")),
        )
        .await
        .expect_err("必须 409");
    assert_eq!(error.status, 409);
    assert_eq!(error.code(), "import_task_not_finished");
}

/// 那一条已经在重试 / 已解决 → **409 `failed_item_not_pending`**。
#[tokio::test]
async fn an_item_that_is_not_pending_is_rejected() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db, "local").await;
    let mut item = failed_file("item-1", "movie_number_not_found", "queued");
    item["library_id"] = json!(library_id);
    let run_id = seed_finished_run(&db, &json!({ "failed_files": [item] }).to_string()).await;

    let error = svc(&db)
        .enqueue_failed_item_retry(
            &search(),
            run_id,
            "item-1",
            &retry_request(&candidate("ABP-1")),
        )
        .await
        .expect_err("必须 409");
    assert_eq!(error.status, 409);
    assert_eq!(error.code(), "failed_item_not_pending");
}

/// 不是「JAV 视频 + 可人工处理的原因」→ **409 `failed_item_search_unavailable`**。
///
/// 与上一条**不同的码**：这条没人在处理，是它本来就不适合人工修（比如整集）。
#[tokio::test]
async fn an_item_of_another_kind_is_rejected() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db, "local").await;
    let mut item = failed_file("item-1", "movie_number_not_found", "pending");
    item["library_id"] = json!(library_id);
    item["media_kind"] = json!("video");
    let run_id = seed_finished_run(&db, &json!({ "failed_files": [item] }).to_string()).await;

    let error = svc(&db)
        .enqueue_failed_item_retry(
            &search(),
            run_id,
            "item-1",
            &retry_request(&candidate("ABP-1")),
        )
        .await
        .expect_err("必须 409");
    assert_eq!(error.status, 409);
    assert_eq!(error.code(), "failed_item_search_unavailable");
}

/// `source_ref` 没了（暂存区被清理）→ **409 `failed_item_source_unavailable`**。
#[tokio::test]
async fn a_vanished_source_ref_is_rejected() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db, "local").await;
    let mut item = failed_file("item-1", "movie_number_not_found", "pending");
    item["library_id"] = json!(library_id);
    item["source_ref"] = json!({});
    let run_id = seed_finished_run(&db, &json!({ "failed_files": [item] }).to_string()).await;

    let error = svc(&db)
        .enqueue_failed_item_retry(
            &search(),
            run_id,
            "item-1",
            &retry_request(&candidate("ABP-1")),
        )
        .await
        .expect_err("必须 409");
    assert_eq!(error.status, 409);
    assert_eq!(error.code(), "failed_item_source_unavailable");
}

/// ★ 候选先校验：**坏候选 + 不存在的任务 → 422**（不是 404）。
///
/// 这一条钉住顺序。反过来（先查台账）用户会看到「任务不存在」，改完任务 id
/// 再撞 422 —— 而真正的问题一直是那个候选。
#[tokio::test]
async fn a_broken_candidate_is_422_before_the_ledger_is_read() {
    let db = TestDb::require().await;

    let error = svc(&db)
        .enqueue_failed_item_retry(&search(), 999_999, "item-1", &retry_request("这不是候选"))
        .await
        .expect_err("必须 422");
    assert_eq!(error.status, 422, "不是 404 —— 顺序是契约");
    assert_eq!(
        error.code(),
        "invalid_metadata_candidate",
        "候选专用码，不是泛化的 validation_error"
    );
}

/// 库不存在 → **404 `media_library_not_found`**。
#[tokio::test]
async fn a_missing_library_is_404() {
    let db = TestDb::require().await;
    let mut item = failed_file("item-1", "movie_number_not_found", "pending");
    item["library_id"] = json!(999_999);
    let run_id = seed_finished_run(&db, &json!({ "failed_files": [item] }).to_string()).await;

    let error = svc(&db)
        .enqueue_failed_item_retry(
            &search(),
            run_id,
            "item-1",
            &retry_request(&candidate("ABP-1")),
        )
        .await
        .expect_err("必须 404");
    assert_eq!(error.status, 404);
    assert_eq!(error.code(), "media_library_not_found");
}

/// 库没有 `provider_key` → **422 `invalid_media_library_provider`**。
///
/// 与上一条同一个 `require_library` 里的两道门，码与状态都不同。
#[tokio::test]
async fn a_library_without_provider_key_is_422() {
    let db = TestDb::require().await;
    let (run_id, item_id, _) = seed_retry_case(&db, "").await;

    let error = svc(&db)
        .enqueue_failed_item_retry(
            &search(),
            run_id,
            &item_id,
            &retry_request(&candidate("ABP-1")),
        )
        .await
        .expect_err("必须 422");
    assert_eq!(error.status, 422);
    assert_eq!(error.code(), "invalid_media_library_provider");
}

/// 同库已有导入在跑 → **409 `import_task_conflict`**，且**不留**半条任务。
///
/// 第二条重试撞上互斥键；这时既不能改失败项状态，也不能留下一条永远不跑的
/// 任务行（`ConflictPolicy::Raise` 在仓储层返回 `Skipped`）。
#[tokio::test]
async fn a_second_retry_of_the_same_library_is_a_conflict() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db, "local").await;
    let mut first = failed_file("item-1", "movie_number_not_found", "pending");
    first["library_id"] = json!(library_id);
    let mut second = failed_file("item-2", "movie_number_not_found", "pending");
    second["library_id"] = json!(library_id);
    let run_id =
        seed_finished_run(&db, &json!({ "failed_files": [first, second] }).to_string()).await;
    let service = svc(&db);

    service
        .enqueue_failed_item_retry(
            &search(),
            run_id,
            "item-1",
            &retry_request(&candidate("ABP-1")),
        )
        .await
        .expect("第一条应当入队");

    let error = service
        .enqueue_failed_item_retry(
            &search(),
            run_id,
            "item-2",
            &retry_request(&candidate("ABP-2")),
        )
        .await
        .expect_err("必须 409");
    assert_eq!(error.status, 409);
    assert_eq!(error.code(), "import_task_conflict");

    // 第二条失败项**没有**被标成 queued（失败发生在回写之前）。
    assert_eq!(stored_item(&db, run_id, "item-2").await["state"], "pending");
    // 同库只有一条活动任务。
    let (count,): (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM background_task_run WHERE mutex_key = $1 AND state IN ('pending', 'running')",
    )
    .bind(format!("library_import:{library_id}"))
    .fetch_one(db.pool())
    .await
    .expect("计数");
    assert_eq!(count, 1, "冲突不留半条任务");
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
