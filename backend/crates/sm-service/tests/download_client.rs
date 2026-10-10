//! 下载器客户端**库侧**两件事的连库对拍（上游 `client_config_service.py`）。
//!
//! | 入口 | 上游 | 本文件盯什么 |
//! |---|---|---|
//! | `list_clients` | `:257-265` | 排序是 `created_at DESC, id DESC`（**最新在前**），不是骨架期注释说的「按 id 升序」|
//! | `delete_client` | `:333-351` | 两道 409 的**码、先后、details**，以及 404 与删除本身 |
//!
//! | `update_client` 的配置合并 | `:145-154` | 未提交的 `secret` 从**库里的旧值**回填 |
//!
//! `create` / `test_client` 不在这里 —— `test_client` 要真 provider 往返，
//! `create` 与 `update` 同一套合并规则，这里只钉 `update` 那条（它才有「旧值」）。

use sm_db::repo::{
    DownloadClientRepository, DownloadTaskRepository, IndexerDownloadClientRepository,
    IndexerRepository, MediaLibraryRepository, NewDownloadClient, NewDownloadTask, NewIndexer,
    NewMediaLibrary,
};
use sm_db::testing::TestDb;
use sm_service::transfers::download_client::{
    DownloadCapabilityRegistry, DownloadClientCapability, DownloadClientConfigField,
    DownloadClientDiagnostic, DownloadClientService, DownloadClientUpdateRequest,
    PreviousClientHandle, ProviderFailureInfo,
};
use std::sync::Arc;

fn unique() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos())
}

/// 一个假的下载能力注册表：只给 `local` 提供能力，字段表声明 `host`（可见）
/// 与 `token`（secret）。
///
/// **不是可有可无的脚手架**：`_resource` 要**知道哪些字段是 secret** 才剥得掉，
/// 而字段表的唯一来源是插件；拿不到时上游把整份配置换成 `{}`
/// （`client_config_service.py:73-91`）。没有它，`list_clients` 只能断言 `{}`，
/// 「secret 被剥掉」这条就没人盯了。与 `sm-api/tests/media_libraries_http.rs`
/// 的 `FakeRegistry` 是同一个套路（媒体库 `_resource` 判据相同）。
struct FakeDownloads;

struct FakeCapability;

impl DownloadClientCapability for FakeCapability {
    fn config_fields(&self) -> Vec<DownloadClientConfigField> {
        vec![
            DownloadClientConfigField {
                key: "host".to_owned(),
                input: "text".to_owned(),
                read_only: false,
            },
            DownloadClientConfigField {
                key: "token".to_owned(),
                input: "secret".to_owned(),
                read_only: false,
            },
        ]
    }

    fn prepare_client(
        &self,
        submitted: &serde_json::Value,
        _library_id: i32,
        _previous: Option<&PreviousClientHandle>,
    ) -> Result<serde_json::Value, ProviderFailureInfo> {
        Ok(submitted.clone())
    }

    fn test_client(
        &self,
        _submitted: &serde_json::Value,
        _library_id: i32,
    ) -> Result<DownloadClientDiagnostic, ProviderFailureInfo> {
        Err(ProviderFailureInfo {
            code: "unimplemented".to_owned(),
            message: "FakeCapability 不探测".to_owned(),
        })
    }
}

