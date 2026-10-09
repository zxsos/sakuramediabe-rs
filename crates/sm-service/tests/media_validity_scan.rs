//! 媒体有效性巡检的**端到端**测试：真库 + 可编程的假清单。
//!
//! # 为什么必须连库
//!
//! 服务的正确性一半在**跨行状态**上：`reconcile` 的判定（纯函数，另有单测）
//! 之外，「复活要顺带重置缩略图状态」「已是对的目标状态的不重写」「脏
//! storage_ref 逐条记失败」都只有真库才验得到。
//!
//! 上游出处：`playback/media_validity_scan_service.py`（189 行）。

use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use sm_db::testing::TestDb;
use sm_service::playback::media_validity_scan::MediaValidityScanService;
use sm_service::playback::provider_helpers::{
    LibraryHandle, MediaHandle, ProviderFailure, StorageGateway, PROVIDER_UNSUPPORTED,
};

mod support;

/// 串行锁。理由与 `media_file_hash_backfill` 套件同一条：advisory lock 的键
/// 是 `(namespace, library_id/media_id)`，不区分 schema，每个测试 schema 的
/// id 都从 1 开始 —— 并行会互相占锁。生产无此问题。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 可编程的清单桩。
///
/// `keys` 是 provider 当前认领的 key 集合（用例中途可以改，模拟「文件回来了」）；
/// `managed_media_ref_key` 取 `media_ref["key"]` —— 与真 provider 一样，
/// 归一规则是 provider 自己的。
struct ScriptedInventory {
    keys: Mutex<BTreeSet<String>>,
    /// `true` = 模拟 provider 不支持扫描（`scan` 直接回 `unsupported`）。
    unsupported: bool,
}

impl StorageGateway for ScriptedInventory {
    fn has_provider(&self, _provider_key: &str) -> bool {
        true
    }

    fn delete_media(
        &self,
        _handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<(), ProviderFailure>> + Send + '_>> {
        unimplemented!("本套件不删媒体")
    }

    fn generate_thumbnails(
        &self,
        _handle: &MediaHandle,
        _workspace: &std::path::Path,
    ) -> Pin<
        Box<
            dyn Future<
                    Output = Result<
                        sm_service::playback::provider_helpers::ThumbnailJobResult,
                        ProviderFailure,
                    >,
                > + Send
                + '_,
        >,
    > {
        unimplemented!("本套件不生成缩略图")
    }

    fn compute_file_hash(
        &self,
        _handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderFailure>> + Send + '_>> {
        unimplemented!("本套件不算哈希")
    }

    fn probe_video_info(
        &self,
        _handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ProviderFailure>> + Send + '_>> {
        unimplemented!("本套件不探测")
    }

    fn scan_managed_media_ref_keys(
        &self,
        _library: &LibraryHandle,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, ProviderFailure>> + Send + '_>> {
        if self.unsupported {
            return Box::pin(async {
                Err(ProviderFailure {
                    code: PROVIDER_UNSUPPORTED.to_owned(),
                    safe_message: "桩：不支持扫描".to_owned(),
                    retryable: false,
                })
            });
        }
        Box::pin(async { Ok(self.keys.lock().expect("锁").iter().cloned().collect()) })
    }

    fn managed_media_ref_key(
        &self,
        _library: &LibraryHandle,
        media_ref: Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderFailure>> + Send + '_>> {
        let key = media_ref
            .get("key")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Box::pin(async move {
            key.ok_or_else(|| ProviderFailure {
                code: "invalid_config".to_owned(),
                safe_message: "桩：引用里没有 key".to_owned(),
                retryable: false,
            })
        })
    }
}

/// 建库 + 一条媒体，`storage_ref` 写成 `{"key": <key>}`。
async fn seed_media_with_ref(db: &TestDb, movie_number: &str, key: &str) -> i32 {
    let media_id = support::seed_media(db, Some(movie_number)).await;
    sqlx::query("UPDATE media SET storage_ref = $1 WHERE id = $2")
        .bind(json!({"key": key}).to_string())
        .bind(media_id)
        .execute(db.pool())
        .await
        .expect("写 storage_ref");
    media_id
}

async fn valid_of(db: &TestDb, media_id: i32) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT valid FROM media WHERE id = $1")
        .bind(media_id)
        .fetch_one(db.pool())
        .await
        .expect("读 valid")
}

async fn thumbnail_state_of(db: &TestDb, media_id: i32) -> (String, i32) {
    sqlx::query_as::<_, (String, i32)>(
        "SELECT thumbnail_generation_state, thumbnail_attempt_count FROM media WHERE id = $1",
    )
    .bind(media_id)
    .fetch_one(db.pool())
    .await
    .expect("读缩略图状态")
}

