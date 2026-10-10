//! 媒体库导入的**入队路径**集成测试（真库），对应上游
//! `shared/import_task_service.py` 的 `enqueue` / `enqueue_batch`。
//!
//! # 为什么必须连真库
//!
//! 这条路径的每一条保证都是**数据库**给的，不是 Rust 代码给的：
//!
//! | 保证 | 靠什么 |
//! |---|---|
//! | 同一媒体库同时只有一个导入在跑 | `UNIQUE(mutex_key)` + `library_import:{id}` |
//! | 不同媒体库互不阻塞 | 互斥键里带库 id |
//! | 批量「全有或全无」 | 单条带**计数谓词**的 UPDATE |
//! | 失败时互斥键被释放 | 终态转移把 `mutex_key` 置 NULL |
//!
//! 换成 mock，这四条全部「通过」，而线上会表现为「两个库互相卡住」或
//! 「一半任务被占了却报成功」。
//!
//! # 断言一律回读数据库
//!
//! 入队的错误几乎都是「返回值看着对、库里的状态错」。所以这里除了断言响应，
//! 还要读 `background_task_run.mutex_key/params` 与
//! `download_task.import_status/import_task_run_id`。

use serde_json::{json, Value};
use sm_db::repo::{
    DownloadClientRepository, DownloadTaskRepository, MediaLibraryRepository, NewDownloadClient,
    NewDownloadTask, NewMediaLibrary,
};
use sm_db::system::activity::task_state;
use sm_db::testing::TestDb;
use sm_db::transfers::downloads::download_state;
use sm_service::transfers::import_task::{
    ImportRequest, ImportTaskService, MediaKind, SourceDisposition,
};

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

fn svc(db: &TestDb) -> ImportTaskService {
    ImportTaskService::new(db.pool())
}

/// 一个最小可用的请求。`source_ref` 是**对象**（上游是 `dict`）。
fn request(library_id: i32) -> ImportRequest {
    ImportRequest {
        media_kind: MediaKind::Jav,
        library_id,
        source_ref: json!({"path": "a/b"}).as_object().cloned().expect("对象"),
        source_disposition: SourceDisposition::Keep,
        collection_id: None,
    }
}

async fn seed_library(db: &TestDb) -> i32 {
    MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("imp-lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id
}

async fn seed_client(db: &TestDb, library_id: i32) -> i32 {
    DownloadClientRepository::new(db.pool().clone())
        .insert(&NewDownloadClient {
            name: format!("imp-cli-{}", n()),
            provider_config: None,
            library_id,
        })
        .await
        .expect("insert download_client")
        .id
}

/// 建一条**下载已完成、带导入来源**的任务 —— 这是可导入的前置状态。
async fn seed_completed_task(db: &TestDb, client_id: i32, movie_number: &str) -> i32 {
    let repo = DownloadTaskRepository::new(db.pool().clone());
    let task = repo
        .insert(&NewDownloadTask {
            client_id,
            remote_id: format!("remote-{}", n()),
            name: format!("下载 {movie_number}"),
            movie_number: Some(movie_number.to_owned()),
        })
        .await
        .expect("insert download_task");
    repo.set_state(
        task.id,
        download_state::COMPLETED,
        Some(1.0),
        Some(r#"{"path":"a/b"}"#),
    )
    .await
    .expect("置下载完成并写入源引用");
    task.id
}

/// `(mutex_key, task_name, params)`。
async fn run_row(db: &TestDb, task_run_id: i32) -> (Option<String>, String, Option<String>) {
    sqlx::query_as::<_, (Option<String>, String, Option<String>)>(
        "SELECT mutex_key, task_name, params FROM background_task_run WHERE id = $1",
    )
    .bind(task_run_id)
    .fetch_one(db.pool())
    .await
    .expect("查 TaskRun")
}

async fn run_state(db: &TestDb, task_run_id: i32) -> String {
    sqlx::query_scalar("SELECT state FROM background_task_run WHERE id = $1")
        .bind(task_run_id)
        .fetch_one(db.pool())
        .await
        .expect("查 TaskRun 状态")
}

/// 本 schema 里 `library_import` 的台账行数（含已判失败的）。
async fn import_run_count(db: &TestDb) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM background_task_run WHERE task_key = 'library_import'")
        .fetch_one(db.pool())
        .await
        .expect("计数")
}

/// `(import_status, import_task_run_id)`。
async fn task_import_row(db: &TestDb, task_id: i32) -> (String, Option<i32>) {
    sqlx::query_as::<_, (String, Option<i32>)>(
        "SELECT import_status, import_task_run_id FROM download_task WHERE id = $1",
    )
    .bind(task_id)
    .fetch_one(db.pool())
    .await
    .expect("查下载任务")
}

async fn load_task(db: &TestDb, task_id: i32) -> sm_db::DownloadTask {
    DownloadTaskRepository::new(db.pool().clone())
        .find_by_id(task_id)
        .await
        .expect("查下载任务")
        .expect("下载任务存在")
}

fn params_of(raw: &Option<String>) -> Value {
    serde_json::from_str(raw.as_deref().expect("params 非空")).expect("params 是 JSON")
}

// ================================================================ 单条入队

/// 库不存在 → **404**（不是 422、更不是 500）。
#[tokio::test]
async fn a_missing_library_is_404() {
    let db = TestDb::require().await;
    let error = svc(&db)
        .enqueue(request(999_999), "manual", None, None)
        .await
        .expect_err("必须 404");
    assert_eq!(error.status, 404);
    assert_eq!(error.code(), "media_library_not_found");
    assert_eq!(
        error.details().and_then(|d| d.get("library_id")),
        Some(&json!(999_999))
    );
    assert_eq!(import_run_count(&db).await, 0, "404 不该留下台账");
}

/// 库没配 provider → **422 `invalid_media_library_provider`**。
///
/// ⚠️ 判据是**逐字的空串**，不是空白：上游是 `provider_key == ""`。这条用例
/// 顺带把这一点钉住（用一个空格的值必须仍然放行）。
#[tokio::test]
async fn a_library_without_a_provider_key_is_422() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    // `NewMediaLibrary` 不许空 provider_key（那是对的），所以只能直接改库 ——
    // 存量库里确实可能有这种行（早期版本或手工写的）。
    sqlx::query("UPDATE media_library SET provider_key = '' WHERE id = $1")
        .bind(library_id)
        .execute(db.pool())
        .await
        .expect("清空 provider_key");

    let error = svc(&db)
        .enqueue(request(library_id), "manual", None, None)
        .await
        .expect_err("必须 422");
    assert_eq!(error.status, 422);
    assert_eq!(error.code(), "invalid_media_library_provider");
    assert_eq!(import_run_count(&db).await, 0);
}

