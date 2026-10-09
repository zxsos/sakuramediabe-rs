//! `ranking_item` / `image_search_session` / `image_search_index_state`
//! 的集成测试。
//!
//! # 三个模型早就写好了，仓储却不存在
//!
//! `discovery/image_search.rs` 上有 `IMAGE_SEARCH_STATE_ID`、
//! `accepts_session`、`query_vector_dim`、`parsed_movie_ids` —— 全部是等着
//! 仓储来用的。而 `accepts_session` 存在的原因在模型文档里写得很直白：
//!
//! > 若空间已切换而会话未失效，检索结果会**静默出错** —— 不报错，只是变差。
//!
//! 所以本文件最重要的不是「能不能写入」，而是
//! `session_is_usable` 会不会在该拒绝的时候放行。

use chrono::{Duration, NaiveDateTime};
use sm_db::common::page::PageRequest;
use sm_db::discovery::image_search::{image_search_status, IMAGE_SEARCH_STATE_ID};
use sm_db::error::DbError;
use sm_db::repo::{
    ImageSearchIndexStateRepository, ImageSearchSessionRepository, MovieRepository,
    NewImageSearchSession, NewMovie, NewRankingItem, RankingItemRepository,
};
use sm_db::testing::TestDb;

fn page() -> PageRequest {
    PageRequest::new(1, 50).unwrap()
}

