//! `ActorJavdbStreamService::stream_search_and_upsert_actor_from_javdb` 的真库验收。
//!
//! # 验的是「帧序」与「统计」两件事
//!
//! 帧序是**契约**（客户端按帧名画进度条），统计是**账面**（`created_count`
//! 与库里的行数必须对得上）。两者都不能只看返回值的形状 —— 所以这里既断言
//! 帧名序列，也回查真库。
//!
//! # 假 provider，真入库
//!
//! 出网的那一段（JavDB HTTP）打桩，入库那一段用真的
//! [`CatalogImportService`] —— 「一位演员与一条 actor 行」的对应关系只有在
//! 真库上才有意义，用 mock 仓储等于把要验的东西假设掉。
//!
//! # 一处**故意空着**的覆盖：按 canonical id 去重
//!
//! 上游在收尾时按本地 actor id 去重（两位 JavDB 卡片可能已并到同一保留
//! 记录）。本仓的 `upsert_actor_from_javdb_resource` **不解析
//! `merged_into_id`**（按 `javdb_id` 直接取行），所以不同候选必然落到不同
//! 行上，那一步目前**不可达** —— 写了也只会是自欺的绿灯。见服务里的注释。

use std::sync::Arc;

use sm_db::repo::{ActorRepository, NewActor};
use sm_db::testing::TestDb;
use sm_service::catalog::actor_javdb_stream::{ActorJavdbStreamService, ActorStreamFrame};
use sm_service::catalog::catalog_import::CatalogImportService;
use sm_service::catalog::metadata_source::{
    MetadataProvider, MetadataSourceError, MetadataSourceService,
};
use sm_service::catalog::movie_image::{ImageTaskSet, ImageTasksBuilder};
use sm_service::error::ServiceError;

/// 剧本式假 JavDB：`search_actors` 的结果由用例给定。
struct FakeJavdb {
    actors: Result<Vec<serde_json::Value>, MetadataSourceError>,
}

#[tonic::async_trait]
impl MetadataProvider for FakeJavdb {
    async fn get_movie_by_number(
        &self,
        _movie_number: &str,
    ) -> Result<Option<serde_json::Value>, MetadataSourceError> {
        Ok(None)
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
        self.actors.clone()
    }
}

/// 不构造任何图片任务（演员那一路不读它）。
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

fn no_downloader() -> sm_service::catalog::catalog_import::ImageDownloader {
    Box::new(|_url: &str, _path: &std::path::Path| Ok(()))
}

/// 一条演员候选（`JavdbMovieActor` 的线上形状）。
fn actor_resource(javdb_id: &str, name: &str) -> serde_json::Value {
    serde_json::json!({
        "javdb_id": javdb_id,
        "javdb_type": 1,
        "name": name,
        "alias_names": [name],
        "avatar_url": "https://c0.jdbstatic.com/avatars/a.jpg",
        "gender": 2,
    })
}

struct Fixture {
    db: TestDb,
    service: ActorJavdbStreamService,
}

impl Fixture {
    async fn new(actors: Result<Vec<serde_json::Value>, MetadataSourceError>) -> Self {
        let db = TestDb::require().await;
        let imports = CatalogImportService::new(db.pool(), Box::new(NoImages), no_downloader());
        let source = Arc::new(MetadataSourceService::new(
            Vec::new(),
            Some(Box::new(FakeJavdb { actors })),
        ));
        let service = ActorJavdbStreamService::new(db.pool(), source, imports);
        Self { db, service }
    }
}

/// 帧名序列（断言顺序用）。
fn frame_names(frames: &[ActorStreamFrame]) -> Vec<&'static str> {
    frames
        .iter()
        .map(|frame| match frame {
            ActorStreamFrame::SearchStarted { .. } => "search_started",
            ActorStreamFrame::ActorFound { .. } => "actor_found",
            ActorStreamFrame::UpsertStarted { .. } => "upsert_started",
            ActorStreamFrame::ImageDownloadStarted { .. } => "image_download_started",
            ActorStreamFrame::ImageDownloadFinished { .. } => "image_download_finished",
            ActorStreamFrame::UpsertFinished { .. } => "upsert_finished",
            ActorStreamFrame::Completed { .. } => "completed",
        })
        .collect()
}

/// 取最后一帧的 `completed`。
fn completed(frames: &[ActorStreamFrame]) -> (&bool, &Option<&'static str>, Option<i64>) {
    match frames.last().expect("至少有一帧") {
        ActorStreamFrame::Completed {
            success,
            reason,
            stats,
            ..
        } => (
            success,
            reason,
            stats.as_ref().map(|stats| stats.created_count),
        ),
        other => panic!("最后一帧应当是 completed，实际 {other:?}"),
    }
}

// ================================================================ 用例

/// 顺利路径：帧序照上游，且**库里真的多了一位**。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_successful_import_walks_the_upstream_frame_sequence() {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().expect("串行锁");
    let fixture = Fixture::new(Ok(vec![actor_resource("Act-1", "上原")])).await;

    let frames = fixture
        .service
        .stream_search_and_upsert_actor_from_javdb("  上原  ")
        .await;

    assert_eq!(
        frame_names(&frames),
        vec![
            "search_started",
            "actor_found",
            "upsert_started",
            "image_download_started",
            "image_download_finished",
            "upsert_finished",
            "completed",
        ]
    );
    // `search_started` 回显**归一后**的名字（前后空白已去）。
    match &frames[0] {
        ActorStreamFrame::SearchStarted { actor_name } => assert_eq!(actor_name, "上原"),
        other => panic!("首帧应当是 search_started，实际 {other:?}"),
    }
    // `has_avatar` 取自**资源**上有没有头像 URL。
    match &frames[4] {
        ActorStreamFrame::ImageDownloadFinished { has_avatar, .. } => assert!(*has_avatar),
        other => panic!("第 5 帧应当是 image_download_finished，实际 {other:?}"),
    }

    let (success, reason, created) = completed(&frames);
    assert!(success, "该成功");
    assert_eq!(*reason, None, "成功帧不带 reason");
    assert_eq!(created, Some(1));

    // 真库回查：一位演员、`javdb_id` 已绑。
    let actor = ActorRepository::new(fixture.db.pool().clone())
        .find_by_javdb_id("Act-1")
        .await
        .expect("查库")
        .expect("演员应当已入库");
    assert_eq!(actor.name, "上原");
}

