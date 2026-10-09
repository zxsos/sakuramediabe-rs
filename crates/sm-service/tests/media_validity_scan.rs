//! 媒体有效性巡检的**端到端**测试：真库 + 可编程的假网关。
//!
//! # 为什么必须连库
//!
//! 这个服务的正确性一半在**状态机**上：「provider 清单里没有」只有配上前端
//! 的 `valid` 现值才知道是失效还是没变化；复活还要**重置缩略图状态**（有图 →
//! succeeded，无图 → pending）。这些分支纯单测盖不住 —— `reconcile` 是纯函数
//! （单测在 `src` 里），这里测的是「对账结果 → 批量落库」那半边。
//!
//! 上游出处：`playback/media_validity_scan_service.py`（189 行）。

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use sm_db::repo::{MediaRepository, NewMedia};
use sm_db::testing::TestDb;
use sm_service::playback::media_validity_scan::MediaValidityScanService;
use sm_service::playback::operation_locks::MediaOperation;
use sm_service::playback::provider_helpers::{
    LibraryHandle, MediaHandle, ProviderFailure, StorageGateway,
};

mod support;

/// 串行锁：advisory lock 键不区分 schema 的夹具事实，说明见
/// `media_file_hash_backfill.rs` 顶部（同一套库锁）。
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 可编程的巡检桩。
struct ScanStub {
    /// 库 id → 拉清单的剧本。`Err` = 库级失败（常用来给 `unsupported`）。
    scan: HashMap<i64, Result<Vec<String>, ProviderFailure>>,
    /// `Some` = 单条 key 计算一律失败（模拟 provider 拒绝脏 storage_ref）。
    key_failure: Option<ProviderFailure>,
    /// 收到的库级调用次数。
    scan_calls: Mutex<Vec<i64>>,
}

impl ScanStub {
    fn new(scan: HashMap<i64, Result<Vec<String>, ProviderFailure>>) -> Self {
        Self {
            scan,
            key_failure: None,
            scan_calls: Mutex::new(Vec::new()),
        }
    }

    fn with_key_failure(mut self, failure: ProviderFailure) -> Self {
        self.key_failure = Some(failure);
        self
    }
}

impl StorageGateway for ScanStub {
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

    fn probe_video_info(
        &self,
        _handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<serde_json::Value, ProviderFailure>> + Send + '_>> {
        unimplemented!("本套件不探测")
    }

    fn compute_file_hash(
        &self,
        _handle: &MediaHandle,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderFailure>> + Send + '_>> {
        unimplemented!("本套件不算哈希")
    }

    fn scan_managed_media_ref_keys(
        &self,
        library: &LibraryHandle,
    ) -> Pin<Box<dyn Future<Output = Result<Vec<String>, ProviderFailure>> + Send + '_>> {
        // `library` 的借用寿命进不了返回的 Future（见 `media_file_hash_backfill.rs`
        // 同一桩的做法）—— 先把要用的拷出来。
        let library_id = library.library_id;
        Box::pin(async move {
            self.scan_calls.lock().expect("锁").push(library_id);
            match self.scan.get(&library_id) {
                Some(Ok(keys)) => Ok(keys.clone()),
                Some(Err(failure)) => Err(failure.clone()),
                None => Ok(Vec::new()),
            }
        })
    }

    fn merged_playback_format(&self, _provider_key: &str) -> Option<String> {
        // 本套件不做合并播放。
        None
    }

    fn managed_media_ref_key(
        &self,
        _library: &LibraryHandle,
        media_ref: serde_json::Value,
    ) -> Pin<Box<dyn Future<Output = Result<String, ProviderFailure>> + Send + '_>> {
        Box::pin(async move {
            if let Some(failure) = &self.key_failure {
                return Err(failure.clone());
            }
            // 剧本：storage_ref 是 `{"key": …}`，key 就是那个字段。
            // （真 provider 的归一逻辑在插件里；桩只管「输入 → 稳定 key」。）
            Ok(media_ref
                .get("key")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned())
        })
    }
}

