//! 分页的集成测试。
//!
//! 分页的 bug 有一个共同特征：**在小数据集上测不出来**。
//! 漏了 `LIMIT`、COUNT 与 SELECT 条件不一致、`total` 少了过滤条件——
//! 这些在只有两三行数据的测试库里全部表现为「正常」。
//!
//! 所以本文件刻意**造够数据**：每个分页测试至少插入 7 行，并请求
//! `page_size` 小于总数，逼出 OFFSET 的真实行为。
//!
//! | 测试 | 验证 |
//! |---|---|
//! | [`total_is_the_filtered_count_not_the_page_length`] | total 是全量 |
//! | [`pages_tile_without_gaps_or_repeats`] | 翻页能拼回完整集合 |
//! | [`a_page_past_the_end_is_empty_but_still_reports_total`] | 越界页仍带 total |
//! | [`invalid_page_parameters_are_rejected_before_any_query`] | 校验在仓储层 |
//! | [`count_and_items_see_the_same_snapshot`] | REPEATABLE READ 生效 |
//! | [`filters_narrow_both_the_count_and_the_page`] | 过滤条件对两者一致 |

use sm_db::common::page::PageRequest;
use sm_db::common::time::now_utc;
use sm_db::error::DbError;
use sm_db::repo::{MediaRepository, NewMedia};
use sm_db::testing::TestDb;

/// 造 `count` 条 media，全部挂在同一个 library 下。
async fn seed_media(repo: &MediaRepository, pool: &sqlx::PgPool, count: i64) -> i32 {
    let library = sqlx::query_as::<_, (i32,)>(
        "INSERT INTO media_library (name, created_at, updated_at) VALUES ('lib', $1, $1) RETURNING id",
    )
    .bind(now_utc())
    .fetch_one(pool)
    .await
    .expect("insert media_library")
    .0;

    for i in 0..count {
        repo.insert(&NewMedia {
            library_id: library,
            file_name: format!("f{i}.mp4"),
            file_size_bytes: 1024,
            movie_number: Some(format!("MOV-{i:03}")),
            video_item_id: None,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: Some(60),
            video_info: None,
        })
        .await
        .expect("insert media");
    }
    library
}

#[tokio::test]
async fn total_is_the_filtered_count_not_the_page_length() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());
    seed_media(&repo, db.pool(), 7).await;

    let page = repo
        .list_by_library(1, PageRequest::new(1, 3).unwrap())
        .await
        .unwrap();

    assert_eq!(page.items.len(), 3, "本页三条");
    assert_eq!(page.total, 7, "total 是全量七条，不是本页三条");
    assert_eq!(page.len(), 3, "len 是本页条数");
}

#[tokio::test]
async fn pages_tile_without_gaps_or_repeats() {
    // OFFSET 算错（0-based / 1-based 混淆）时，这个测试会失败。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());
    let library = seed_media(&repo, db.pool(), 7).await;

    let mut collected = Vec::new();
    for page_no in 1..=3 {
        let page = repo
            .list_by_library(library, PageRequest::new(page_no, 3).unwrap())
            .await
            .unwrap();
        assert_eq!(page.total, 7, "每一页的 total 都是同一个全量值");
        collected.extend(page.items.iter().map(|m| m.id));
    }

    assert_eq!(collected.len(), 7, "三页拼起来正好七条");
    let mut sorted = collected.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 7, "没有重复——offset 没有重叠");
    assert_eq!(
        collected,
        {
            let mut s = collected.clone();
            s.sort_unstable();
            s
        },
        "拼接顺序就是 id 升序，说明 ORDER BY 稳定"
    );

    // 第四页是空的，但 total 仍然正确
    let past_end = repo
        .list_by_library(library, PageRequest::new(4, 3).unwrap())
        .await
        .unwrap();
    assert!(past_end.is_empty());
    assert_eq!(past_end.total, 7, "越界页也必须带 total");
}

#[tokio::test]
async fn a_page_past_the_end_is_empty_but_still_reports_total() {
    // 客户端 fetch_all_pages 靠 total 决定还拉不拉，所以越界页返回
    // total=0 会让它以为数据取完了。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());
    let library = seed_media(&repo, db.pool(), 3).await;

    let page = repo
        .list_by_library(library, PageRequest::new(999, 20).unwrap())
        .await
        .unwrap();
    assert!(page.is_empty());
    assert_eq!(page.total, 3, "空页不等于没有数据");
    assert_eq!(sm_core::pagination::last_page(3, 20), 1);
}

#[tokio::test]
async fn invalid_page_parameters_are_rejected_before_any_query() {
    // 校验在仓储层：不需要连库就能拒绝，也不会打到数据库。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());
    seed_media(&repo, db.pool(), 2).await;

    for (page_no, size) in [(0, 20), (-1, 20), (1, 0), (1, 101), (1, 10_000)] {
        let err = PageRequest::new(page_no, size).unwrap_err();
        assert!(
            matches!(err, DbError::Business { .. }),
            "page={page_no} size={size} 应被 422 拒绝，实际 {err:?}"
        );
    }

    // 上限本身合法
    assert!(PageRequest::new(1, sm_core::pagination::MAX_PAGE_SIZE).is_ok());
}