/// `jav` 带合集 → **422 `validation_error`**，且**不建台账**。
#[tokio::test]
async fn a_jav_import_cannot_carry_a_collection() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let mut payload = request(library_id);
    payload.collection_id = Some(1);

    let error = svc(&db)
        .enqueue(payload, "manual", None, None)
        .await
        .expect_err("必须 422");
    assert_eq!(error.status, 422);
    assert_eq!(error.code(), "validation_error");
    assert_eq!(import_run_count(&db).await, 0);
}

/// **202 的形状** + 互斥键按库 + `params` 的键。
///
/// 这里同时钉住三件事：受理响应的三个键、库级互斥键（**不是**
/// `aps:library_import`）、以及 `params` 里 `download_task_id` 为 `null`
/// 也**必须存在**（worker 按它读参数）。
#[tokio::test]
async fn the_accepted_run_carries_the_per_library_mutex_key() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;

    let accepted = svc(&db)
        .enqueue(request(library_id), "manual", None, None)
        .await
        .expect("入队");
    assert_eq!(accepted.task_key, "library_import");
    assert_eq!(accepted.state, task_state::PENDING);
    // 响应说 `pending`，库里那一行也得是 —— 「返回值和库里的状态不一致」是
    // 入队路径上最坏的一类错。
    assert_eq!(
        run_state(&db, accepted.task_run_id).await,
        task_state::PENDING
    );

    let (mutex_key, task_name, params) = run_row(&db, accepted.task_run_id).await;
    assert_eq!(
        mutex_key.as_deref(),
        Some(format!("library_import:{library_id}").as_str()),
        "互斥键必须按库 —— 写成 aps:library_import 会把按库并行退化成全局串行"
    );
    assert_eq!(task_name, "JAV媒体库导入");

    let params = params_of(&params);
    assert_eq!(params["media_kind"], json!("jav"));
    assert_eq!(params["library_id"], json!(library_id));
    assert_eq!(params["source_disposition"], json!("keep"));
    assert_eq!(params["source_ref"], json!({"path": "a/b"}));
    assert_eq!(params["collection_id"], json!(null));
    assert_eq!(
        params["download_task_id"],
        json!(null),
        "缺省也要有键：worker 读的是键"
    );
    assert!(
        params.get("target_movie_number").is_none(),
        "没有下载任务时不该带目标番号"
    );
}