/// ★ 搜不到 → 早退帧 `completed {success: false, reason: "actor_not_found"}`，
/// **不带** `stats`/`failed_items`（上游早退帧就没有这两个键）。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_missing_actor_ends_in_an_early_completed_frame() {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().expect("串行锁");
    let fixture = Fixture::new(Err(MetadataSourceError::NotFound)).await;

    let frames = fixture
        .service
        .stream_search_and_upsert_actor_from_javdb("查无此人")
        .await;
    assert_eq!(frame_names(&frames), vec!["search_started", "completed"]);
    match &frames[1] {
        ActorStreamFrame::Completed {
            success,
            reason,
            stats,
            failed_items,
            ..
        } => {
            assert!(!success);
            assert_eq!(*reason, Some("actor_not_found"));
            assert!(stats.is_none(), "早退帧不带 stats");
            assert!(failed_items.is_empty(), "早退帧不带 failed_items");
        }
        other => panic!("实际 {other:?}"),
    }
}

/// 来源挂了 → 同样是早退帧，但 `reason` 是 `internal_error`：
/// 「没搜到」与「搜不了」在客户端是两种提示。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn a_source_failure_ends_in_an_internal_error_frame() {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().expect("串行锁");
    let fixture = Fixture::new(Err(MetadataSourceError::RequestFailed("超时".to_owned()))).await;

    let frames = fixture
        .service
        .stream_search_and_upsert_actor_from_javdb("上原")
        .await;
    let (success, reason, _) = completed(&frames);
    assert!(!success);
    assert_eq!(*reason, Some("internal_error"));
}

/// ★ 一条成一条败：**失败不中断整条流**，好的一条照常入库，
/// 坏的那条进 `failed_items`。这是上游 `except → append → continue` 的语义。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn one_bad_candidate_does_not_stop_the_others() {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().expect("串行锁");
    // 第二条没有 `name` → 入库时 service 层报 `actor_name_missing`。
    let fixture = Fixture::new(Ok(vec![
        actor_resource("Act-ok", "好演员"),
        serde_json::json!({ "javdb_id": "Act-bad", "name": "" }),
    ]))
    .await;

    let frames = fixture
        .service
        .stream_search_and_upsert_actor_from_javdb("好演员")
        .await;

    // 好的一条仍然走完了它的两帧。
    assert_eq!(
        frame_names(&frames),
        vec![
            "search_started",
            "actor_found",
            "upsert_started",
            "image_download_started",
            "image_download_finished",
            "image_download_started",
            "upsert_finished",
            "completed",
        ],
        "坏的那条不发 image_download_finished（它没走到那一步）"
    );
    match frames.last().expect("有收尾帧") {
        ActorStreamFrame::Completed {
            success,
            reason,
            actors,
            failed_items,
            stats,
        } => {
            assert!(success, "有一条成功就是成功");
            assert_eq!(*reason, None);
            assert_eq!(actors.len(), 1);
            let stats = stats.as_ref().expect("成功帧带 stats");
            assert_eq!(
                (stats.total, stats.created_count, stats.failed_count),
                (2, 1, 1)
            );
            assert_eq!(failed_items.len(), 1);
            assert_eq!(failed_items[0].javdb_id, "Act-bad");
            assert_eq!(failed_items[0].reason, "upsert_failed");
        }
        other => panic!("实际 {other:?}"),
    }

    // 好的那条真的在库里；坏的那条不该留下半条记录。
    let repo = ActorRepository::new(fixture.db.pool().clone());
    assert!(repo
        .find_by_javdb_id("Act-ok")
        .await
        .expect("查库")
        .is_some());
    assert!(
        repo.find_by_javdb_id("Act-bad")
            .await
            .expect("查库")
            .is_none(),
        "校验失败不该建出一位空名字的演员"
    );
}

/// 已存在的演员再搜一次：`already_exists_count = 1`、**不新建第二行**。
///
/// 「入库前存在吗」必须在 upsert **之前**问 —— 之后再问永远是 true，
/// `created_count` 会恒为 0。
#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn an_existing_actor_is_counted_as_already_existing() {
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _serial = SERIAL.lock().expect("串行锁");
    let fixture = Fixture::new(Ok(vec![actor_resource("Act-dup", "老演员")])).await;
    ActorRepository::new(fixture.db.pool().clone())
        .insert(&NewActor {
            javdb_id: "Act-dup".to_owned(),
            name: "老演员".to_owned(),
        })
        .await
        .expect("预置");

    let frames = fixture
        .service
        .stream_search_and_upsert_actor_from_javdb("老演员")
        .await;
    match frames.last().expect("有收尾帧") {
        ActorStreamFrame::Completed { stats, .. } => {
            let stats = stats.as_ref().expect("带 stats");
            assert_eq!(
                (stats.created_count, stats.already_exists_count),
                (0, 1),
                "已存在不计入新建"
            );
        }
        other => panic!("实际 {other:?}"),
    }
    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM actor WHERE javdb_id = 'Act-dup'")
        .fetch_one(fixture.db.pool())
        .await
        .expect("计数");
    assert_eq!(rows, 1, "不该建出第二行");
}