impl DownloadCapabilityRegistry for FakeDownloads {
    fn download_client_for(
        &self,
        provider_key: &str,
    ) -> Result<Option<Box<dyn DownloadClientCapability>>, ProviderFailureInfo> {
        if provider_key == "local" {
            Ok(Some(Box::new(FakeCapability)))
        } else {
            Ok(None)
        }
    }
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

/// ★ 列表：**最新在前**（`created_at DESC, id DESC`），`provider_config` 是对象，
/// 且 **secret 字段被剥掉**（上游 `_resource`，`client_config_service.py:73-91`）。
#[tokio::test]
async fn listing_is_newest_first_and_projects_an_object() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db).await;
    let first = seed_client(&db, library_id, None).await;
    // `token` 是 provider 声明的 secret —— 响应里**必须**看不到它。
    let second = seed_client(&db, library_id, Some(r#"{"host":"h","token":"s3cr3t"}"#)).await;
    let third = seed_client(&db, library_id, None).await;

    let clients = DownloadClientService::new_with_downloads(db.pool(), Arc::new(FakeDownloads))
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
        serde_json::json!({"host": "h"}),
        "`host` 留下、`token` 被 `_resource` 剥掉"
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

/// 读回一个下载器的**存储配置**。
///
/// 响应里看不到 `secret`（被 `_resource` 剥掉），所以「provider 有没有收到 token」
/// 只能看库里那一行 —— `FakeCapability::prepare_client` 原样返回它收到的东西，
/// 而服务层把那个返回值落库。
async fn stored_config(db: &TestDb, client_id: i32) -> serde_json::Value {
    let row = DownloadClientRepository::new(db.pool().clone())
        .find_by_id(client_id)
        .await
        .expect("查下载器")
        .expect("行还在");
    serde_json::from_str(row.provider_config.as_deref().expect("配置文本")).expect("配置是 JSON")
}

/// ★ 只改 `host` 时 `token` **从旧值回填**（上游 `_prepare` `:145-154`）。
///
/// 这是那条「改一次地址就把凭据抹掉」的 bug 的回归钉。
#[tokio::test]
async fn updating_one_field_keeps_the_stored_secret() {
    let db = TestDb::require().await;
    let service = DownloadClientService::new_with_downloads(db.pool(), Arc::new(FakeDownloads));
    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id, Some(r#"{"host":"h1","token":"s3cr3t"}"#)).await;

    service
        .update_client(
            client_id,
            DownloadClientUpdateRequest {
                name: None,
                library_id: None,
                provider_config: Some(serde_json::json!({"host": "h2"})),
            },
        )
        .await
        .expect("只改 host 应当成功");

    let config = stored_config(&db, client_id).await;
    assert_eq!(config["host"], "h2", "提交的值生效");
    assert_eq!(config["token"], "s3cr3t", "★ 没提交的 token 必须带过去");
}

/// 提交了新 `token` → **用提交的**，旧值不覆盖。
#[tokio::test]
async fn a_resubmitted_secret_wins_over_the_stored_one() {
    let db = TestDb::require().await;
    let service = DownloadClientService::new_with_downloads(db.pool(), Arc::new(FakeDownloads));
    let library_id = seed_library(&db).await;
    let client_id = seed_client(&db, library_id, Some(r#"{"host":"h","token":"old"}"#)).await;

    service
        .update_client(
            client_id,
            DownloadClientUpdateRequest {
                name: None,
                library_id: None,
                provider_config: Some(serde_json::json!({"host": "h", "token": "new"})),
            },
        )
        .await
        .expect("更新应当成功");

    assert_eq!(stored_config(&db, client_id).await["token"], "new");
}

/// 只动 `library_id`（**没提交** `provider_config`）也保留配置 —— 上游那条
/// `allow_read_only` 分支（`:324`）走的就是这里。
#[tokio::test]
async fn moving_to_another_library_keeps_the_whole_config() {
    let db = TestDb::require().await;
    let service = DownloadClientService::new_with_downloads(db.pool(), Arc::new(FakeDownloads));
    let first = seed_library(&db).await;
    let second = seed_library(&db).await;
    let client_id = seed_client(&db, first, Some(r#"{"host":"h","token":"s3cr3t"}"#)).await;

    service
        .update_client(
            client_id,
            DownloadClientUpdateRequest {
                name: None,
                library_id: Some(second),
                provider_config: None,
            },
        )
        .await
        .expect("换库应当成功");

    let config = stored_config(&db, client_id).await;
    assert_eq!(config["host"], "h");
    assert_eq!(config["token"], "s3cr3t");
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