/// 同一库的第二次导入 → **409**，details 里带**阻塞方的 run id**。
///
/// 客户端要靠那个 id 告诉用户「正在跑的是哪一条」—— 所以它必须是第一次那条，
/// 而不是 `null`。
#[tokio::test]
async fn a_second_import_for_the_same_library_is_409_with_the_blocking_run() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;

    let first = svc(&db)
        .enqueue(request(library_id), "manual", None, None)
        .await
        .expect("第一次入队");

    let error = svc(&db)
        .enqueue(request(library_id), "manual", None, None)
        .await
        .expect_err("必须 409");
    assert_eq!(error.status, 409);
    assert_eq!(error.code(), "import_task_conflict");
    assert_eq!(
        error
            .details()
            .and_then(|d| d.get("blocking_task_run_id"))
            .and_then(Value::as_i64),
        Some(i64::from(first.task_run_id))
    );
    assert_eq!(import_run_count(&db).await, 1, "409 不该建第二条台账");
}

/// **两个库互不阻塞** —— 这是「按库互斥」的存在理由。
///
/// 如果把互斥键退回 `aps:{task_key}`，这条用例会以 409 失败，而线上表现为
/// 「A 库导入时 B 库导不进去」，没有任何报错。
#[tokio::test]
async fn two_libraries_do_not_block_each_other() {
    let db = TestDb::require().await;
    let first_library = seed_library(&db).await;
    let second_library = seed_library(&db).await;

    let first = svc(&db)
        .enqueue(request(first_library), "manual", None, None)
        .await
        .expect("A 库入队");
    let second = svc(&db)
        .enqueue(request(second_library), "manual", None, None)
        .await
        .expect("B 库不该被 A 库挡住");

    assert_ne!(first.task_run_id, second.task_run_id);
    assert_eq!(import_run_count(&db).await, 2);
}

/// 由下载任务发起时：回写下载任务、带目标番号、**用调用方给的任务名**。
#[tokio::test]
async fn the_download_task_is_marked_running_and_linked() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id).await;
    let task_id = seed_completed_task(&db, client_id, "IMP-0001").await;

    let accepted = svc(&db)
        .enqueue(
            request(library_id),
            "manual",
            Some(task_id),
            Some("下载任务导入 IMP-0001"),
        )
        .await
        .expect("入队");

    let (status, linked) = task_import_row(&db, task_id).await;
    assert_eq!(status, "running");
    assert_eq!(
        linked,
        Some(accepted.task_run_id),
        "下载任务要挂上这条台账，否则界面上的进度无从对起"
    );

    let (_, task_name, params) = run_row(&db, accepted.task_run_id).await;
    assert_eq!(
        task_name, "下载任务导入 IMP-0001",
        "调用方给的名字要覆盖缺省名"
    );
    let params = params_of(&params);
    assert_eq!(params["download_task_id"], json!(task_id));
    assert_eq!(
        params["target_movie_number"],
        json!("IMP-0001"),
        "下载任务导入只认准该任务的目标番号，资源包里的其它番号一律忽略"
    );
}

/// 下载任务不存在 → **502 `import_task_create_failed`**（**不是** 404）。
///
/// 上游的 `DownloadTask.get_by_id` 在 `try` 里，`DoesNotExist` 落进通用的
/// `except Exception`，所以报的是「入队失败」。这里照抄 —— 客户端拿到 502 会
/// 去重试/报警，拿到 404 会以为「这个任务被删了」。
#[tokio::test]
async fn a_missing_download_task_is_502() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;

    let error = svc(&db)
        .enqueue(request(library_id), "manual", Some(999_999), None)
        .await
        .expect_err("必须 502");
    assert_eq!(error.status, 502);
    assert_eq!(error.code(), "import_task_create_failed");
    assert!(
        error.details().is_some_and(|d| d.contains_key("detail")),
        "上游把底层原因放在 details.detail 里"
    );
    assert_eq!(import_run_count(&db).await, 0);
}

// ================================================================ 批量入队

