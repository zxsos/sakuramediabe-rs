//! 下载器客户端**库侧**两件事的连库对拍（上游 `client_config_service.py`）。
//!
//! | 入口 | 上游 | 本文件盯什么 |
//! |---|---|---|
//! | `list_clients` | `:257-265` | 排序是 `created_at DESC, id DESC`（**最新在前**），不是骨架期注释说的「按 id 升序」|
//! | `delete_client` | `:333-351` | 两道 409 的**码、先后、details**，以及 404 与删除本身 |
//!
//! `create` / `update` / `test_client` 不在这里 —— 它们要 provider seam（阶段二）。

use sm_db::repo::{
    DownloadClientRepository, DownloadTaskRepository, IndexerDownloadClientRepository,
    IndexerRepository, MediaLibraryRepository, NewDownloadClient, NewDownloadTask, NewIndexer,
    NewMediaLibrary,
};
use sm_db::testing::TestDb;
use sm_service::transfers::download_client::{DownloadClientService, DownloadClientUpdateRequest};

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

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

/// ★ 列表：**最新在前**（`created_at DESC, id DESC`），`provider_config` 是对象。
#[tokio::test]
async fn listing_is_newest_first_and_projects_an_object() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let first = seed_client(&db, library_id, None).await;
    let second = seed_client(&db, library_id, Some(r#"{"host":"h"}"#)).await;
    let third = seed_client(&db, library_id, None).await;

    let clients = DownloadClientService::new(db.pool())
        .list_clients()
        .await
        .expect("列出");
    let ids: Vec<i32> = clients.iter().map(|client| client.id).collect();
    assert!(
        ids.starts_with(&[third, second, first]),
        "最新在前（含并列时按 id 降序）：{ids:?}"
    );

    let with_config = clients
        .iter()
        .find(|client| client.id == second)
        .expect("第二个在列表里");
    assert_eq!(
        with_config.provider_config,
        serde_json::json!({"host": "h"})
    );
    assert_eq!(with_config.library_id, library_id);
    let without = clients
        .iter()
        .find(|client| client.id == first)
        .expect("第一个在列表里");
    assert_eq!(
        without.provider_config,
        serde_json::json!({}),
        "NULL → {{}}"
    );
}

/// ★ 删除：不存在 → **404**；干净 → 真的删掉。
#[tokio::test]
async fn deleting_a_clean_client_succeeds_and_a_missing_one_is_404() {
    let db = TestDb::require().await;
    let service = DownloadClientService::new(db.pool());

    let missing = service.delete_client(999_999).await.expect_err("不存在");
    assert_eq!(missing.status, 404);
    assert_eq!(missing.code(), "download_client_not_found");

    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id, None).await;
    service.delete_client(client_id).await.expect("删除");
    assert!(
        DownloadClientRepository::new(db.pool().clone())
            .find_by_id(client_id)
            .await
            .expect("回查")
            .is_none(),
        "行应该没了"
    );
}

/// ★ 两道 409 的**先后与码**（上游 `:335` 先任务、`:342` 后绑定）。
///
/// 两道都建上时，第一道必须是**任务**那条 —— 反过来客户端会先看到「被索引器
/// 绑定」，把绑定删掉后再看到「有任务」，提示与上游不一致。
#[tokio::test]
async fn the_two_conflicts_follow_the_upstream_order() {
    let db = TestDb::require().await;
    let service = DownloadClientService::new(db.pool());
    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id, None).await;

    // 一个索引器绑定它。
    let indexer = IndexerRepository::new(db.pool().clone())
        .insert(&NewIndexer {
            name: format!("idx-{}", unique()),
            url: "http://127.0.0.1:9117".to_owned(),
            kind: "pt".to_owned(),
            api_key: None,
        })
        .await
        .expect("插入 indexer");
    IndexerDownloadClientRepository::new(db.pool().clone())
        .bind(indexer.id, client_id)
        .await
        .expect("绑定");

    // 只有绑定时：被索引器绑定的那道 409。
    let bound = service.delete_client(client_id).await.expect_err("被绑定");
    assert_eq!(bound.status, 409);
    assert_eq!(bound.code(), "download_client_in_use_by_indexers");
    assert_eq!(
        bound.details().and_then(|details| details.get("client_id")),
        Some(&serde_json::json!(client_id))
    );

    // 再加一条任务：这时**第一道**变成任务那条（它先判）。
    let task = DownloadTaskRepository::new(db.pool().clone())
        .insert(&NewDownloadTask {
            client_id,
            remote_id: format!("remote-{}", unique()),
            name: "some release".to_owned(),
            movie_number: None,
        })
        .await
        .expect("插入 download_task");
    let in_use = service.delete_client(client_id).await.expect_err("有任务");
    assert_eq!(in_use.status, 409);
    assert_eq!(
        in_use.code(),
        "download_client_in_use",
        "两道都成立时先报**任务**那条（上游 `:335`）"
    );

    // 「有没有任务」看的是**行在不在**，与状态无关 —— 上游用的是 `exists()`，
    // 不是「有没有在跑的任务」。所以这里不需要改状态：任何一行都拦。
    let still = service
        .delete_client(client_id)
        .await
        .expect_err("仍有任务行");
    assert_eq!(still.code(), "download_client_in_use");

    sqlx::query("DELETE FROM download_task WHERE id = $1")
        .bind(task.id)
        .execute(db.pool())
        .await
        .expect("删任务行");
    let back_to_bound = service.delete_client(client_id).await.expect_err("被绑定");
    assert_eq!(back_to_bound.code(), "download_client_in_use_by_indexers");

    // 解绑 → 才能删。
    sqlx::query("DELETE FROM indexer_download_client WHERE download_client_id = $1")
        .bind(client_id)
        .execute(db.pool())
        .await
        .expect("解绑");
    service.delete_client(client_id).await.expect("终于能删");
}

/// 更新请求的**空更新**判据：三个字段都没给 → 走 422（由 `update_client` 报）。
///
/// 这里只钉 serde 那一层（阶段二落地时用它）—— 骨架期断言的是 `enabled`/`config`，
/// 那两个字段上游没有。
#[test]
fn an_empty_update_payload_is_recognisable() {
    let empty: DownloadClientUpdateRequest =
        serde_json::from_value(serde_json::json!({})).expect("空对象合法");
    assert!(
        empty.name.is_none() && empty.library_id.is_none() && empty.provider_config.is_none(),
        "三个字段都没给 = 空更新"
    );
}