fn now() -> NaiveDateTime {
    sm_db::common::time::now_utc()
}

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 建一个 `movie` 行 —— `ranking_item.movie_id` 指向它。
///
/// **`movie_id` 是 NOT NULL**，所以榜单条目必须先有影片。这正是模型文档里
/// 记的那条设计纠正：「榜单数据先于刮削入库」在数据库层面不成立。
async fn seed_movie(db: &TestDb) -> (i32, String) {
    let number = format!("RANK-{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "榜单影片".to_owned(),
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
        .expect("insert movie");
    (movie.id, number)
}

fn ranking(
    source: &str,
    board: &str,
    period: &str,
    rank: i32,
    movie: (i32, String),
) -> NewRankingItem {
    NewRankingItem {
        source_key: source.to_owned(),
        board_key: board.to_owned(),
        period: period.to_owned(),
        rank,
        movie_number: movie.1,
        movie_id: movie.0,
    }
}
// ================================================================ ranking_item

#[tokio::test]
async fn rank_is_unique_within_a_board_so_a_board_can_accumulate_history() {
    // 唯一索引是 `(source_key, board_key, period, rank)` —— rank 只在**某个
    // 榜单内**唯一。所以这张表**可以**累积历史。
    //
    // 这与同文件里的 `daily_recommendation_item` 语义相反：那张表的 `rank`
    // 是**全局**唯一，第二天生成的 rank=1 会与第一天冲突，所以必须每次
    // 清空重写。两张表都在 `discovery/rankings.rs`，很容易被当成同一回事。
    //
    // 本测试固化的是「同一榜单同一名次幂等覆盖，不同名次互不干扰」。
    let db = TestDb::require().await;
    let repo = RankingItemRepository::new(db.pool().clone());
    let board = format!("board{}", n());

    let m1 = seed_movie(&db).await;
    let m2 = seed_movie(&db).await;

    repo.upsert(&ranking("javdb", &board, "2026-10", 1, m1.clone()))
        .await
        .unwrap();
    repo.upsert(&ranking("javdb", &board, "2026-10", 2, m2.clone()))
        .await
        .unwrap();

    // 同一名次再写 —— 幂等覆盖，不报冲突。
    let m3 = seed_movie(&db).await;
    let updated = repo
        .upsert(&ranking("javdb", &board, "2026-10", 1, m3.clone()))
        .await
        .expect("同榜单同名次应幂等覆盖");
    assert_eq!(updated.movie_id, m3.0, "第 1 名应指向新影片");

    let listed = repo
        .list_by_board("javdb", &board, "2026-10")
        .await
        .unwrap();
    assert_eq!(listed.len(), 2, "仍然只有两行，不是三行");
    assert_eq!(listed[0].rank, 1);
    assert_eq!(listed[0].movie_id, m3.0, "第 1 名已被覆盖");
    assert_eq!(listed[1].movie_id, m2.0);
    assert_eq!(
        listed[1].board_identity(),
        ("javdb", board.as_str(), "2026-10")
    );
}

#[tokio::test]
async fn different_boards_and_periods_do_not_collide() {
    // `period` 是唯一索引的一部分，所以「总榜」与「2026-10 月榜」可以
    // 各自有第 1 名。
    let db = TestDb::require().await;
    let repo = RankingItemRepository::new(db.pool().clone());
    let board = format!("multi{}", n());
    let movie = seed_movie(&db).await;

    repo.upsert(&ranking("javdb", &board, "", 1, movie.clone()))
        .await
        .unwrap();
    repo.upsert(&ranking("javdb", &board, "2026-10", 1, movie.clone()))
        .await
        .expect("不同 period 不冲突");

    assert!(repo.list_by_board("javdb", &board, "").await.unwrap()[0].is_all_time());
    assert_eq!(
        repo.list_by_board("javdb", &board, "2026-10")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn rank_zero_is_rejected_because_ranks_start_at_one() {
    let db = TestDb::require().await;
    let repo = RankingItemRepository::new(db.pool().clone());
    let movie = seed_movie(&db).await;

    let err = repo
        .upsert(&ranking(
            "javdb",
            &format!("z{}", n()),
            "",
            0,
            movie.clone(),
        ))
        .await
        .expect_err("名次从 1 开始");
    assert!(matches!(err, DbError::Business { .. }), "{err:?}");

    let err = repo
        .upsert(&ranking("", &format!("z{}", n()), "", 1, movie))
        .await
        .expect_err("source_key 不能为空");
    assert!(err.to_string().contains("source_key"), "{err}");
}

#[tokio::test]
async fn a_ranking_item_cannot_outlive_its_movie() {
    // `ranking_item_movie_id_fk` 是 `ON DELETE CASCADE`，而 `movie_id` 是
    // NOT NULL —— 所以「影片没了但榜单条目还在」这种状态进不来。
    let db = TestDb::require().await;
    let repo = RankingItemRepository::new(db.pool().clone());
    let board = format!("cascade{}", n());
    let movie = seed_movie(&db).await;

    repo.upsert(&ranking("javdb", &board, "", 1, movie.clone()))
        .await
        .unwrap();
    assert_eq!(repo.list_by_movie(movie.0, page()).await.unwrap().total, 1);

    sqlx::query("DELETE FROM movie WHERE id = $1")
        .bind(movie.0)
        .execute(db.pool())
        .await
        .expect("delete movie");

    assert_eq!(
        repo.list_by_movie(movie.0, page()).await.unwrap().total,
        0,
        "影片被删，榜单条目随之消失（CASCADE）"
    );
    assert!(repo
        .list_by_board("javdb", &board, "")
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn boards_can_be_listed_and_deleted() {
    let db = TestDb::require().await;
    let repo = RankingItemRepository::new(db.pool().clone());
    let source = format!("src{}", n());
    let b1 = format!("b1{}", n());
    let b2 = format!("b2{}", n());
    let movie = seed_movie(&db).await;

    repo.upsert(&ranking(&source, &b1, "2026-10", 1, movie.clone()))
        .await
        .unwrap();
    repo.upsert(&ranking(&source, &b1, "2026-11", 2, movie.clone()))
        .await
        .unwrap();
    repo.upsert(&ranking(&source, &b2, "", 1, movie))
        .await
        .unwrap();

    let boards = repo.list_boards(&source, page()).await.unwrap();
    assert_eq!(boards.total, 3, "两个榜单 + 两个周期 = 三个身份三元组");

    assert_eq!(repo.delete_board(&source, &b1, "2026-10").await.unwrap(), 1);
    assert_eq!(
        repo.delete_board(&source, &b1, "2026-10").await.unwrap(),
        0,
        "重复删返回 0"
    );
    assert_eq!(
        repo.list_by_board(&source, &b1, "2026-11")
            .await
            .unwrap()
            .len(),
        1,
        "另一个周期不受影响"
    );
}
// ================================================================ image_search_index_state（单例）

#[tokio::test]
async fn the_state_table_holds_exactly_one_row_and_upsert_keeps_it_that_way() {
    // `id` 恒为 1，且上游写的是 `default=1` 而**不是** auto_increment。
    // 所以第二次写入必须是 upsert —— 用 insert 会撞主键，而「模型换了要
    // 更新空间 id」恰恰是这张表最频繁的写操作。
    let db = TestDb::require().await;
    let repo = ImageSearchIndexStateRepository::new(db.pool().clone());

    let first = repo.set_indexed_space("siglip2-v1").await.unwrap();
    assert_eq!(first.id, IMAGE_SEARCH_STATE_ID);
    assert_eq!(first.indexed_space_id, "siglip2-v1");
    assert!(first.is_singleton_row());

    let second = repo.set_indexed_space("siglip2-v2").await.unwrap();
    assert_eq!(second.id, IMAGE_SEARCH_STATE_ID, "还是同一行");
    assert_eq!(second.indexed_space_id, "siglip2-v2");

    // 全表确实只有一行。
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM image_search_index_state")
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(count, 1, "单例表不能长出第二行");

    // 这张表**没有**时间戳 —— 全库第二张不继承 TimestampedMixin 的表。
    //
    // `table_schema = $1` 不是可选项：每个测试建自己的 schema，而
    // `information_schema.columns` 跨全部 schema 可见。不加过滤会拿到
    // 历史残留的同名表，列名**重复**一份 —— 而用 `.any()` 的那个测试
    // （`this_table_has_no_plugin_ownership_columns`）恰好容忍了重复，
    // 所以只有断言了完整列表的这个测试会发现。
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = $1 AND table_name = 'image_search_index_state' \
         ORDER BY column_name",
    )
    .bind(db.schema())
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        columns,
        vec!["id".to_owned(), "indexed_space_id".to_owned()],
        "只有两列：加时间戳会诱使后来者把它当历史表读"
    );
}

#[tokio::test]
async fn a_blank_space_id_is_rejected() {
    // 空串不是「默认空间」—— 那会让所有会话的兼容性判断失去依据，
    // 而静默的检索错误比一次可解释的拒绝糟糕得多。
    let db = TestDb::require().await;
    let repo = ImageSearchIndexStateRepository::new(db.pool().clone());
    for blank in ["", "   ", "\t"] {
        let err = repo
            .set_indexed_space(blank)
            .await
            .expect_err("空空间 id 应被拒");
        assert!(matches!(err, DbError::Business { .. }), "{err:?}");
    }
}
#[tokio::test]
async fn a_session_is_refused_when_the_embedding_space_has_moved_on() {
    // **本文件最重要的一处。**
    //
    // SigLIP2 一换，嵌入维度就变，客户端会话里存的旧查询向量无法与新索引
    // 比较。若空间已切换而会话仍在有效期内，检索结果会**静默出错** ——
    // 不报错，只是变差。
    //
    // 三个条件全过才可用：未过期、状态 ready、维度相符。这里只测第三项，
    // 前两项在下面两个测试里。
    let db = TestDb::require().await;
    let state = ImageSearchIndexStateRepository::new(db.pool().clone());
    let sessions = ImageSearchSessionRepository::new(db.pool().clone());

    state.set_indexed_space("siglip2-v2").await.unwrap();
    let session = sessions
        .create(&NewImageSearchSession {
            session_id: format!("sess{}", n()),
            page_size: 20,
            // 旧空间的向量：4 维。
            query_vector: Some("[0.1,0.2,0.3,0.4]".to_owned()),
            score_threshold: None,
            expires_at: now() + Duration::hours(1),
        })
        .await
        .unwrap();

    assert_eq!(session.query_vector_dim(), Some(4));

    // 空间现在是 8 维 —— 维度不符，必须拒绝。
    assert!(
        !state.session_is_usable(&session, Some(8)).await.unwrap(),
        "维度不符时必须拒绝 —— 放行会静默产出错误结果"
    );
    // 维度相符才放行。
    assert!(
        state.session_is_usable(&session, Some(4)).await.unwrap(),
        "维度相符才放行"
    );
    // 维度未知时**不阻断** —— 那是「无法判断」，不是「不兼容」。
    assert!(
        state.session_is_usable(&session, None).await.unwrap(),
        "维度未知不该被当成不兼容"
    );
}

#[tokio::test]
async fn an_expired_or_invalidated_session_is_refused_even_when_the_space_matches() {
    // 过期与作废是**两件事**：过期由 `expires_at` 决定，作废由 `status`
    // 决定。一个未过期的会话也可能已作废，所以只查 `expires_at` 不够。
    let db = TestDb::require().await;
    let state = ImageSearchIndexStateRepository::new(db.pool().clone());
    let sessions = ImageSearchSessionRepository::new(db.pool().clone());
    state.set_indexed_space("space-a").await.unwrap();

    let vector = Some("[0.1,0.2]".to_owned());

    // 未过期 + ready -> 放行。
    let good = sessions
        .create(&NewImageSearchSession {
            session_id: format!("ok{}", n()),
            page_size: 20,
            query_vector: vector.clone(),
            score_threshold: None,
            expires_at: now() + Duration::hours(1),
        })
        .await
        .unwrap();
    assert!(state.session_is_usable(&good, Some(2)).await.unwrap());

    // 已过期 -> 拒绝，即使空间与维度都对。
    let expired = sessions
        .create(&NewImageSearchSession {
            session_id: format!("exp{}", n()),
            page_size: 20,
            query_vector: vector.clone(),
            score_threshold: None,
            expires_at: now() - Duration::minutes(1),
        })
        .await
        .unwrap();
    assert!(expired.is_expired(now()));
    assert!(
        !state.session_is_usable(&expired, Some(2)).await.unwrap(),
        "过期会话必须拒绝"
    );

    // 作废（未过期）-> 拒绝。
    let invalidated = sessions
        .create(&NewImageSearchSession {
            session_id: format!("inv{}", n()),
            page_size: 20,
            query_vector: vector,
            score_threshold: None,
            expires_at: now() + Duration::hours(1),
        })
        .await
        .unwrap();
    sqlx::query("UPDATE image_search_session SET status = $2 WHERE id = $1")
        .bind(invalidated.id)
        .bind(image_search_status::INVALID)
        .execute(db.pool())
        .await
        .unwrap();
    let invalidated = sessions
        .find_by_session_id(&invalidated.session_id)
        .await
        .unwrap()
        .unwrap();
    assert!(invalidated.is_invalid());
    assert!(
        !state
            .session_is_usable(&invalidated, Some(2))
            .await
            .unwrap(),
        "作废会话必须拒绝"
    );
}

#[tokio::test]
async fn a_missing_state_row_makes_every_session_unusable() {
    // 状态行缺失是一个**部署顺序**问题：先建会话、后写状态行（或者迁移
    // 漏了这一行）。此时候会话向量与索引向量无法比较，而比较不了就只能
    // 返回错误结果。
    //
    // 所以选「拒绝」而不是「放行」：放行的代价是静默的错误检索。
    //
    // 本测试用一个**带随机后缀的空间 id** 做隔离 —— 因为这是单例表，
    // 删除会影响到并行跑的其他测试。
    let db = TestDb::require().await;
    let state = ImageSearchIndexStateRepository::new(db.pool().clone());
    let sessions = ImageSearchSessionRepository::new(db.pool().clone());

    let marker = format!("space-for-missing-{}", n());
    state.set_indexed_space(&marker).await.unwrap();

    let session = sessions
        .create(&NewImageSearchSession {
            session_id: format!("m{}", n()),
            page_size: 20,
            query_vector: Some("[1.0,2.0]".to_owned()),
            score_threshold: None,
            expires_at: now() + Duration::hours(1),
        })
        .await
        .unwrap();
    assert!(state.session_is_usable(&session, Some(2)).await.unwrap());

    // 把单例行删掉，模拟「状态表还没初始化」。
    sqlx::query("DELETE FROM image_search_index_state WHERE indexed_space_id = $1")
        .bind(&marker)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(state.get().await.unwrap().is_none(), "状态行已删除");

    assert!(
        !state.session_is_usable(&session, Some(2)).await.unwrap(),
        "状态行缺失时必须拒绝 —— 那是静默错误检索的入口"
    );
}
#[tokio::test]
async fn pages_advance_the_cursor_and_exclusions_accumulate() {
    let db = TestDb::require().await;
    let repo = ImageSearchSessionRepository::new(db.pool().clone());
    let session_id = format!("page{}", n());

    let session = repo
        .create(&NewImageSearchSession {
            session_id: session_id.clone(),
            page_size: 20,
            query_vector: Some("[1.0,2.0,3.0]".to_owned()),
            score_threshold: Some(0.75),
            expires_at: now() + Duration::hours(1),
        })
        .await
        .unwrap();
    assert!(!session.has_next_page(), "首轮没有下一页");
    assert_eq!(session.page_size, 20);
    assert_eq!(session.score_threshold, Some(0.75));

    // 第一页。
    assert!(repo
        .save_page(&session_id, &[1, 2, 3], Some("cursor-2"))
        .await
        .unwrap());
    let after = repo.find_by_session_id(&session_id).await.unwrap().unwrap();
    assert_eq!(after.parsed_movie_ids(), Some(vec![1, 2, 3]));
    assert!(after.has_next_page());

    // 第二页：游标推进，同时把第一页排除掉，避免重复命中。
    assert!(repo.set_exclusions(&session_id, &[1, 2, 3]).await.unwrap());
    assert!(repo.save_page(&session_id, &[4, 5], None).await.unwrap());
    let after = repo.find_by_session_id(&session_id).await.unwrap().unwrap();
    assert_eq!(after.parsed_movie_ids(), Some(vec![4, 5]));
    assert_eq!(after.parsed_exclude_movie_ids(), Some(vec![1, 2, 3]));
    assert!(!after.has_next_page(), "游标为 None 表示没有下一页了");
    assert_eq!(after.query_vector_dim(), Some(3));

    // 会话不存在时返回 false 而不是报错 —— 清理任务可能刚把它删掉。
    assert!(!repo.save_page("no-such-session", &[1], None).await.unwrap());
}

#[tokio::test]
async fn expired_sessions_are_listed_and_reaped_by_their_index() {
    // `image_search_session_expires_at_idx` 存在**就是为了**清理任务。
    let db = TestDb::require().await;
    let repo = ImageSearchSessionRepository::new(db.pool().clone());

    let stale = format!("stale{}", n());
    let fresh = format!("fresh{}", n());
    repo.create(&NewImageSearchSession {
        session_id: stale.clone(),
        page_size: 20,
        query_vector: None,
        score_threshold: None,
        expires_at: now() - Duration::hours(2),
    })
    .await
    .unwrap();
    repo.create(&NewImageSearchSession {
        session_id: fresh.clone(),
        page_size: 20,
        query_vector: None,
        score_threshold: None,
        expires_at: now() + Duration::hours(2),
    })
    .await
    .unwrap();

    let expired = repo.list_expired(now(), page()).await.unwrap();
    let ids: Vec<&str> = expired
        .items
        .iter()
        .map(|s| s.session_id.as_str())
        .collect();
    assert!(ids.contains(&stale.as_str()), "过期会话应被列出: {ids:?}");
    assert!(!ids.contains(&fresh.as_str()), "未过期不该出现");
    assert_eq!(
        expired.total as usize,
        expired.items.len(),
        "total 与 items 必须是同一个集合"
    );

    // 清理。只删过期那个。
    let reaped = repo.delete_expired(now()).await.unwrap();
    assert!(reaped >= 1);
    assert!(
        repo.find_by_session_id(&fresh).await.unwrap().is_some(),
        "未过期的必须留着"
    );
    assert!(repo.find_by_session_id(&stale).await.unwrap().is_none());
}

#[tokio::test]
async fn session_ids_are_unique_and_blank_ones_are_rejected() {
    let db = TestDb::require().await;
    let repo = ImageSearchSessionRepository::new(db.pool().clone());
    let session_id = format!("uniq{}", n());

    repo.create(&NewImageSearchSession {
        session_id: session_id.clone(),
        page_size: 20,
        query_vector: None,
        score_threshold: None,
        expires_at: now() + Duration::hours(1),
    })
    .await
    .unwrap();

    let err = repo
        .create(&NewImageSearchSession {
            session_id: session_id.clone(),
            page_size: 20,
            query_vector: None,
            score_threshold: None,
            expires_at: now() + Duration::hours(1),
        })
        .await
        .expect_err("session_id 有 UNIQUE 约束");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "{err:?}"
    );

    for blank in ["", "   "] {
        assert!(repo
            .create(&NewImageSearchSession {
                session_id: blank.to_owned(),
                page_size: 20,
                query_vector: None,
                score_threshold: None,
                expires_at: now() + Duration::hours(1),
            })
            .await
            .is_err());
    }
}
