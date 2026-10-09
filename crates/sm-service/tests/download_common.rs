//! transfers 域「取数 + 校验」的连库对拍（上游 `downloads/common.py`，229 行）。
//!
//! # 为什么这些函数必须有连库用例
//!
//! 它们的产物是**错误契约 + 投影形状**，而两者都只有连库才测得出来：
//!
//! * 404/422 的**码**与 `details` 里的键（`client_id` / `library_id` / `task_id`
//!   / `indexer_name`）是客户端分流与高亮输入框的依据；
//! * 投影的字段类型（`movie_number: Option<String>`、`client_id: i32`）与
//!   DDL 对齐 —— 骨架期这几处是错的（`i64`、非空 String），只有真库能证伪。
//!
//! # `DownloadClientRow.provider_config` 是**对象**，不是文本
//!
//! 库里那列是不透明 JSON **文本**（可能 `NULL`），句柄侧要的是对象。转换规则
//! 照上游 `client.provider_config or {}`：NULL / 脏数据都当空对象 —— 本文件用
//! 一条把 `provider_config` 写成 `NULL` 的行钉住「不会变成 `null`」。

use sm_db::repo::{
    DownloadClientRepository, DownloadTaskRepository, IndexerDownloadClientRepository,
    IndexerRepository, MediaLibraryRepository, NewDownloadClient, NewDownloadTask, NewIndexer,
    NewMediaLibrary,
};
use sm_db::testing::TestDb;
use sm_service::transfers::download_common::{
    list_indexer_clients, require_client, require_indexer, require_library, require_task,
};

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// 建一个媒体库，返回 id（下载器必须挂在某个库下 —— 有外键）。
async fn seed_library(db: &TestDb) -> i32 {
    MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{}", unique()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("插入 media_library")
        .id
}

/// 建一个下载器，返回 (id, library_id)。
async fn seed_client(db: &TestDb, library_id: i32, config: Option<&str>) -> i32 {
    DownloadClientRepository::new(db.pool().clone())
        .insert(&NewDownloadClient {
            name: format!("client-{}", unique()),
            provider_config: config.map(str::to_owned),
            library_id,
        })
        .await
        .expect("插入 download_client")
        .id
}

/// ★ 查不到 → **404** + details `{client_id}`；查到 → 投影带 `library_id`
/// 且 `provider_config` 是**对象**。
#[tokio::test]
async fn require_client_is_404_with_the_id_and_projects_an_object_config() {
    let db = TestDb::require().await;
    let missing = require_client(db.pool(), 999_999)
        .await
        .expect_err("不存在的客户端");
    assert_eq!(missing.status, 404);
    assert_eq!(missing.code(), "download_client_not_found");
    assert_eq!(
        missing.details().and_then(|d| d.get("client_id")),
        Some(&serde_json::json!(999_999))
    );

    let library_id = seed_library(&db).await;
    // `provider_config = NULL` 的行：句柄侧要 `{}`，不是 `null`。
    let client_id = seed_client(&db, library_id, None).await;
    let row = require_client(db.pool(), client_id)
        .await
        .expect("存在的客户端");
    assert_eq!(row.id, client_id);
    assert_eq!(row.library_id, library_id, "句柄要用它查库记录");
    assert_eq!(
        row.provider_config,
        serde_json::json!({}),
        "NULL 当空对象（上游 `or {{}}`）"
    );
}

/// ★ 媒体库查不到 → **404** + details `{library_id}`；`provider_key` 原样带出。
#[tokio::test]
async fn require_library_is_404_with_the_id_and_keeps_the_provider_key() {
    let db = TestDb::require().await;
    let missing = require_library(db.pool(), 999_999)
        .await
        .expect_err("不存在的库");
    assert_eq!(missing.status, 404);
    assert_eq!(missing.code(), "media_library_not_found");
    assert_eq!(
        missing.details().and_then(|d| d.get("library_id")),
        Some(&serde_json::json!(999_999))
    );

    let library_id = seed_library(&db).await;
    let row = require_library(db.pool(), library_id)
        .await
        .expect("存在的库");
    assert_eq!(row.id, library_id);
    assert_eq!(row.provider_key, "local");
}