/// 在**指定库**里建一条带 storage_ref 的媒体（`support::seed_media` 每次都另建
/// 一个库，对账用例要「同一个库里多条媒体」）。
async fn seed_media_in(db: &TestDb, library_id: i32, ref_key: &str) -> i32 {
    let number = format!("SCAN-{}", support::n());
    support::seed_movie_if_missing(db, &number).await;
    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("{}.mkv", number),
            file_size_bytes: 1,
            movie_number: Some(number),
            video_item_id: None,
            // storage_ref 是 provider 的命名空间；桩按 `{"key": …}` 归一。
            storage_ref: Some(format!(r#"{{"key":"{ref_key}"}}"#)),
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 1,
            video_info: None,
        })
        .await
        .expect("insert media")
        .id
}

/// 直接改 `valid`（复活用例的前置：行已失效）。
async fn set_valid(db: &TestDb, media_id: i32, valid: bool) {
    sqlx::query("UPDATE media SET valid = $2 WHERE id = $1")
        .bind(media_id)
        .bind(valid)
        .execute(db.pool())
        .await
        .expect("置 valid");
}

async fn is_valid(db: &TestDb, media_id: i32) -> bool {
    sqlx::query_scalar::<_, bool>("SELECT valid FROM media WHERE id = $1")
        .bind(media_id)
        .fetch_one(db.pool())
        .await
        .expect("读 valid")
}

async fn thumbnail_state_of(db: &TestDb, media_id: i32) -> String {
    sqlx::query_scalar::<_, String>("SELECT thumbnail_generation_state FROM media WHERE id = $1")
        .bind(media_id)
        .fetch_one(db.pool())
        .await
        .expect("读缩略图状态")
}

/// 建库 + 桩 + 服务。库 id 用来写剧本。
async fn setup(db: &TestDb, remote: Vec<String>) -> (i32, Arc<ScanStub>, MediaValidityScanService) {
    let library_id = support::seed_library(db).await;
    let mut scan = HashMap::new();
    scan.insert(i64::from(library_id), Ok(remote));
    let stub = Arc::new(ScanStub::new(scan));
    let pool = db.pool_with_max_connections(4).await;
    let service = MediaValidityScanService::new(&pool, stub.clone());
    (library_id, stub, service)
}

fn unsupported_failure() -> ProviderFailure {
    ProviderFailure {
        code: "unsupported".to_owned(),
        safe_message: "该插件不支持巡检".to_owned(),
        retryable: false,
    }
}

/// ★ provider 清单里没有的 → 失效；有的 → 原样（unchanged）。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn invalidates_only_media_missing_from_the_provider() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let (library_id, _stub, service) = setup(&db, vec!["k-kept".to_owned()]).await;
    let kept = seed_media_in(&db, library_id, "k-kept").await;
    let gone = seed_media_in(&db, library_id, "k-gone").await;

    let stats = service.scan_media_validity(None).await.expect("跑完");

    assert_eq!(stats.scanned_libraries, 1);
    assert_eq!(stats.scanned_media, 2);
    assert_eq!(stats.invalidated_media, 1);
    assert_eq!(stats.unchanged_media, 1);
    assert_eq!(stats.updated_media, 1);
    assert_eq!(stats.remote_file_count, 1);
    assert!(is_valid(&db, kept).await, "在清单里的保持有效");
    assert!(!is_valid(&db, gone).await, "不在清单里的被判失效");
}

/// ★ 复活带**缩略图状态重置**：有缩略图 → succeeded（别浪费一次重生成），
/// 没有 → pending（让生成任务重新看它）。计数与错误字段清零。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn reviving_resets_the_thumbnail_state() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let (library_id, _stub, service) = setup(&db, vec!["k1".to_owned(), "k2".to_owned()]).await;
    let with_thumb = seed_media_in(&db, library_id, "k1").await;
    let without_thumb = seed_media_in(&db, library_id, "k2").await;
    set_valid(&db, with_thumb, false).await;
    set_valid(&db, without_thumb, false).await;
    // 一条有缩略图（先插一张 image 再挂上），一条没有。
    let image_id = support::seed_image(&db, "scan-origin").await;
    support::seed_thumbnail(&db, with_thumb, image_id, 0).await;
    // 同时把失败痕迹留在「有图」那条上，验证复活时被清掉。
    sqlx::query(
        "UPDATE media SET thumbnail_generation_state = 'failed', \
         thumbnail_attempt_count = 9, thumbnail_last_error = 'boom' WHERE id = $1",
    )
    .bind(with_thumb)
    .execute(db.pool())
    .await
    .expect("造失败痕迹");

    let stats = service.scan_media_validity(None).await.expect("跑完");

    assert_eq!(stats.revived_media, 2);
    assert_eq!(stats.updated_media, 2);
    assert_eq!(stats.invalidated_media, 0);
    assert!(is_valid(&db, with_thumb).await);
    assert!(is_valid(&db, without_thumb).await);
    assert_eq!(
        thumbnail_state_of(&db, with_thumb).await,
        "succeeded",
        "有缩略图的复活 → succeeded，不重新生成"
    );
    assert_eq!(
        thumbnail_state_of(&db, without_thumb).await,
        "pending",
        "没图的复活 → pending，让生成任务重新看它"
    );
}

