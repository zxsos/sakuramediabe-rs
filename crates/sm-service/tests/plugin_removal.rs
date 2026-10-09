//! 卸载插件的**占用检查**（真实 PostgreSQL）。
//!
//! # 这层测四件事
//!
//! | 判据 | 期望 | 理由 |
//! |---|---|---|
//! | 409 的 `details` | 五个键与上游 `PluginInUseError.details` 逐字一致 | 客户端按这些键提示「先迁移哪些库」 |
//! | 顺序 | **检查在删除之前** | 反过来会得到「409 但插件已经没了」 |
//! | 文案 | 两个计数都报 | 只报媒体数会让「库下只有下载器」的实例读到 `0 个媒体` |
//! | 空 key 索引 | 放行 | 这是当前生产行为（注册表没接上），**显式钉住**它 |
//!
//! 最后一条**不是期望行为** —— 它把一个已知缺口钉在明面上。哪天 provider
//! 注册表带上了反向索引，这条测试会失败，那时正好把它改成断言 409。
//!
//! 删除动作本身（真的建目录、真的搬 `data/`）由 `sm-plugins` 的单测覆盖；
//! 本组只关心「检查 → 删除」这个编排。
//!
//! 每个用例的 `TestDb` 是独立 schema。

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use sm_db::repo::{
    DownloadClientRepository, MediaLibraryRepository, MediaRepository, MovieRepository,
    NewDownloadClient, NewMedia, NewMediaLibrary, NewMovie,
};
use sm_db::testing::TestDb;
use sm_service::error::ServiceError;
use sm_service::system::plugin_removal::{
    NoProviderKeys, PluginRemovalService, ProviderKeyIndex, PLUGIN_IN_USE,
};
use sm_service::system::plugins::{
    PluginAdmin, PluginDetail, PluginInstallOutcome, PluginSummary, PLUGIN_NOT_FOUND,
};

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 只记账、不碰文件系统的 [`PluginAdmin`]。
struct RecordingAdmin {
    removed: Mutex<Vec<String>>,
    /// `remove_code` 要返回的错误。用来验「检查通过之后，删除动作的错会原样
    /// 透传出去」—— 那一支就是生产里 404 的来源。
    fail_with: Option<ServiceError>,
}

impl RecordingAdmin {
    fn new() -> Self {
        Self {
            removed: Mutex::new(Vec::new()),
            fail_with: None,
        }
    }

    fn failing(error: ServiceError) -> Self {
        Self {
            removed: Mutex::new(Vec::new()),
            fail_with: Some(error),
        }
    }

    fn removed(&self) -> Vec<String> {
        self.removed.lock().expect("锁没 poisoned").clone()
    }
}

/// 本组用例不涉及的动作统一报 500 —— 走到它们就说明编排走错了。
fn not_this_test(message: &str) -> ServiceError {
    ServiceError::from_status(500, "internal_error", message)
}

impl PluginAdmin for RecordingAdmin {
    fn list(&self) -> Result<Vec<PluginSummary>, ServiceError> {
        Ok(Vec::new())
    }

    fn detail(&self, _plugin_id: &str) -> Result<Option<PluginDetail>, ServiceError> {
        Ok(None)
    }

    fn set_enabled(&self, _plugin_id: &str, _enabled: bool) -> Result<PluginSummary, ServiceError> {
        Err(not_this_test("本组用例不涉及启停"))
    }

    fn prepare_upload_slot(&self) -> Result<PathBuf, ServiceError> {
        Err(not_this_test("本组用例不涉及上传"))
    }

    fn archive_size_limit(&self) -> u64 {
        100 * 1024 * 1024
    }

    fn install_zip(
        &self,
        _zip_path: &Path,
        _sha256: Option<&str>,
        _enable: bool,
    ) -> Result<PluginInstallOutcome, ServiceError> {
        Err(not_this_test("本组用例不涉及安装"))
    }

    fn upgrade_zip(
        &self,
        _plugin_id: &str,
        _zip_path: &Path,
        _sha256: Option<&str>,
    ) -> Result<PluginInstallOutcome, ServiceError> {
        Err(not_this_test("本组用例不涉及升级"))
    }

    fn remove_code(&self, plugin_id: &str) -> Result<(), ServiceError> {
        self.removed
            .lock()
            .expect("锁没 poisoned")
            .push(plugin_id.to_owned());
        match &self.fail_with {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}

/// 恒定返回同一组 key 的索引。
struct FixedKeys(Vec<String>);

impl ProviderKeyIndex for FixedKeys {
    fn provider_keys_for_plugin(&self, _plugin_id: &str) -> Vec<String> {
        self.0.clone()
    }
}

// ================================================================ 造数

async fn seed_library(db: &TestDb, provider_key: &str) -> i32 {
    MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{:06}", n()),
            provider_key: provider_key.to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("建媒体库失败")
        .id
}

/// 一部影片。媒体**必须**恰好归属 `movie` 或 `video_item` 之一（仓储的
/// business 规则），而这里测的是「库里有多少媒体」，用影片侧最省事。
async fn seed_movie(db: &TestDb) -> String {
    let number = format!("PR-{:06}", n());
    MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "卸载检查用影片".to_owned(),
            javdb_id: None,
            summary: String::new(),
            maker_name: None,
            director_name: None,
            release_date: None,
            duration_minutes: 0,
            score: 0.0,
            score_number: 0,
            series_id: None,
            cover_image_id: None,
            thin_cover_image_id: None,
            metadata_source: None,
        })
        .await
        .expect("建影片失败");
    number
}