/// ★ 任务查不到 → **404** + details `{task_id}`；`movie_number` 可空。
///
/// 「任务早于影片入库」是正常流程（搜索结果先到），所以那一列可空 ——
/// 骨架期投影把它写成非空 `String`，真库上会直接把这条路径堵死。
#[tokio::test]
async fn require_task_is_404_with_the_id_and_allows_a_missing_movie_number() {
    let db = TestDb::require().await;
    let missing = require_task(db.pool(), 999_999)
        .await
        .expect_err("不存在的任务");
    assert_eq!(missing.status, 404);
    assert_eq!(missing.code(), "download_task_not_found");
    assert_eq!(
        missing.details().and_then(|d| d.get("task_id")),
        Some(&serde_json::json!(999_999))
    );

    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id, Some(r#"{"host":"h"}"#)).await;
    let task = DownloadTaskRepository::new(db.pool().clone())
        .insert(&NewDownloadTask {
            client_id,
            remote_id: format!("remote-{}", unique()),
            name: "some release".to_owned(),
            movie_number: None,
        })
        .await
        .expect("插入 download_task");
    let row = require_task(db.pool(), task.id).await.expect("存在的任务");
    assert_eq!(row.id, task.id);
    assert_eq!(row.client_id, client_id);
    assert!(row.movie_number.is_none(), "番号允许为空");
    assert_eq!(row.state, "queued", "DDL 缺省态");
}

/// ★ 索引器：空白名 → **422**（details 用**原始**值）；查不到 → **422**
/// （details 用**归一化后**的值）。
///
/// 这条是上游 `require_indexer`（`:171-188`）与那三个 `require_*` **唯一不同**
/// 的地方：它报 422 而不是 404（是「提交下载」的入口校验，不是资源查询）。
#[tokio::test]
async fn require_indexer_is_422_and_trims_the_name() {
    let db = TestDb::require().await;

    let blank = require_indexer(db.pool(), "   ")
        .await
        .expect_err("空白名字");
    assert_eq!(blank.status, 422);
    assert_eq!(blank.code(), "download_request_indexer_not_found");
    assert_eq!(
        blank.details().and_then(|d| d.get("indexer_name")),
        Some(&serde_json::json!("   ")),
        "空白那次回**原始**值给前端高亮输入框"
    );

    let name = format!("idx-{}", unique());
    IndexerRepository::new(db.pool().clone())
        .insert(&NewIndexer {
            name: name.clone(),
            url: "http://127.0.0.1:9117".to_owned(),
            kind: "pt".to_owned(),
            api_key: None,
        })
        .await
        .expect("插入 indexer");

    // 带空白也能命中（查库前会 trim）。
    let row = require_indexer(db.pool(), &format!("  {name}  "))
        .await
        .expect("前后空白应被归一化");
    assert_eq!(row.name, name);
    assert_eq!(row.kind, "pt", "候选资源要拿它填 indexer_kind");

    let unknown = require_indexer(db.pool(), "  nope  ")
        .await
        .expect_err("查不到");
    assert_eq!(unknown.status, 422);
    assert_eq!(
        unknown.details().and_then(|d| d.get("indexer_name")),
        Some(&serde_json::json!("nope")),
        "查不到时回**归一化后**的名字（与空白那次不同）"
    );
}

/// ★ 绑定列表：取**完整行**、按**关联行 id 升序**、且不串到别的索引器。
///
/// 顺序不是排版 —— 上游 `resolve_preferred_client` 取第一个，所以顺序一变，
/// 种子就提交到另一个下载器上，而全程没有报错。
#[tokio::test]
async fn list_indexer_clients_keeps_binding_order_and_full_rows() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let first = seed_client(&db, library_id, Some(r#"{"host":"first"}"#)).await;
    let second = seed_client(&db, library_id, Some(r#"{"host":"second"}"#)).await;

    let name = format!("idx-{}", unique());
    let indexer = IndexerRepository::new(db.pool().clone())
        .insert(&NewIndexer {
            name: name.clone(),
            url: "http://127.0.0.1:9117".to_owned(),
            kind: "bt".to_owned(),
            api_key: None,
        })
        .await
        .expect("插入 indexer");
    // 刻意的绑定顺序：**后建的那个先绑**。列表必须按绑定顺序（不是 id 顺序）。
    let bindings = IndexerDownloadClientRepository::new(db.pool().clone());
    bindings.bind(indexer.id, second).await.expect("绑定第二个");
    bindings.bind(indexer.id, first).await.expect("绑定第一个");

    // 另一个索引器挂一个客户端，用来验「不会串」。
    let other_name = format!("idx-{}", unique());
    IndexerRepository::new(db.pool().clone())
        .insert(&NewIndexer {
            name: other_name.clone(),
            url: "http://127.0.0.1:9118".to_owned(),
            kind: "bt".to_owned(),
            api_key: None,
        })
        .await
        .expect("插入第二个 indexer");

    let row = require_indexer(db.pool(), &name)
        .await
        .expect("存在的索引器");
    let clients = list_indexer_clients(db.pool(), &row)
        .await
        .expect("绑定列表");
    assert_eq!(
        clients.iter().map(|client| client.id).collect::<Vec<_>>(),
        vec![second, first],
        "按绑定顺序，不是按客户端 id"
    );
    assert_eq!(
        clients[0].provider_config,
        serde_json::json!({"host": "second"}),
        "完整行：`provider_config` 也要带出来（构造句柄要用）"
    );
    assert_eq!(clients[0].library_id, library_id);

    // 没绑定的索引器 → 空列表（合法结果，由 `resolve_preferred_client` 报 422）。
    let lonely = require_indexer(db.pool(), &other_name)
        .await
        .expect("存在的索引器");
    assert!(list_indexer_clients(db.pool(), &lonely)
        .await
        .expect("空绑定列表")
        .is_empty());
}
