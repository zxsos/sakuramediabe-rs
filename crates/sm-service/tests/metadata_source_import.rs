//! `MetadataSourceService::import_by_number` 的真库验收。
//!
//! # 它验的是「入库路径」的第一段
//!
//! 上游 `import_by_number`（`metadata_source_service.py:30-44`）做三件事：
//! 查已存在 → 按番号取元数据（JavDB 优先、插件兜底）→ 分派给对应的导入方法。
//! 这里用一个**假的 JavDB provider** 覆盖前两段与分派里的 JavDB 那支。
//!
//! 插件那一支**测不到**：它要真起一个 gRPC 插件进程（那属于跨仓集成测试，
//! 见 `docs/tasks/proto-p1-gaps.md` §2.5）。所以本文件刻意只测 JavDB 支 ——
//! 假装测了插件支比不测更糟。
//!
//! # 为什么值得用真库
//!
//! 「已存在就短路」这条断言只有在真库上才有意义：它要的是**库里有那一行**，
//! 而不是内存里的一个 mock。用 mock 仓储测这句话等于把要验的东西假设掉了。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use sm_db::repo::MovieRepository;
use sm_db::testing::TestDb;
use sm_service::catalog::catalog_import::CatalogImportService;
use sm_service::catalog::metadata_source::{
    MetadataProvider, MetadataSourceError, MetadataSourceService,
};
use sm_service::catalog::movie_image::{ImageTaskSet, ImageTasksBuilder};
use sm_service::error::ServiceError;

/// 可计数的假 JavDB。
///
/// `calls` 是这份文件里**最重要**的东西：短路那条断言靠它证伪 ——
/// 「第二次没有新建」可以由很多原因造成（比如库里查不到、或者又建了一部），
/// 只有「来源一次都没被再问」才是「短路生效」。
struct FakeJavdb {
    calls: Arc<AtomicUsize>,
    /// `None` = 「没收录」（不是失败）。
    detail: Option<serde_json::Value>,
}

#[tonic::async_trait]
impl MetadataProvider for FakeJavdb {
    async fn get_movie_by_number(
        &self,
        _movie_number: &str,
    ) -> Result<Option<serde_json::Value>, MetadataSourceError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.detail.clone())
    }

    async fn get_movie_by_javdb_id(
        &self,
        _javdb_id: &str,
    ) -> Result<Option<serde_json::Value>, MetadataSourceError> {
        Ok(None)
    }

    async fn search_actors(
        &self,
        _keyword: &str,
    ) -> Result<Vec<serde_json::Value>, MetadataSourceError> {
        Ok(Vec::new())
    }
}

/// 不构造任何图片任务的替身。
///
/// JavDB 那一支**不读**它（`CatalogImportService::create_movie` 只建 movie 行，
/// 图片那一层还没接）。真去造任务反而会掩盖「这一支不需要它」。
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

/// 没有启用任何插件来源的配置。
fn no_plugins() -> serde_json::Value {
    serde_json::json!({"plugins": {"enabled": []}})
}

struct Fixture {
    db: TestDb,
    imports: CatalogImportService,
    /// 被测对象。没有启用任何插件来源（`Vec::new()`）—— 插件那一支要真起
    /// 进程，见模块文档。
    source: MetadataSourceService,
    calls: Arc<AtomicUsize>,
}

impl Fixture {
    /// `detail = None` 表示假 JavDB「没收录这部片」。
    async fn new(detail: Option<serde_json::Value>) -> Self {
        let db = TestDb::require().await;
        let imports = CatalogImportService::new(db.pool(), Box::new(NoImages), no_downloader());
        let calls = Arc::new(AtomicUsize::new(0));
        let source = MetadataSourceService::new(
            Vec::new(),
            Some(Box::new(FakeJavdb {
                calls: Arc::clone(&calls),
                detail,
            })),
        );
        Self {
            db,
            imports,
            calls,
            source,
        }
    }
}

// ================================================================ 用例

/// JavDB 收录了 → 建库，返回 `(id, true)`，且**那一行真的在库里**。
#[tokio::test]
async fn a_javdb_hit_creates_the_movie() {
    let fixture = Fixture::new(Some(javdb_detail("MSI-001"))).await;

    let (movie_id, created) = fixture
        .source
        .import_by_number(&no_plugins(), "MSI-001", &fixture.imports, false)
        .await
        .expect("导入应当成功");

    assert!(created, "第一次导入一定是新建");
    let movie = MovieRepository::new(fixture.db.pool().clone())
        .find_by_number("MSI-001")
        .await
        .expect("查库")
        .expect("影片应当已经落库");
    assert_eq!(movie.id, movie_id);
    assert_eq!(movie.title, "测试标题");
    assert_eq!(movie.maker_name.as_deref(), Some("测试厂商"));
    // JavDB 那一支必须带上真实身份 —— 这是它与插件那一支的**唯一**区别。
    assert_eq!(movie.javdb_id.as_deref(), Some("javdb-777"));
}

/// ★ 已存在的番号**不再问一次来源**。
///
/// 这条是上游 `import_by_number` 第一行的意义所在：批量导入时，
/// 「每次多打一次 JavDB / 插件调用」是实打实的成本。
///
/// 用 `calls` 而不是「返回值是 false」来断言 —— 后者有很多种方式为真。
#[tokio::test]
async fn an_existing_number_is_never_asked_of_the_source_again() {
    let fixture = Fixture::new(Some(javdb_detail("MSI-002"))).await;

    let (first_id, first_created) = fixture
        .source
        .import_by_number(&no_plugins(), "MSI-002", &fixture.imports, false)
        .await
        .expect("第一次导入");
    assert!(first_created);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);

    let (second_id, second_created) = fixture
        .source
        .import_by_number(&no_plugins(), "MSI-002", &fixture.imports, false)
        .await
        .expect("第二次导入");
    assert_eq!(second_id, first_id, "返回的是同一部");
    assert!(!second_created, "第二次不是新建");
    assert_eq!(
        fixture.calls.load(Ordering::SeqCst),
        1,
        "第二次**不该**再问来源一次（短路失效的话这里会是 2）"
    );
}

/// 谁都没有这部片 → `NotFound`（不是 `RequestFailed`）。
///
/// 两者的下游处置完全不同：`NotFound` 是正常结果（客户端提示「没收录」），
/// `RequestFailed` 会被当成故障重试。
#[tokio::test]
async fn nobody_having_the_movie_is_not_found_not_a_failure() {
    // 假 JavDB 返回 `None`（没收录），且没有启用任何插件来源。
    let fixture = Fixture::new(None).await;

    let error = fixture
        .source
        .import_by_number(&no_plugins(), "MSI-003", &fixture.imports, false)
        .await
        .expect_err("都不收录就该报错");

    assert_eq!(error, MetadataSourceError::NotFound);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1, "问过一次 JavDB");
    assert!(
        MovieRepository::new(fixture.db.pool().clone())
            .find_by_number("MSI-003")
            .await
            .expect("查库")
            .is_none(),
        "没收录时不该建出任何记录"
    );
}
