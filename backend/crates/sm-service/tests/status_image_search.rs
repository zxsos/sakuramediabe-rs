//! `GET /status/image-search` 的**装配**测试 —— 三个响应分支各钉一条。
//!
//! # 为什么要测这一层
//!
//! 这个端点不是「读张表」：它把**推理服务**、**Qdrant 集合**、**两张表的计数**、
//! **索引空间状态机**四路合进一个响应，而每一路的失败都**不该**把整页打成 500
//! （上游 `status_service.py:524-573` 把异常都捕成 `error` 字段）。所以「哪几路
//! 会失败、失败时长什么样」得逐分支钉住 —— 而其中最关键的一条，**光看代码看不
//! 出来**：未启用分支到底碰不碰数据库。
//!
//! # 三个分支
//!
//! | 分支 | 条件 | 外部依赖 |
//! |---|---|---|
//! | 未启用 | 配置开关关 | 无（**且必须真的什么都不碰**）|
//! | 启用但服务没建起来 | 开关开、`inference_base_url` 为空 | 真库（只有计数）|
//! | 启用且服务在 | 正常部署 | 真库 + **真 Qdrant** + 打桩推理服务 |
//!
//! 第三分支里 `vector_dtype` / `collection_status` 必须是**真 Qdrant 说的**：
//! `dense.rs` 的单测只锁了「映射函数」这一半，另一半（`collection_info` 里取的
//! 到底是哪个字段、集合建出来是什么类型）只有对着真实例才验得到。
//!
//! # 门禁
//!
//! - 库：`TestDb::require()`（缺库响亮失败，见 `sm_db::testing`）
//! - Qdrant：读 `SMVEC_TEST_QDRANT_URL`（**gRPC** 端口），未设置则 SKIP —— 与
//!   `qdrant_dense.rs` 同一口径
//!
//! 用的 Qdrant 是**生产集合名**（`DenseStore::connect` 绑死），所以这台必须是专用
//! 测试实例。本文件只读集合、不写点，所以不需要像 `qdrant_dense.rs` 那样清库。

mod support;

use std::sync::Arc;
use std::time::Duration;

use sm_db::playback::media::image_search_index_status;
use sm_db::repo::{ImageSearchIndexStateRepository, ImageSearchSessionRepository};
use sm_db::testing::TestDb;
use sm_service::discovery::embedding::EmbeddingClient;
use sm_service::discovery::image_search::{ImageSearchLimits, ImageSearchService};
use sm_service::discovery::image_search_space::{ImageSearchIndexSpaceService, STATE_UNAVAILABLE};
use sm_service::discovery::qdrant::{DenseStore, THUMBNAIL_COLLECTION, THUMBNAIL_PAYLOAD_INDEX};
use sm_service::system::status::{ImageSearchProbe, StatusService};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 假推理服务的空间号 —— 断言 `space_id` 时用它。
const SPACE_ID: &str = "space-status-test";

/// 集合的向量维度。与 `ensure_table` 调用一致即可。
const VECTOR_SIZE: usize = 4;

/// 图搜索引任务的 `task_key`（服务层同名字面量）。
const TASK_KEY: &str = "image_search_index";

/// 打桩推理服务。`modalities` **故意乱序**传 —— 好验「升序」是代码做的。
async fn mount_embedding(server: &MockServer, modalities: &[&str]) {
    Mock::given(method("GET"))
        .and(path("/v1/embedding-space"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "space_id": SPACE_ID,
            "dimension": VECTOR_SIZE,
            "modalities": modalities,
        })))
        .mount(server)
        .await;
}

