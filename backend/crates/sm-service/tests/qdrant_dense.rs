//! `qdrant` 存储层对**真实 Qdrant 实例**的集成测试。
//!
//! 参照物：上游 `upstream/sakuramediabe/src/service/discovery/qdrant_thumbnail_store.py`
//! 与 `qdrant_plot_image_store.py`。
//!
//! # 为什么要真实例
//!
//! 这一层要验的东西**只有真 Qdrant 能验**：COSINE + FLOAT16 的实际行为、
//! `MatchAny` 过滤的语义、HNSW/optimizer 参数是否被接受、
//! `upsert(wait=true)` 之后立刻 `query` 能不能读到。用 mock 客户端验这些
//! 等于验「我调了 mock」。
//!
//! # 门禁
//!
//! 读 `SMVEC_TEST_QDRANT_URL`（**gRPC** 地址，如 `http://127.0.0.1:6334`），
//! 未设置就 SKIP —— 与 `sm_db::testing` 对 `SMDB_TEST_DATABASE_URL` 的做法
//! 一致。端口是 6334 而不是 6333：本客户端走 gRPC，6333 是 REST。
//!
//! 用的是**生产集合名**（`ThumbnailVectorStore` 内部绑死），所以每个用例
//! 前后都 `clear()`。这台 Qdrant **必须是专用测试实例**。

use sm_service::discovery::qdrant::{
    PlotImageVectorRecord, PlotImageVectorStore, ThumbnailVectorRecord, ThumbnailVectorStore,
    THUMBNAIL_PAYLOAD_INDEX,
};

/// **所有用例必须串行**。
///
/// 存储层内部绑死了**生产集合名**（`media_thumbnail_vectors_siglip2_v1` 等），
/// 而 `#[tokio::test]` 默认多线程并行 —— 并行时一个用例在建集合、另一个
/// 同时在建，`create_collection` 就返回「已存在」而整条用例红。
///
/// 第一次跑就是这么炸的：11 个失败全是 502，而单独跑每一个都绿。
///
/// 不改成「每用例一个集合名」是因为那要动生产代码的接口（集合名是绑定
/// 关系的一部分，正是要测的东西）。用锁串行是这里最诚实的做法。
static TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 读测试实例地址；未设置则返回 `None`（调用方 SKIP）。
fn qdrant_url() -> Option<String> {
    std::env::var("SMVEC_TEST_QDRANT_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

/// 取出实例地址并拿到串行锁。
///
/// 返回的 `MutexGuard` 要被调用方持有到用例结束，所以它同时是「已就绪」
/// 标记 —— 拿不到地址就 `return`，用例直接跳过。
macro_rules! qdrant_or_skip {
    () => {
        match qdrant_url() {
            Some(url) => {
                let guard = TEST_LOCK.lock().await;
                (url, guard)
            }
            None => {
                eprintln!(
                    "SKIP: 未设置 SMVEC_TEST_QDRANT_URL —— 请指向专用测试实例的 \
                     gRPC 端口，如 http://127.0.0.1:6334"
                );
                return;
            }
        }
    };
}

fn thumbnails(url: &str) -> ThumbnailVectorStore {
    ThumbnailVectorStore::connect(url, None).expect("应能连上测试实例")
}

fn plot_images(url: &str) -> PlotImageVectorStore {
    PlotImageVectorStore::connect(url, None).expect("应能连上测试实例")
}

/// 造「单位向量 + 不同角度」，避免所有向量相同导致 COSINE 分数全一样、
/// 排序断言失去意义。
fn vector(seed: f32) -> Vec<f32> {
    vec![seed.cos(), seed.sin(), 0.0, 0.0]
}

fn record(
    thumbnail_id: i64,
    media_id: i64,
    movie_id: i64,
    offset: i64,
    seed: f32,
) -> ThumbnailVectorRecord {
    ThumbnailVectorRecord {
        thumbnail_id,
        media_id,
        movie_id,
        offset_seconds: offset,
        vector: vector(seed),
    }
}

// ---------------------------------------------------------------- 建表

/// 建表后再用**相同**维度调一次必须成功（走校验分支，不是创建分支）。
#[tokio::test]
async fn ensure_table_is_idempotent_for_the_same_size() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");

    store.ensure_table(8).await.expect("首次建表");
    store
        .ensure_table(8)
        .await
        .expect("同维度重复建表应走校验并通过");
    assert!(store.inner().exists().await);
    store.clear().await.expect("清库");
}

/// **核心防护**：换了 embedding 模型（维度不同）必须报错，不能默默接受。
///
/// 这条对应 `ImageSearchIndexState` / `accepts_session` 那套防护的服务端
/// 一半 —— 客户端不报，索引就会写成一个查不出来的集合，而症状是
/// 「搜出来的都不相似」，极难定位。
#[tokio::test]
async fn ensure_table_rejects_a_different_vector_size() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");

    store.ensure_table(8).await.expect("首次建表");
    let err = store.ensure_table(16).await.expect_err("维度不同必须报错");
    assert_eq!(
        err.status, 409,
        "维度不符是 409（要重建索引），不是 502/503"
    );
    assert_eq!(err.code(), "vector_store_mismatch");
    assert!(
        err.api.message.contains("vector size"),
        "错误消息应指明是 vector size 这项：{}",
        err.api.message
    );
    store.clear().await.expect("清库");
}

