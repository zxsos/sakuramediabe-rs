//! `CatalogMovieMetadataImporter` 的真库验收 —— 「入库路径」从窄接口走到头。
//!
//! `transfers` 侧那两个方法（`import_by_number` / `import_by_candidate`）是
//! 「按番号重新取元数据」与「按用户选中的候选重新取元数据」的入口。这里用
//! **假的 JavDB provider + 真的 `CatalogImportService` + 真库**把它们走通：
//!
//! | 用例 | 验的是 |
//! |---|---|
//! | 按番号 | 落库 + 返回值那个「是否新建」 |
//! | 按候选 | 候选 id → 详情 → 落库：`fetch_candidate` 那条路真的通 |
//! | 都没收录 | 错误码是 `metadata_not_found`（不是 500） |
//!
//! # 插件那一支仍然测不到
//!
//! 它要真起一个 gRPC 插件进程（跨仓集成测试，见 `docs/tasks/proto-p1-gaps.md`
//! §2.5）。所以「按候选」这条**只覆盖 JavDB 支** —— 假装测了插件支比不测更糟。
//!
//! # 为什么值得用真库
//!
//! 「第二次不再问来源」与「那一行真的在库里」只有在真库上才有意义：mock 一个
//! 仓储来测这两条，等于把要验的东西假设掉了。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use sm_db::repo::MovieRepository;
use sm_db::testing::TestDb;
use sm_service::catalog::catalog_import::CatalogImportService;
use sm_service::catalog::metadata_source::{
    MetadataProvider, MetadataSourceError, MetadataSourceService,
};
use sm_service::catalog::movie_image::{ImageTaskSet, ImageTasksBuilder};
use sm_service::catalog::movie_metadata_importer::CatalogMovieMetadataImporter;
use sm_service::catalog::movie_metadata_search::MovieMetadataSearchService;
use sm_service::error::ServiceError;
use sm_service::system::ConfigService;
use sm_service::transfers::import_service::{
    CatalogImport as TransfersCatalogImport, MovieMetadataImporter, NewMedia,
};

// ================================================================ 替身

/// 可计数的假 JavDB：按番号与按 id 各给一份预置详情。
///
/// `calls` 是这份文件里最重要的东西：「候选那条路没有按番号再问一次」只能靠它
/// 证伪 —— 两次调用都返回同一份详情时，用返回值是分不出走了哪条路的。
struct FakeJavdb {
    calls: Arc<AtomicUsize>,
    /// `get_movie_by_number` 的返回。`None` = 「没收录」（不是失败）。
    by_number: Option<serde_json::Value>,
    /// `get_movie_by_javdb_id` 的返回。`None` = 「没收录」。
    by_id: Option<serde_json::Value>,
}

#[tonic::async_trait]
impl MetadataProvider for FakeJavdb {
    async fn get_movie_by_number(
        &self,
        _movie_number: &str,
    ) -> Result<Option<serde_json::Value>, MetadataSourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.by_number.clone())
    }

    async fn get_movie_by_javdb_id(
        &self,
        _javdb_id: &str,
    ) -> Result<Option<serde_json::Value>, MetadataSourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.by_id.clone())
    }

    async fn search_actors(
        &self,
        _keyword: &str,
    ) -> Result<Vec<serde_json::Value>, MetadataSourceError> {
        Ok(Vec::new())
    }
}

/// `transfers::CatalogImport` 的**占位实现**。
///
/// `MovieMetadataImporter` 的签名里带着这个参数（骨架如此），而本仓的真实现
/// 一次都不读它 —— 写入走的是构造时注入的 **catalog** `CatalogImport`（catalog
/// 那个是异步全接口，transfers 这个只有两个同步方法，装不下多来源写入；理由见
/// `catalog::movie_metadata_importer` 的模块文档）。
///
/// 所以这里的方法一旦被碰到就立刻炸：**被调用就说明「这个参数被用上了」**，
/// 那是设计变了，该回头看那个 trait 的签名。
struct NeverUsedTransfersImport;

impl TransfersCatalogImport for NeverUsedTransfersImport {
    fn import_movie(
        &self,
        _movie_number: &str,
        _metadata: &serde_json::Value,
    ) -> Result<(i64, bool), ServiceError> {
        panic!("`MovieMetadataImporter` 的 import 参数不该被使用");
    }

    fn import_media(&self, _media: &NewMedia) -> Result<i64, ServiceError> {
        panic!("`MovieMetadataImporter` 的 import 参数不该被使用");
    }
}

/// 不构造任何图片任务的替身（JavDB 支不读它 —— 图片那一层还没接）。
struct NoImages;

impl ImageTasksBuilder for NoImages {
    fn build_movie_import_image_tasks(
        &self,
        _movie_number: &str,
        _cover_image_url: Option<&str>,
        _plot_urls: &[String],
        _actors: &[serde_json::Value],
    ) -> Result<ImageTaskSet, ServiceError> {
        Ok(ImageTaskSet::default())
    }
}

/// 一个不下载任何东西的替身（图片支线未接，不会被调用）。
fn no_downloader() -> sm_service::catalog::catalog_import::ImageDownloader {
    Box::new(|_url: &str, _path: &std::path::Path| Ok(()))
}