/// ★ 库锁被占（导入正在写这个库）：整库媒体计入 skipped，**不排队**
/// —— 排队会等到导入改完才执行，那时状态已变。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_locked_library_is_fully_skipped() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let (library_id, stub, service) = setup(&db, vec![]).await;
    seed_media_in(&db, library_id, "k1").await;
    seed_media_in(&db, library_id, "k2").await;
    // 占住库锁（跨 await 持有是夹具故意的，理由见顶部）。
    let lock = MediaOperation::try_library(db.pool(), library_id)
        .await
        .expect("取锁")
        .expect("该拿到");

    let stats = service.scan_media_validity(None).await.expect("跑完");

    lock.release().await;
    assert_eq!(stats.scanned_libraries, 0, "锁被占的库不做对账");
    assert_eq!(stats.skipped_media, 2);
    assert!(
        stub.scan_calls.lock().expect("锁").is_empty(),
        "锁被占时连 provider 都不该被问"
    );
}

/// ★ provider 不支持巡检（两个能力方法缺一个的下游表现）：进
/// `unsupported_libraries` 名单，**不是** `failed_libraries` ——
/// 前者是合法状态（用户要看见），后者是坏消息。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn an_unsupported_library_is_reported_not_failed() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    let library_id = support::seed_library(&db).await;
    let mut scan = HashMap::new();
    scan.insert(i64::from(library_id), Err(unsupported_failure()));
    let stub = Arc::new(ScanStub::new(scan));
    let pool = db.pool_with_max_connections(4).await;
    let service = MediaValidityScanService::new(&pool, stub);
    let media_id = seed_media_in(&db, library_id, "k1").await;

    let stats = service.scan_media_validity(None).await.expect("跑完");

    assert_eq!(stats.failed_libraries, 0, "不支持不是失败");
    assert_eq!(
        stats.unsupported_libraries,
        vec![format!("local#{library_id}")],
        "按 provider_key#id 列名，让用户能定位是哪个库"
    );
    assert_eq!(stats.skipped_media, 1, "一条都没检查，计入 skipped");
    assert_eq!(stats.scanned_libraries, 0);
    assert!(is_valid(&db, media_id).await, "判不了就不动它");
}

/// ★ 单条 storage_ref 归一失败（provider 拒了脏引用）：**不能**因此标失效
/// ——「判不了」和「不在清单里」是两件事，冤案不可逆。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_ref_key_failure_never_invalidates() {
    let _serial = SERIAL.lock().expect("串行锁");
    let db = TestDb::require().await;
    // 拉清单成功（k1 在册），但**单条 key 计算**全失败 —— 两层失败要分开测。
    // 桩与 service 只用来搭库（另起 failing_service），不留绑定。
    let (library_id, ..) = setup(&db, vec!["k1".to_owned()]).await;
    let good = seed_media_in(&db, library_id, "k1").await;
    let dirty = seed_media_in(&db, library_id, "k2").await;
    let stub = Arc::new(
        ScanStub::new(HashMap::new()).with_key_failure(ProviderFailure {
            code: "invalid_argument".to_owned(),
            safe_message: "storage_ref 无法归一".to_owned(),
            retryable: false,
        }),
    );
    let pool = db.pool_with_max_connections(4).await;
    let failing_service = MediaValidityScanService::new(&pool, stub);

    let stats = failing_service
        .scan_media_validity(None)
        .await
        .expect("跑完");

    assert_eq!(stats.failed_media, 2, "两条都算不出 key");
    assert_eq!(stats.scanned_media, 0);
    assert_eq!(stats.invalidated_media, 0);
    assert!(is_valid(&db, good).await);
    assert!(is_valid(&db, dirty).await, "判不了的不标失效");
}