/// 往库里放一个媒体。**每次调用多建一部影片** —— 同一部影片可以有多条媒体
/// （不同版本），但那样就分不清「两条媒体」还是「一条媒体数了两次」。
async fn seed_media(db: &TestDb, library_id: i32) {
    let movie_number = seed_movie(db).await;
    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("m{:06}.mkv", n()),
            file_size_bytes: 1024,
            movie_number: Some(movie_number),
            video_item_id: None,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 0,
            video_info: None,
        })
        .await
        .expect("建媒体失败");
}

async fn seed_download_client(db: &TestDb, library_id: i32) {
    DownloadClientRepository::new(db.pool().clone())
        .insert(&NewDownloadClient {
            name: format!("dl-{:06}", n()),
            provider_config: None,
            library_id,
        })
        .await
        .expect("建下载客户端失败");
}

// ================================================================ 用例

/// ★ 插件的 provider 还挂着库 → **409**，且 `details` 五个键齐全。
#[tokio::test]
async fn a_plugin_whose_provider_owns_a_library_cannot_be_removed() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db, "local").await;
    seed_media(&db, library_id).await;
    seed_media(&db, library_id).await;
    seed_download_client(&db, library_id).await;

    let admin = RecordingAdmin::new();
    let keys = FixedKeys(vec!["local".to_owned()]);

    let error = PluginRemovalService::remove(db.pool(), &admin, &keys, "local")
        .await
        .expect_err("仍被引用该拒删");

    assert_eq!(error.status, 409);
    assert_eq!(error.code(), PLUGIN_IN_USE);

    let details = error.details().expect("409 必须带 details");
    assert_eq!(details["plugin_id"], "local");
    assert_eq!(details["provider_keys"], serde_json::json!(["local"]));
    assert_eq!(details["library_ids"], serde_json::json!([library_id]));
    // 计数要**真实**，不是「第一页有几条」—— 客户端据此提示用户。
    assert_eq!(details["media_count"], 2);
    assert_eq!(details["download_client_count"], 1);

    assert!(
        admin.removed().is_empty(),
        "★ 检查必须发生在删除之前 —— 否则会出现「409 但插件已经没了」"
    );
}

/// 文案照上游：**两个计数都报**，且给出可操作的下一步。
#[tokio::test]
async fn the_conflict_message_names_both_counts_and_what_to_do() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db, "local").await;
    seed_media(&db, library_id).await;
    seed_download_client(&db, library_id).await;

    let error = PluginRemovalService::ensure_not_in_use(db.pool(), "local", &["local".to_owned()])
        .await
        .expect_err("仍被引用该拒删");

    let message = &error.api.message;
    assert!(message.contains("1 个媒体库"), "{message}");
    assert!(message.contains("1 个媒体"), "{message}");
    assert!(message.contains("1 个下载客户端"), "{message}");
    assert!(message.contains("请先迁移或删除相关媒体库"), "{message}");
}

/// 有 provider key 但**没有库**用它 → 放行（上游第二处提前返回）。
#[tokio::test]
async fn a_plugin_with_keys_but_no_library_can_be_removed() {
    let db = TestDb::require().await;
    // 库里挂的是**别的** provider。
    seed_library(&db, "other").await;

    let admin = RecordingAdmin::new();
    PluginRemovalService::remove(
        db.pool(),
        &admin,
        &FixedKeys(vec!["local".to_owned()]),
        "local",
    )
    .await
    .expect("没有引用就该能删");

    assert_eq!(admin.removed(), vec!["local".to_owned()]);
}

/// ★ 当前生产行为：key 索引恒空 → **检查不生效**，插件会被删掉。
#[tokio::test]
async fn an_empty_key_index_lets_the_removal_through_even_with_libraries() {
    let db = TestDb::require().await;
    let library_id = seed_library(&db, "local").await;
    seed_media(&db, library_id).await;

    let admin = RecordingAdmin::new();
    PluginRemovalService::remove(db.pool(), &admin, &NoProviderKeys, "local")
        .await
        .expect("★ 已知缺口：没有 key 就直接放行");

    assert_eq!(
        admin.removed(),
        vec!["local".to_owned()],
        "检查通过后应当真的删了"
    );
}

/// 删除动作自己的 404 由 `PluginAdmin` 报出来，本层**原样透传**（不吞成 409）。
#[tokio::test]
async fn the_administrators_404_is_passed_through_untouched() {
    let db = TestDb::require().await;
    let admin = RecordingAdmin::failing(ServiceError::from_status(
        404,
        PLUGIN_NOT_FOUND,
        "未知插件 plugin_id=ghost",
    ));

    let error = PluginRemovalService::remove(db.pool(), &admin, &NoProviderKeys, "ghost")
        .await
        .expect_err("未安装该 404");
    assert_eq!(error.status, 404);
    assert_eq!(error.code(), PLUGIN_NOT_FOUND);
}

/// 边界：没有 key 时**不该**去查库（上游第一处 `if not provider_keys: return`）。
#[tokio::test]
async fn no_keys_means_no_database_work() {
    let db = TestDb::require().await;
    seed_library(&db, "local").await;
    assert!(
        PluginRemovalService::ensure_not_in_use(db.pool(), "local", &[])
            .await
            .is_ok()
    );
}