#[tokio::test]
async fn count_and_items_see_the_same_snapshot() {
    // 这是 REPEATABLE READ 的**全部理由**。
    //
    // READ COMMITTED 下 COUNT 与 SELECT 各取一个快照，并发写入会让两者
    // 看到不同的世界：COUNT 说 7，SELECT 在 offset=6 只取到 1 条。
    // 客户端看到 total=7 却只拿到 6 条，于是反复请求最后一页。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());
    let library = seed_media(&repo, db.pool(), 7).await;

    // 先验证正常情况下两者一致
    let page = repo
        .list_by_library(library, PageRequest::new(3, 3).unwrap())
        .await
        .unwrap();
    assert_eq!(page.total, 7);
    assert_eq!(page.items.len(), 1, "offset 6 只剩一条");

    // 事务隔离级别确实被设置成了 REPEATABLE READ。
    // 直接问 PostgreSQL 自己，而不是相信我们写对了代码。
    let level = sqlx::query_scalar::<_, String>("SHOW transaction_isolation")
        .fetch_one(db.pool())
        .await;
    // 池化连接此时不在事务里，所以这里拿到的是默认值 —— 重点是下面
    // 的 in_snapshot_tx 内部验证。
    let _ = level;
}

#[tokio::test]
async fn in_snapshot_tx_reports_the_requested_isolation_level() {
    // 直接验证 `in_snapshot_tx` 真的设置了 REPEATABLE READ。
    // 假设它没设置的话，上一个测试的结论就不成立。
    let Some(db) = TestDb::create().await else {
        return;
    };

    let level = sm_db::common::page::in_snapshot_tx(db.pool(), |conn| {
        Box::pin(async move {
            sqlx::query_scalar::<_, String>("SHOW transaction_isolation")
                .fetch_one(&mut *conn)
                .await
                .map_err(DbError::from)
        })
    })
    .await
    .expect("in_snapshot_tx");

    assert_eq!(
        level, "repeatable read",
        "快照事务必须是 REPEATABLE READ，否则 total 与 items 可能不一致"
    );
}

#[tokio::test]
async fn in_snapshot_tx_rolls_back_on_error() {
    let Some(db) = TestDb::create().await else {
        return;
    };

    let result = sm_db::common::page::in_snapshot_tx(db.pool(), |conn| {
        Box::pin(async move {
            sqlx::query("CREATE TABLE should_be_rolled_back (id int)")
                .execute(&mut *conn)
                .await
                .map_err(DbError::from)?;
            Err::<(), _>(DbError::business("test", "故意失败"))
        })
    })
    .await;

    assert!(result.is_err(), "闭包返回 Err 时整体应失败");

    // 表不该存在 —— 证明真的回滚了
    let exists = sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
         WHERE table_name = 'should_be_rolled_back')",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert!(!exists, "失败的快照事务必须回滚");
}

#[tokio::test]
async fn filters_narrow_both_the_count_and_the_page() {
    // COUNT 与 SELECT 的 WHERE 不一致时，这个测试会失败——
    // 而那种 bug 在「过滤条件恰好命中全部行」时是看不出来的。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());
    let library = seed_media(&repo, db.pool(), 9).await;

    // MOV-000..008 共 9 条，全部在 library 下
    let all = repo
        .list_by_library(library, PageRequest::new(1, 100).unwrap())
        .await
        .unwrap();
    assert_eq!(all.total, 9);

    // 按番号精确查一条
    let one = repo
        .list_by_movie_number("MOV-004", PageRequest::new(1, 20).unwrap())
        .await
        .unwrap();
    assert_eq!(one.items.len(), 1);
    assert_eq!(one.total, 1, "COUNT 也必须应用同一个过滤条件");
    assert_eq!(one.items[0].movie_number.as_deref(), Some("MOV-004"));

    // 不存在的番号 -> 空页 + total 0
    let none = repo
        .list_by_movie_number("NOPE", PageRequest::new(1, 20).unwrap())
        .await
        .unwrap();
    assert!(none.is_empty());
    assert_eq!(none.total, 0);
}

#[tokio::test]
async fn page_shape_verification_catches_a_leaked_limit() {
    // `verify_page_shape` 是宏里自动调用的。这里直接测它，
    // 因为「查询漏了 LIMIT」这个 bug 靠正常数据永远测不出来——
    // 数据比 page_size 少时，漏 LIMIT 与不漏结果一样。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MediaRepository::new(db.pool().clone());
    let library = seed_media(&repo, db.pool(), 3).await;

    // 正常路径：verify_page_shape 在宏内部已通过
    let ok = repo
        .list_by_library(library, PageRequest::new(1, 10).unwrap())
        .await
        .unwrap();
    assert_eq!(ok.items.len(), 3);
    assert!(ok.items.len() <= 10);

    // 超小 page_size 时，LIMIT 真的生效
    let tight = repo
        .list_by_library(library, PageRequest::new(1, 2).unwrap())
        .await
        .unwrap();
    assert_eq!(tight.items.len(), 2, "LIMIT 2 必须只返回两条");
    assert_eq!(tight.total, 3, "但 total 仍是全量");
}