/// 维度为 0 是无效参数，要在本地就拒掉，不发请求。
#[tokio::test]
async fn ensure_table_rejects_a_zero_vector_size() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    let err = store.ensure_table(0).await.expect_err("维度 0 必须报错");
    assert_eq!(err.status, 422);
}

// ------------------------------------------------------------ 写入与计数

#[tokio::test]
async fn upsert_then_count_reflects_the_number_of_points() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");
    store.ensure_table(4).await.expect("建表");

    assert_eq!(store.count().await.expect("空集合计数"), 0);

    store
        .upsert_records(&[
            record(1, 10, 100, 0, 0.0),
            record(2, 10, 100, 5, 0.5),
            record(3, 11, 200, 0, 1.0),
        ])
        .await
        .expect("写入");
    assert_eq!(store.count().await.expect("计数"), 3);

    // 重复写同一个 id 是 upsert 语义，不应产生第二行
    store
        .upsert_records(&[record(1, 10, 100, 99, 0.0)])
        .await
        .expect("重复 upsert");
    assert_eq!(
        store.count().await.expect("计数"),
        3,
        "同 id 应覆盖而不是新增"
    );

    // 空批次不报错
    store.upsert_records(&[]).await.expect("空批次应直接成功");
    assert_eq!(store.count().await.expect("计数"), 3);
    store.clear().await.expect("清库");
}

// ---------------------------------------------------------------- 检索

#[tokio::test]
async fn search_returns_normalized_scores_and_the_right_payload() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");
    store.ensure_table(4).await.expect("建表");
    store
        .upsert_records(&[record(1, 10, 100, 0, 0.0), record(2, 10, 100, 5, 1.0)])
        .await
        .expect("写入");

    let hits = store
        .search(&vector(0.0), 10, 0, None, None)
        .await
        .expect("检索");
    assert_eq!(hits.len(), 2);

    assert_eq!(hits[0].thumbnail_id, 1, "最相似的应排第一");
    assert_eq!(hits[0].media_id, 10);
    assert_eq!(hits[0].movie_id, 100);
    assert_eq!(hits[0].offset_seconds, 0);
    assert!(
        (hits[0].score - 1.0).abs() < 1e-4,
        "完全相同应归一化到 1.0，实际 {}",
        hits[0].score
    );
    for hit in &hits {
        assert!(
            (0.0..=1.0).contains(&hit.score),
            "分数必须落在 [0,1]，实际 {}",
            hit.score
        );
    }
    store.clear().await.expect("清库");
}

/// 集合**不存在**时检索返回空列表，**不是错误**。
///
/// 对应上游 `:416-417`：图搜是增强功能，「还没索引过」不该让页面 500。
#[tokio::test]
async fn search_on_a_missing_collection_is_empty_not_an_error() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");

    let hits = store
        .search(&vector(0.0), 10, 0, None, None)
        .await
        .expect("集合不存在时不得报错");
    assert!(hits.is_empty(), "集合不存在应返回空列表");
}

#[tokio::test]
async fn limit_zero_is_rejected() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    let err = store
        .search(&vector(0.0), 0, 0, None, None)
        .await
        .expect_err("limit=0 必须报错");
    assert_eq!(err.status, 422);
}

// ---------------------------------------------------------------- 过滤

/// `movie_ids` 是**包含**过滤。
#[tokio::test]
async fn movie_ids_filter_restricts_to_those_movies() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");
    store.ensure_table(4).await.expect("建表");
    store
        .upsert_records(&[
            record(1, 10, 100, 0, 0.0),
            record(2, 10, 100, 1, 0.1),
            record(3, 11, 200, 0, 0.2),
            record(4, 11, 200, 1, 0.3),
        ])
        .await
        .expect("写入");

    let hits = store
        .search(&vector(0.0), 10, 0, Some(&[200]), None)
        .await
        .expect("按 movie_id 过滤检索");
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_eq!(hit.movie_id, 200, "只应返回 movie_id=200 的命中");
    }
    store.clear().await.expect("清库");
}

/// `exclude_movie_ids` 是**排除**过滤。
#[tokio::test]
async fn exclude_movie_ids_filter_drops_those_movies() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");
    store.ensure_table(4).await.expect("建表");
    store
        .upsert_records(&[
            record(1, 10, 100, 0, 0.0),
            record(2, 10, 100, 1, 0.1),
            record(3, 11, 200, 0, 0.2),
        ])
        .await
        .expect("写入");

    let hits = store
        .search(&vector(0.0), 10, 0, None, Some(&[200]))
        .await
        .expect("排除 movie_id=200");
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_ne!(hit.movie_id, 200, "movie_id=200 的必须被排除");
    }
    store.clear().await.expect("清库");
}