async fn service(db: &TestDb, inventory: ScriptedInventory) -> MediaValidityScanService {
    // 两条连接：库锁占一条，库操作走另一条（见哈希回填套件的注释）。
    let pool = db.pool_with_max_connections(2).await;
    MediaValidityScanService::new(&pool, Arc::new(inventory))
}

/// ★ 否定证据的闭环：清单里没有 → 失效；回来了 → 复活且缩略图状态被重置。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn invalidates_gone_files_and_revives_returning_ones() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let present = seed_media_with_ref(&db, "SCAN-PRESENT", "stay.mkv").await;
    let gone = seed_media_with_ref(&db, "SCAN-GONE", "gone.mkv").await;
    // 预置一个「失效过」的痕迹，验证复活时会被清掉。
    sqlx::query(
        "UPDATE media SET thumbnail_last_error = '旧失败记录', thumbnail_attempt_count = 3 \
         WHERE id = $1",
    )
    .bind(gone)
    .execute(db.pool())
    .await
    .expect("预置缩略图失败痕迹");

    // 桩要用 `Arc` 共享给服务与用例两边：「文件回来了」是**同一个库**的续跑，
    // 必须中途改同一个桩里的清单。
    let inventory = Arc::new(ScriptedInventory {
        keys: Mutex::new(BTreeSet::from(["stay.mkv".to_owned()])),
        unsupported: false,
    });
    let pool = db.pool_with_max_connections(2).await;
    let service = MediaValidityScanService::new(&pool, inventory.clone());

    // ── 第一轮：gone.mkv 不在清单里 → 失效 ──
    //（`seed_media` 每条媒体自带一个新库，所以这里是**两个库**各扫一轮。）
    let stats = service.scan_media_validity(None).await.expect("第一轮");
    assert_eq!(stats.scanned_libraries, 2);
    assert_eq!(stats.invalidated_media, 1);
    assert_eq!(stats.unchanged_media, 1, "还在的那条不重写");
    assert_eq!(stats.revived_media, 0);
    assert!(!valid_of(&db, gone).await, "确定不在清单里 → 失效");
    assert!(valid_of(&db, present).await);

    // ── 文件回来了 → 复活 + 缩略图状态重置 ──
    inventory
        .keys
        .lock()
        .expect("锁")
        .insert("gone.mkv".to_owned());
    let stats = service.scan_media_validity(None).await.expect("第二轮");
    assert_eq!(stats.revived_media, 1);
    assert_eq!(stats.invalidated_media, 0, "第二轮没有新的失效");
    assert!(valid_of(&db, gone).await);
    let (state, attempts) = thumbnail_state_of(&db, gone).await;
    assert_eq!(attempts, 0, "计数清零");
    assert!(
        state == "pending" || state == "succeeded",
        "回到生成流程的起点（有缩略图 → succeeded，没有 → pending），实际：{state}"
    );
    assert!(
        stats.unchanged_media >= 1,
        "stay.mkv 两轮都没变化，不再重写"
    );
}

/// ★ 不支持扫描的库：列进 `unsupported_libraries`（名字，不只是数），媒体
/// 全部跳过且**不改** valid。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn an_unsupported_library_is_surfaced_and_left_alone() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let media_id = seed_media_with_ref(&db, "SCAN-UNSUPPORTED", "x.mkv").await;

    let stats = service(
        &db,
        ScriptedInventory {
            keys: Mutex::new(BTreeSet::new()),
            unsupported: true,
        },
    )
    .await
    .scan_media_validity(None)
    .await
    .expect("跑完");

    assert_eq!(stats.unsupported_libraries.len(), 1);
    assert!(stats.unsupported_libraries[0].starts_with("local#"));
    assert_eq!(stats.failed_libraries, 0, "不支持是合法状态，不是失败");
    assert_eq!(stats.skipped_media, 1);
    assert!(valid_of(&db, media_id).await, "没查清之前不能动 valid");
}

/// ★ 脏 storage_ref：逐条记 `failed_media`，**不判失效** —— 判据是「provider
/// 说没有」，而不是「算不出 key」。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_dirty_storage_ref_is_a_failure_not_an_invalidation() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let media_id = support::seed_media(&db, Some("SCAN-DIRTY")).await;
    // 不写 storage_ref（NULL）→ 算不出 key。

    let stats = service(
        &db,
        ScriptedInventory {
            keys: Mutex::new(BTreeSet::new()),
            unsupported: false,
        },
    )
    .await
    .scan_media_validity(None)
    .await
    .expect("跑完");

    assert_eq!(stats.failed_media, 1);
    assert_eq!(stats.invalidated_media, 0, "「没法判」不是「不在」");
    assert!(valid_of(&db, media_id).await);
}