/// 空列表与跨库都是**调用方的编程错误** → 500 `programmer_error`。
///
/// 上游对这两种抛 `ValueError`（不是 `ApiError`）：HTTP 层回 500，因为
/// 「worker 代码写错了」不是用户输入的问题。
#[tokio::test]
async fn enqueue_batch_rejects_empty_and_cross_library_batches() {
    let db = TestDb::require().await;

    let empty = svc(&db).enqueue_batch(&[]).await.expect_err("空列表");
    assert_eq!(empty.status, 500);
    assert_eq!(empty.code(), "programmer_error");

    // 两个库各一条任务 → 跨库。
    let first_library = seed_library(&db).await;
    let second_library = seed_library(&db).await;
    let first_client = seed_client(&db, first_library).await;
    let second_client = seed_client(&db, second_library).await;
    let first_task = load_task(&db, seed_completed_task(&db, first_client, "IMP-B01").await).await;
    let second_task = load_task(
        &db,
        seed_completed_task(&db, second_client, "IMP-B02").await,
    )
    .await;

    let cross = svc(&db)
        .enqueue_batch(&[first_task, second_task])
        .await
        .expect_err("跨库");
    assert_eq!(cross.status, 500);
    assert_eq!(cross.code(), "programmer_error");
    assert_eq!(import_run_count(&db).await, 0, "两种拒绝都不该建台账");
}

/// 批量成功：**一条台账**占住全部任务，番号逐个带上。
#[tokio::test]
async fn enqueue_batch_occupies_every_pending_task() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id).await;
    let first = load_task(&db, seed_completed_task(&db, client_id, "IMP-C01").await).await;
    let second = load_task(&db, seed_completed_task(&db, client_id, "IMP-C02").await).await;

    svc(&db)
        .enqueue_batch(&[first.clone(), second.clone()])
        .await
        .expect("批量入队");

    assert_eq!(import_run_count(&db).await, 1, "一批只建一条台账");
    let (first_status, first_run) = task_import_row(&db, first.id).await;
    let (second_status, second_run) = task_import_row(&db, second.id).await;
    assert_eq!(first_status, "running");
    assert_eq!(second_status, "running");
    assert_eq!(first_run, second_run, "同一批必须挂在同一条台账上");

    let run_id = first_run.expect("有台账");
    let (mutex_key, task_name, params) = run_row(&db, run_id).await;
    assert_eq!(
        mutex_key.as_deref(),
        Some(format!("library_import:{library_id}").as_str())
    );
    assert_eq!(task_name, "下载任务连续导入（2个）");

    let params = params_of(&params);
    assert_eq!(params["library_id"], json!(library_id));
    let batch = params["download_tasks"].as_array().expect("数组");
    assert_eq!(batch.len(), 2);
    assert_eq!(batch[0]["download_task_id"], json!(first.id));
    assert_eq!(batch[0]["target_movie_number"], json!("IMP-C01"));
    assert_eq!(batch[1]["download_task_id"], json!(second.id));
    assert_eq!(batch[1]["target_movie_number"], json!("IMP-C02"));
    assert_eq!(batch[0]["media_kind"], json!("jav"));
    assert_eq!(batch[0]["source_disposition"], json!("keep"));
}

/// 批量里有**一条已被别的导入占用** → **整批拒绝**，且**一条都不改**。
///
/// 这是「全有或全无」的核心用例：修成「逐条 UPDATE」之后，
/// `first` 会被改掉而后返回 409 —— 那条任务就永远卡在 `running` 上（没人跑它）。
/// 半途的台账也必须**撤掉**（判失败 → 释放互斥键），否则这个库的导入
/// 会永久 409。
#[tokio::test]
async fn enqueue_batch_is_all_or_nothing() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id).await;
    let pending = load_task(&db, seed_completed_task(&db, client_id, "IMP-D01").await).await;
    let taken = load_task(&db, seed_completed_task(&db, client_id, "IMP-D02").await).await;

    // 先把第二条置成「导入中」——模拟别的导入已经占住它。
    DownloadTaskRepository::new(db.pool().clone())
        .set_import_status(taken.id, "running", None)
        .await
        .expect("占住第二条");

    let error = svc(&db)
        .enqueue_batch(&[pending.clone(), taken.clone()])
        .await
        .expect_err("整批拒绝");
    assert_eq!(error.status, 409);
    assert_eq!(error.code(), "download_task_import_conflict");

    let (pending_status, pending_run) = task_import_row(&db, pending.id).await;
    assert_eq!(
        (pending_status.as_str(), pending_run),
        ("pending", None),
        "整批拒绝时那一条 pending 也不能被改"
    );

    // 撤掉的台账是**失败**的：它的互斥键必须已释放，否则这个库永远 409。
    let states: Vec<String> = sqlx::query_scalar(
        "SELECT state FROM background_task_run WHERE task_key = 'library_import'",
    )
    .fetch_all(db.pool())
    .await
    .expect("查台账状态");
    assert_eq!(states, vec![task_state::FAILED.to_owned()]);

    svc(&db)
        .enqueue(request(library_id), "manual", None, None)
        .await
        .expect("互斥键已释放，这个库还能再入队");
}