/// JavDB provider 原文（`create_movie` 读的那组键）。
fn javdb_detail(number: &str) -> serde_json::Value {
    serde_json::json!({
        "movie_number": number,
        "title": "测试标题",
        "javdb_id": "javdb-777",
        "summary": "测试简介",
        "maker_name": "测试厂商",
        "director_name": "测试导演",
        "release_date": "2024-03-05",
        "duration_minutes": 120,
        "score": 4.5,
        "score_number": 100,
    })
}

/// 一份**一个插件来源都没启用**的临时配置。
///
/// 显式写出来而不是靠「文件不存在 → 默认值」：默认值里 `plugins.enabled`
/// 是什么都不该由本文件假设。
fn temp_config() -> ConfigService {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|delta| delta.as_nanos())
        .unwrap_or_default();
    let path = std::env::temp_dir().join(format!(
        "sm-metadata-importer-{}-{seq}-{nanos}.toml",
        std::process::id()
    ));
    std::fs::write(&path, "[plugins]\nenabled = []\n").expect("写临时配置");
    ConfigService::new(path)
}

// ================================================================ 夹具

struct Fixture {
    db: TestDb,
    metadata: CatalogMovieMetadataImporter,
    calls: Arc<AtomicUsize>,
}

impl Fixture {
    async fn new(by_number: Option<serde_json::Value>, by_id: Option<serde_json::Value>) -> Self {
        let db = TestDb::require().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let config = temp_config();
        // ★ `source` 只建一份，两条路共用：`search` 里那份与顶层那份必须是同一个
        // provider 实例（各建一份，`calls` 就对不上，而计数正是这里的判据）。
        let source = Arc::new(MetadataSourceService::new(
            Vec::new(),
            Some(Box::new(FakeJavdb {
                calls: Arc::clone(&calls),
                by_number,
                by_id,
            })),
        ));
        let search = Arc::new(MovieMetadataSearchService::new(
            config.clone(),
            Arc::clone(&source),
        ));
        let catalog = Arc::new(CatalogImportService::new(
            db.pool(),
            Box::new(NoImages),
            no_downloader(),
        ));
        let metadata = CatalogMovieMetadataImporter::new(config, source, search, catalog);
        Self {
            db,
            metadata,
            calls,
        }
    }
}

// ================================================================ 用例

/// ★ 按番号：落库，且返回值就是「是否新建」。
#[tokio::test]
async fn importing_by_number_reports_whether_it_created_the_movie() {
    let fixture = Fixture::new(Some(javdb_detail("MMI-001")), None).await;

    let created = fixture
        .metadata
        .import_by_number("MMI-001", &NeverUsedTransfersImport, false)
        .await
        .expect("导入应当成功");
    assert!(created, "第一次导入一定是新建");

    let movie = MovieRepository::new(fixture.db.pool().clone())
        .find_by_number("MMI-001")
        .await
        .expect("查库")
        .expect("影片应当已经落库");
    assert_eq!(movie.title, "测试标题");
    assert_eq!(movie.javdb_id.as_deref(), Some("javdb-777"));

    let again = fixture
        .metadata
        .import_by_number("MMI-001", &NeverUsedTransfersImport, false)
        .await
        .expect("第二次导入");
    assert!(!again, "第二次不是新建");
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        1,
        "第二次**不该**再问来源一次（短路失效的话这里会是 2）"
    );
}

/// ★ 按候选（JavDB 支）：候选 id → 按 **javdb_id** 取详情 → 落库。
///
/// 这是「手动重试选中一个候选」的入口，也是 `fetch_candidate` 唯一的真库验收。
///
/// 夹具把 `by_number` 置空：如果这条路走错成「按番号再问一次」，JavDB 会说
/// 「没收录」，用例就会以 `metadata_not_found` 失败 —— 而不是悄悄多打一次 API。
#[tokio::test]
async fn importing_a_candidate_uses_the_candidate_detail() {
    let fixture = Fixture::new(None, Some(javdb_detail("MMI-002"))).await;

    let created = fixture
        .metadata
        .import_by_candidate("javdb:MMI-002:javdb-777", &NeverUsedTransfersImport, true)
        .await
        .expect("候选导入应当成功");
    assert!(created, "第一次导入一定是新建");

    let movie = MovieRepository::new(fixture.db.pool().clone())
        .find_by_number("MMI-002")
        .await
        .expect("查库")
        .expect("影片应当已经落库");
    assert_eq!(movie.javdb_id.as_deref(), Some("javdb-777"));
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        1,
        "候选那条路只按 id 问一次"
    );
}

/// ★ 谁都没有这部片 → `metadata_not_found`（不是 500、也不是别的）。
///
/// 这个码会被写进失败项的 `failure_detail`（`import_service.rs:626`）并出现在
/// 重试端点的正文里（`:684`）—— 客户端靠它区分「换候选」与「重试」。
#[tokio::test]
async fn a_movie_nobody_has_is_metadata_not_found() {
    let fixture = Fixture::new(None, None).await;

    let error = fixture
        .metadata
        .import_by_number("MMI-003", &NeverUsedTransfersImport, false)
        .await
        .expect_err("都不收录就该报错");

    assert_eq!(error.code(), "metadata_not_found");
    assert!(
        MovieRepository::new(fixture.db.pool().clone())
            .find_by_number("MMI-003")
            .await
            .expect("查库")
            .is_none(),
        "没收录时不该建出任何记录"
    );
}