/// **碰 Qdrant 的用例必须串行。**
///
/// `DenseStore` 绑死**生产集合名**（`media_thumbnail_vectors_siglip2_v1`），而
/// `#[tokio::test]` 默认多线程并行：两条用例同时走 `ensure_table` 的「先查再建」，
/// 一条建成了、另一条就撞 `Collection … already exists`（502）。第一次跑正是这么
/// 炸的 —— 单独跑每条都是绿的。`qdrant_dense.rs` 踩过同一个坑，那里也是用锁。
///
/// 跨文件不用管：cargo 是**逐个测试目标**跑的，不同文件不会同时进行。
static QDRANT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 读 Qdrant 地址；未设置则 `None`（调用方 SKIP）。
fn qdrant_url() -> Option<String> {
    std::env::var("SMVEC_TEST_QDRANT_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// 建一条 `media_thumbnail` 并把它改到指定索引状态。
///
/// `support::seed_thumbnail` 固定写 `0`（待处理），所以要另外改一次。
async fn seed_thumbnail_in_status(db: &TestDb, movie_number: Option<&str>, status: i32) -> i32 {
    let media_id = support::seed_media(db, movie_number).await;
    let image_id = support::seed_image(db, &format!("status/{}.webp", support::n())).await;
    let thumbnail_id = support::seed_thumbnail(db, media_id, image_id, 0).await;
    sqlx::query("UPDATE media_thumbnail SET image_search_index_status = $1 WHERE id = $2")
        .bind(status)
        .bind(thumbnail_id)
        .execute(db.pool())
        .await
        .expect("改索引状态");
    thumbnail_id
}

/// 建一条「在队 / 在跑」的任务行。`params` 直接给 JSON 文本。
async fn seed_task_run(db: &TestDb, state: &str, params: Option<&str>) {
    sqlx::query(
        "INSERT INTO background_task_run \
             (task_key, task_name, trigger_type, state, params) \
         VALUES ($1, '图搜索引', 'manual', $2, $3)",
    )
    .bind(TASK_KEY)
    .bind(state)
    .bind(params)
    .execute(db.pool())
    .await
    .expect("插入 background_task_run");
}

/// 组装一个**启用中的**图搜服务：真 Qdrant + 打桩推理服务 + 真库。
///
/// 返回的服务与 `MockServer` 必须一起活着 —— 前者持有后者的地址。
async fn build_service(db: &TestDb, qdrant: &str, embedding: &MockServer) -> ImageSearchService {
    let store = Arc::new(
        DenseStore::connect(qdrant, None, THUMBNAIL_COLLECTION, THUMBNAIL_PAYLOAD_INDEX)
            .expect("应能连上测试 Qdrant"),
    );
    store
        .ensure_table(VECTOR_SIZE)
        .await
        .expect("建/校验缩略图集合");
    let client = EmbeddingClient::with_http_client(
        reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .connect_timeout(Duration::from_secs(2))
            .no_proxy()
            .build()
            .expect("构建测试用 HTTP 客户端"),
        embedding.uri(),
        None,
    );
    ImageSearchService::new(
        store,
        Arc::new(client),
        ImageSearchSessionRepository::new(db.pool().clone()),
        ImageSearchIndexSpaceService::new(ImageSearchIndexStateRepository::new(db.pool().clone())),
        ImageSearchLimits {
            default_page_size: 20,
            max_page_size: 100,
            session_ttl_seconds: 3600,
        },
    )
}

// ---------------------------------------------------------------- 未启用

/// ★ 未启用时是**一串纯静态值**，且**一次库都不查**。
///
/// 「不查库」这一条是这条用例真正要钉的：上游在 `status_service.py:411-423`
/// 直接 `return`，本仓同形。但两者**看起来**都能编译、也都能返回 `enabled:false`
/// —— 区别只在于有没有顺手去数一下缩略图。所以先播一条待处理缩略图，再断言计数
/// 仍是 0：若哪天有人把计数挪到分支前面，这里会红。
#[tokio::test]
async fn disabled_reports_static_values_and_touches_nothing() {
    let db = TestDb::require().await;
    // 先播一条待处理缩略图：若未启用分支「顺手」去数了它，下面的断言就会红。
    seed_thumbnail_in_status(&db, Some("DIS-000001"), image_search_index_status::PENDING).await;

    let status = StatusService::new(db.pool())
        .get_image_search_status(ImageSearchProbe {
            enabled: false,
            service: None,
            inference_base_url: "http://inference.test:8100",
            qdrant_url: "http://qdrant.test:6334",
        })
        .await
        .expect("未启用分支不该失败");

    assert!(!status.enabled);
    assert!(!status.healthy, "未启用即不健康");

    // 推理服务：整份默认值（全 None + 空模态）。
    assert!(!status.embedding_service.healthy);
    assert_eq!(status.embedding_service.endpoint, None);
    assert_eq!(status.embedding_service.space_id, None);
    assert_eq!(status.embedding_service.dimension, None);
    assert!(status.embedding_service.modalities.is_empty());
    assert_eq!(status.embedding_service.error, None);

    // 向量库：地址与集合名要回（客户端靠它们显示「配置指向哪儿」），其余默认。
    assert!(!status.image_search_vector_store.healthy);
    assert_eq!(
        status.image_search_vector_store.url,
        "http://qdrant.test:6334"
    );
    assert_eq!(
        status.image_search_vector_store.collection_name,
        THUMBNAIL_COLLECTION
    );
    assert!(!status.image_search_vector_store.exists);
    assert_eq!(status.image_search_vector_store.points_count, None);
    assert_eq!(status.image_search_vector_store.vector_size, None);
    assert_eq!(status.image_search_vector_store.vector_dtype, None);
    assert_eq!(status.image_search_vector_store.collection_status, None);
    assert_eq!(status.image_search_vector_store.error, None);

    // ★ 库里**确实**有一条待处理缩略图（上面的播种保证了），这里必须是 0。
    assert_eq!(
        status.indexing.pending_thumbnails, 0,
        "未启用分支不该查库 —— 查了就会数到那条播种的行"
    );
    assert_eq!(status.indexing.failed_thumbnails, 0);

    assert_eq!(status.index_space.state, STATE_UNAVAILABLE);
    assert_eq!(status.index_space.indexed_space_id, None);
    assert_eq!(status.index_space.current_space_id, None);
    assert!(!status.index_space.is_rebuilding);

    // `checked_at` 是这一请求现取的，不是某个常量。
    let now = sm_db::common::time::now_utc();
    let drift = (now - status.checked_at).num_seconds().abs();
    assert!(drift <= 60, "checked_at 该是刚刚（差 {drift}s）");
}

// ---------------------------------------------------------------- 启用但服务没建起来

/// ★ 计数只数 `media_thumbnail`，且**不受「有没有挂 movie」影响**。
///
/// 这条对着的是本切片最容易抄错的一处：仓里另有一个「待索引总数」查询
/// （`PendingImageRepository::pending_count`），它**并了 `movie_plot_image`、还加了
/// `m.movie IS NOT NULL`** —— 那是**索引任务的候选口径**。状态页要的是**展示口径**
/// （上游 `status_service.py:576-590` 就是两次裸 `count()`）。两者互换会让状态页
/// 的数字与任务实际要处理的量对不上，而**没有任何报错**。
///
/// 所以这里特意播一条「没有 movie 归属」的待处理缩略图：展示口径要**算它**，
/// 任务候选口径会**漏掉它**。
#[tokio::test]
async fn counts_only_media_thumbnails_and_ignores_the_movie_join() {
    let db = TestDb::require().await;

    // 有 movie 归属的：两条待处理、一条失败、一条已成功（不该被数）。
    seed_thumbnail_in_status(&db, Some("STA-000001"), image_search_index_status::PENDING).await;
    seed_thumbnail_in_status(&db, Some("STA-000002"), image_search_index_status::FAILED).await;
    seed_thumbnail_in_status(&db, Some("STA-000003"), image_search_index_status::SUCCESS).await;
    // ★ 没有 movie 归属的待处理行：展示口径要算，任务候选口径会漏。
    seed_thumbnail_in_status(&db, None, image_search_index_status::PENDING).await;

    // 用「启用但服务没建起来」那条分支来拿计数：它不碰 Qdrant / 推理服务，
    // 只查库。这正是本节要孤立验的东西。
    let status = StatusService::new(db.pool())
        .get_image_search_status(ImageSearchProbe {
            enabled: true,
            service: None,
            inference_base_url: "",
            qdrant_url: "http://qdrant.test:6334",
        })
        .await
        .expect("该分支只查库，不该失败");

    assert!(status.enabled);
    assert!(!status.healthy);
    assert_eq!(
        status.indexing.pending_thumbnails, 2,
        "两条待处理（含**没有 movie 归属**那条）；已成功的不算"
    );
    assert_eq!(status.indexing.failed_thumbnails, 1);

    // 这一分支下没法探测，但要如实说明原因，而不是假装健康。
    assert!(!status.embedding_service.healthy);
    assert!(status.embedding_service.error.is_some());
    assert!(!status.image_search_vector_store.healthy);
    assert!(status.image_search_vector_store.error.is_some());
    assert_eq!(status.index_space.state, STATE_UNAVAILABLE);
}

// ---------------------------------------------------------------- 启用且服务在

/// ★ 完整分支：四路都是真件（真库 + 真 Qdrant + 打桩推理服务）。
///
/// 重点在两处只有真件才验得到的：
///
/// 1. `vector_dtype` / `collection_status` 是**真 Qdrant 报回来的**聚合结果 ——
///    上游 golden（`tests/api/test_status_api.py:201-202`）是 `"float16"` / `"green"`。
///    `dense.rs` 的单测锁了映射函数，这里锁「取的是不是那个字段」。
/// 2. `healthy` **只看推理服务与向量库**：索引空间哪怕是 `rebuild_required` 也
///    不该把它拉成 false。
#[tokio::test]
async fn enabled_and_reachable_reports_all_four_sources() {
    let Some(qdrant) = qdrant_url() else {
        eprintln!("SKIP: 未设置 SMVEC_TEST_QDRANT_URL —— 请指向专用测试实例的 gRPC 端口");
        return;
    };
    let _guard = QDRANT_LOCK.lock().await;
    let db = TestDb::require().await;
    let embedding = MockServer::start().await;
    // 乱序给，验「升序」是代码做的。
    mount_embedding(&embedding, &["text", "image"]).await;
    let service = build_service(&db, &qdrant, &embedding).await;

    let status = StatusService::new(db.pool())
        .get_image_search_status(ImageSearchProbe {
            enabled: true,
            service: Some(&service),
            inference_base_url: &embedding.uri(),
            qdrant_url: &qdrant,
        })
        .await
        .expect("四路都通时不该失败");

    assert!(status.enabled);
    assert!(status.healthy, "两路都健康：{status:?}");

    // —— 推理服务
    assert!(status.embedding_service.healthy);
    assert_eq!(status.embedding_service.space_id.as_deref(), Some(SPACE_ID));
    assert_eq!(status.embedding_service.dimension, Some(VECTOR_SIZE as u64));
    assert_eq!(
        status.embedding_service.endpoint.as_deref(),
        Some(embedding.uri().as_str())
    );
    assert_eq!(
        status.embedding_service.modalities,
        vec!["image".to_owned(), "text".to_owned()],
        "模态必须升序"
    );
    assert_eq!(status.embedding_service.error, None);

    // —— 向量库（真 Qdrant）
    let store = &status.image_search_vector_store;
    assert!(store.healthy, "{store:?}");
    assert!(store.exists, "`ensure_table` 之后集合应当存在");
    assert_eq!(store.url, qdrant);
    assert_eq!(store.collection_name, THUMBNAIL_COLLECTION);
    assert_eq!(store.points_count, Some(0), "没写过点");
    assert_eq!(store.vector_size, Some(VECTOR_SIZE as u64));
    assert_eq!(
        store.vector_dtype.as_deref(),
        Some("float16"),
        "★ 必须是 REST 风格小写串（上游 golden 是 \"float16\"）"
    );
    let collection_status = store.collection_status.as_deref();
    assert!(
        matches!(collection_status, Some("green" | "yellow" | "red" | "grey")),
        "★ 必须是 REST 风格小写串而不是枚举序数，实际：{collection_status:?}"
    );
    assert_eq!(store.error, None);

    // —— 计数（本用例没播任何缩略图）
    assert_eq!(status.indexing.pending_thumbnails, 0);
    assert_eq!(status.indexing.failed_thumbnails, 0);

    // —— 索引空间：单例表没行 + 没有已完成的索引记录 ⇒ uninitialized。
    //    空间号来自推理服务，所以 `current_space_id` 必须回显它。
    assert_eq!(
        status.index_space.state,
        sm_service::discovery::image_search_space::STATE_UNINITIALIZED,
        "没有索引状态行时是未初始化"
    );
    assert_eq!(
        status.index_space.current_space_id.as_deref(),
        Some(SPACE_ID)
    );
    assert_eq!(status.index_space.indexed_space_id, None);
    assert!(!status.index_space.is_rebuilding, "没播任务行");
}

/// ★ 有 `image_search_index` 任务在跑且带 `reset` → `is_rebuilding` 为真。
///
/// 上游 `_is_image_search_rebuilding`（`status_service.py:592-602`）取那一行的
/// `params.reset`。这条只有完整分支到得了（另两条分支不查任务表），所以单独一条。
#[tokio::test]
async fn a_reset_task_in_flight_marks_the_index_as_rebuilding() {
    let Some(qdrant) = qdrant_url() else {
        eprintln!("SKIP: 未设置 SMVEC_TEST_QDRANT_URL —— 请指向专用测试实例的 gRPC 端口");
        return;
    };
    let _guard = QDRANT_LOCK.lock().await;
    let db = TestDb::require().await;
    let embedding = MockServer::start().await;
    mount_embedding(&embedding, &["image", "text"]).await;
    let service = build_service(&db, &qdrant, &embedding).await;

    // ⚠️ 只播**一条**：仓储层的查询没有 `ORDER BY`，多行时取到哪行由数据库决定
    // （上游同样如此，见 `find_active_by_task_key` 的文档）。播两条会让本用例
    // 时绿时红。
    seed_task_run(&db, "running", Some(r#"{"reset": true}"#)).await;

    let status = StatusService::new(db.pool())
        .get_image_search_status(ImageSearchProbe {
            enabled: true,
            service: Some(&service),
            inference_base_url: &embedding.uri(),
            qdrant_url: &qdrant,
        })
        .await
        .expect("探测不该失败");

    assert!(
        status.index_space.is_rebuilding,
        "状态是 running 且 params.reset 为 true —— 该判为重建中"
    );
}