/// 过滤字段要先建标量索引，否则退化成全量扫描。重复调用必须安全。
#[tokio::test]
async fn scalar_indices_are_built_and_idempotent() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");
    store.ensure_table(4).await.expect("建表");

    store
        .ensure_scalar_indices()
        .await
        .expect("建 payload 索引");
    store
        .ensure_scalar_indices()
        .await
        .expect("重复建索引应安全");
    assert_eq!(
        THUMBNAIL_PAYLOAD_INDEX.len(),
        2,
        "缩略图要索引 movie_id 与 media_id 两个字段"
    );
    store.clear().await.expect("清库");
}

// ---------------------------------------------------------------- 删除

#[tokio::test]
async fn delete_by_thumbnail_ids_removes_exactly_those() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");
    store.ensure_table(4).await.expect("建表");
    store
        .upsert_records(&[record(1, 10, 100, 0, 0.0), record(2, 10, 100, 1, 0.1)])
        .await
        .expect("写入");

    store
        .delete_by_thumbnail_ids(&[1])
        .await
        .expect("按缩略图 id 删除");
    assert_eq!(store.count().await.expect("计数"), 1, "只应删掉 id=1 那个");

    store
        .delete_by_thumbnail_ids(&[])
        .await
        .expect("空列表应直接成功");
    assert_eq!(store.count().await.expect("计数"), 1);

    store
        .delete_by_thumbnail_ids(&[2, 2, 2])
        .await
        .expect("重复 id 应被去重");
    assert_eq!(store.count().await.expect("计数"), 0);
    store.clear().await.expect("清库");
}

#[tokio::test]
async fn delete_by_media_id_removes_all_thumbnails_of_that_media() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");
    store.ensure_table(4).await.expect("建表");
    store
        .upsert_records(&[
            record(1, 10, 100, 0, 0.0),
            record(2, 10, 100, 1, 0.1),
            record(3, 11, 100, 0, 0.2),
        ])
        .await
        .expect("写入");

    store
        .delete_by_media_id(10)
        .await
        .expect("按 media_id 删除");
    assert_eq!(
        store.count().await.expect("计数"),
        1,
        "media=10 的两个缩略图都该被删"
    );
    store.clear().await.expect("清库");
}

/// `clear` 删的是**整个集合**，集合不存在时也算成功。
#[tokio::test]
async fn clear_deletes_the_collection_and_is_idempotent() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");
    store.ensure_table(4).await.expect("建表");
    assert!(store.inner().exists().await);

    store.clear().await.expect("清库");
    assert!(!store.inner().exists().await, "clear 后集合不该存在");
    store.clear().await.expect("集合已不存在时 clear 仍应成功");
}

// ------------------------------------------------------------ 剧照存储

/// 剧照集合与缩略图集合**相互独立**。
#[tokio::test]
async fn plot_image_collection_is_independent_of_thumbnail() {
    let (url, _guard) = qdrant_or_skip!();
    let thumbnail_store = thumbnails(&url);
    let plot_store = plot_images(&url);
    thumbnail_store.clear().await.expect("清缩略图");
    plot_store.clear().await.expect("清剧照");

    thumbnail_store.ensure_table(4).await.expect("建缩略图表");
    plot_store.ensure_table(4).await.expect("建剧照表");

    thumbnail_store
        .upsert_records(&[record(1, 10, 100, 0, 0.0)])
        .await
        .expect("写缩略图");
    plot_store
        .upsert_records(&[PlotImageVectorRecord {
            plot_image_id: 77,
            movie_id: 100,
            vector: vector(0.0),
        }])
        .await
        .expect("写剧照");

    // 清掉缩略图，剧照应毫发无损
    thumbnail_store.clear().await.expect("清缩略图");
    assert_eq!(
        plot_store.count().await.expect("剧照计数"),
        1,
        "剧照不该被缩略图的 clear 带走"
    );

    let hits = plot_store
        .search(&vector(0.0), 10, 0, None, None)
        .await
        .expect("检索剧照");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].plot_image_id, 77);
    assert_eq!(hits[0].movie_id, 100);
    assert!(
        (hits[0].score - 1.0).abs() < 1e-4,
        "完全相同应归一化到 1.0，实际 {}",
        hits[0].score
    );

    plot_store
        .delete_by_plot_image_ids(&[77])
        .await
        .expect("按剧照 id 删除");
    assert_eq!(plot_store.count().await.expect("剧照计数"), 0);
    plot_store.clear().await.expect("清剧照");
}

/// 状态：集合不存在时 `exists=false` 且点数为 0。
#[tokio::test]
async fn status_reports_absence_then_presence() {
    let (url, _guard) = qdrant_or_skip!();
    let store = thumbnails(&url);
    store.clear().await.expect("清库");

    let absent = store.status().await.expect("状态查询");
    assert!(!absent.exists);
    assert_eq!(absent.points, 0);

    store.ensure_table(4).await.expect("建表");
    store
        .upsert_records(&[record(1, 10, 100, 0, 0.0), record(2, 10, 100, 1, 0.1)])
        .await
        .expect("写入");

    let present = store.status().await.expect("状态查询");
    assert!(present.exists);
    assert_eq!(present.points, 2);
    store.clear().await.expect("清库");
}
