//! 每日推荐**生成侧**的真库验收（`generate_latest_snapshot`）。
//!
//! # 这层测什么
//!
//! 读侧那批（`sm-api/tests/daily_recommendations_http.rs`）是**直接往表里塞
//! 快照行**再读出来 —— 它证明不了生成侧写进去的形状对不对。这里反过来：
//! 造影片 / 演员 / 榜单 / 最近播放，跑**真的生成**，再读回快照表。
//!
//! | 判据 | 期望 | 为什么必须真库 |
//! |---|---|---|
//! | 候选只含「非集合、未拉黑」 | 拉黑 / 集合片不入快照 | 过滤在 SQL 里，纯函数测不到 |
//! | `rank` 从 1 起、被 `limit` 截断 | `[1, 2]` | 唯一的 `rank` 约束只有真库会撞 |
//! | 重复生成**整体替换** | 旧批不残留 | upsert 会在唯一约束上炸，而「炸」才是正确行为 |
//! | 四路装载器真的读到行 | `subscribed_actor` 信号 = 1.0 | 列名 / 关联方向写错只有跑起来才知道 |
//!
//! # Qdrant 全程缺席，这是**刻意的**
//!
//! 六路信号里相似度那一路依赖 Qdrant。这里每次生成都传 `None`，于是它走
//! 「降级成空表」那条路 —— 那正是上游 `:196-199` 捕获 `MovieSimilarityIndexError`
//! 的行为，也是 CI（没有 Qdrant）里唯一能覆盖的形状。

use std::collections::HashMap;

use sm_db::repo::{
    ActorRepository, DailyRecommendationItemRepository, MovieActorRepository, MovieRepository,
    NewActor, NewMovie, NewRankingItem, RankingItemRepository,
};
use sm_db::testing::TestDb;
use sm_db::DailyRecommendationItem;
use sm_service::discovery::daily_recommendation::{
    DailyRecommendationService, SnapshotStats, DAILY_RECOMMENDATION_LIMIT,
};

/// 全局自增，用来造互不冲突的番号。
fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 跑一次生成。**相似度恒传 `None`**（见模块文档）。
async fn generate(db: &TestDb, limit: i64) -> SnapshotStats {
    DailyRecommendationService::new(db.pool())
        .generate_latest_snapshot(None, limit, None, None)
        .await
        .expect("生成快照")
}

async fn snapshot_rows(db: &TestDb) -> Vec<DailyRecommendationItem> {
    DailyRecommendationItemRepository::new(db.pool().clone())
        .list_by_rank()
        .await
        .expect("读回快照")
}

/// 造一部影片，返回 `(id, 番号)`。`heat` 走 `UPDATE` —— `NewMovie` 不含计数器列。
async fn seed_movie(db: &TestDb, heat: i32) -> (i32, String) {
    let number = format!("DRG-{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: format!("{number} 标题"),
            ..NewMovie::default()
        })
        .await
        .expect("insert movie");
    set_heat(db, movie.id, heat).await;
    (movie.id, number)
}

async fn set_heat(db: &TestDb, movie_id: i32, heat: i32) {
    sqlx::query("UPDATE movie SET heat = $2 WHERE id = $1")
        .bind(movie_id)
        .bind(heat)
        .execute(db.pool())
        .await
        .expect("set heat");
}

async fn blacklist(db: &TestDb, movie_id: i32) {
    sqlx::query("UPDATE movie SET is_blacklisted = true WHERE id = $1")
        .bind(movie_id)
        .execute(db.pool())
        .await
        .expect("set is_blacklisted");
}

async fn mark_collection(db: &TestDb, movie_id: i32) {
    sqlx::query("UPDATE movie SET is_collection = true WHERE id = $1")
        .bind(movie_id)
        .execute(db.pool())
        .await
        .expect("set is_collection");
}

/// 造一位**已订阅**演员，返回 id。
async fn seed_subscribed_actor(db: &TestDb) -> i32 {
    let actor = ActorRepository::new(db.pool().clone())
        .insert(&NewActor {
            javdb_id: format!("actor-{}", n()),
            name: format!("演员{}", n()),
        })
        .await
        .expect("insert actor");
    sqlx::query("UPDATE actor SET is_subscribed = true WHERE id = $1")
        .bind(actor.id)
        .execute(db.pool())
        .await
        .expect("subscribe actor");
    actor.id
}

async fn link_actor(db: &TestDb, movie_id: i32, actor_id: i32) {
    MovieActorRepository::new(db.pool().clone())
        .link(movie_id, actor_id)
        .await
        .expect("link actor");
}

async fn rank_movie(db: &TestDb, movie_id: i32, number: &str, rank: i32, period: &str) {
    RankingItemRepository::new(db.pool().clone())
        .upsert(&NewRankingItem {
            source_key: "javdb".to_owned(),
            board_key: "trending".to_owned(),
            period: period.to_owned(),
            rank,
            movie_number: number.to_owned(),
            movie_id,
        })
        .await
        .expect("upsert ranking");
}

/// 造「最近播放」列表，成员按给定顺序（越靠后 = 越近播放）。
async fn seed_recently_played(db: &TestDb, movie_ids: &[i32]) {
    let playlist_id: i32 = sqlx::query_scalar(
        "INSERT INTO playlist (name, description, kind, created_at, updated_at) \
         VALUES ('最近播放', '', 'recently_played', now(), now()) RETURNING id",
    )
    .fetch_one(db.pool())
    .await
    .expect("insert playlist");

    for (index, movie_id) in movie_ids.iter().enumerate() {
        // `updated_at` 逐条递减，于是**最后一个**是「最近播放的那一部」。
        sqlx::query(
            "INSERT INTO playlist_movie (playlist_id, movie_id, created_at, updated_at) \
             VALUES ($1, $2, now(), now() - make_interval(secs => $3))",
        )
        .bind(playlist_id)
        .bind(movie_id)
        // 越靠后越近：`index` 增大 -> 减去的秒数变小。
        .bind((movie_ids.len() - index) as f64 * 60.0)
        .execute(db.pool())
        .await
        .expect("insert playlist_movie");
    }
}

/// 快照行的 `reason_codes` / `signal_scores`。
fn codes_of(row: &DailyRecommendationItem) -> Vec<String> {
    serde_json::from_str(row.reason_codes.as_deref().unwrap_or("[]")).expect("reason_codes 是 JSON")
}

fn signals_of(row: &DailyRecommendationItem) -> serde_json::Value {
    serde_json::from_str(row.signal_scores.as_deref().unwrap_or("{}"))
        .expect("signal_scores 是 JSON")
}

fn by_movie(rows: Vec<DailyRecommendationItem>) -> HashMap<i32, DailyRecommendationItem> {
    rows.into_iter().map(|row| (row.movie_id, row)).collect()
}

// ================================================================ 用例

/// 空库：没有候选就没有快照，且两条冷启动标记都为真。
///
/// 「无兴趣信号 且 无公共信号」= 极冷启动 —— 空库里两者都没有。
#[tokio::test]
async fn an_empty_catalog_produces_an_empty_snapshot() {
    let db = TestDb::require().await;

    let stats = generate(&db, DAILY_RECOMMENDATION_LIMIT).await;

    assert_eq!(stats.candidate_movies, 0);
    assert_eq!(stats.stored_items, 0);
    assert!(stats.cold_start, "没有兴趣信号");
    assert!(stats.extreme_cold_start, "也没有公共信号");
    assert!(snapshot_rows(&db).await.is_empty());
}

/// 候选过滤 + `limit` 截断 + `rank` 从 1 起 + 快照日期是**今天**（本地）。
#[tokio::test]
async fn candidates_exclude_blacklisted_and_collections_and_rank_starts_at_one() {
    let db = TestDb::require().await;
    let (_a, _) = seed_movie(&db, 10).await;
    let (_b, _) = seed_movie(&db, 20).await;
    let (_c, _) = seed_movie(&db, 30).await;
    // 这两部热度最高，若过滤失效它们一定会挤进前 2。
    let (hidden, _) = seed_movie(&db, 90_000).await;
    let (collection, _) = seed_movie(&db, 80_000).await;
    blacklist(&db, hidden).await;
    mark_collection(&db, collection).await;

    let stats = generate(&db, 2).await;

    assert_eq!(stats.candidate_movies, 3, "拉黑与集合片不进候选");
    assert_eq!(stats.stored_items, 2, "被 limit 截断");

    let rows = snapshot_rows(&db).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter().map(|row| row.rank).collect::<Vec<_>>(),
        vec![1, 2],
        "rank 从 1 连续编号"
    );
    let ids: Vec<i32> = rows.iter().map(|row| row.movie_id).collect();
    assert!(
        !ids.contains(&hidden) && !ids.contains(&collection),
        "拉黑 / 集合片不得出现在快照里：{ids:?}"
    );

    let today = chrono::Local::now().date_naive();
    assert_eq!(
        stats.snapshot_date,
        today.format("%Y-%m-%d").to_string(),
        "默认取本地今天，不是 UTC"
    );
    assert!(rows.iter().all(|row| row.snapshot_date == today));
}

/// 热度与榜单各自产出理由码与信号分。
///
/// 没有兴趣信号 -> 走**冷启动**制度（`COLD_START_WEIGHTS`），热度权重 11/18。
#[tokio::test]
async fn heat_and_ranking_produce_their_reason_codes() {
    let db = TestDb::require().await;
    let (hot, _) = seed_movie(&db, 9_000).await;
    let (ranked, number) = seed_movie(&db, 0).await;
    rank_movie(&db, ranked, &number, 1, "daily").await;

    let stats = generate(&db, DAILY_RECOMMENDATION_LIMIT).await;
    assert!(stats.cold_start, "没有兴趣信号，走冷启动");
    assert!(
        !stats.extreme_cold_start,
        "有公共信号（热度 / 榜单），不是极冷启动"
    );

    let rows = by_movie(snapshot_rows(&db).await);
    let hot_row = rows.get(&hot).expect("热度过高必入选");
    let hot_codes = codes_of(hot_row);
    assert!(
        hot_codes.iter().any(|code| code == "popular_movie"),
        "{hot_codes:?}"
    );
    let hot_signals = signals_of(hot_row);
    assert_eq!(
        hot_signals["heat"].as_f64().unwrap(),
        1.0,
        "唯一的正热度就是参考值（95 分位），归一后为 1.0"
    );

    let ranked_row = rows.get(&ranked).expect("在榜影片入选");
    let ranked_codes = codes_of(ranked_row);
    assert!(
        ranked_codes.iter().any(|code| code == "ranking_trending"),
        "{ranked_codes:?}"
    );
    let ranked_signals = signals_of(ranked_row);
    assert_eq!(
        ranked_signals["ranking"].as_f64().unwrap(),
        1.0,
        "daily 榜第 1 名：权重 1.0 × decay 1.0"
    );
    // 无兴趣信号 + 有新鲜度 -> 加「较新发布」。这是上游 `:315` 的条件，
    // 不是单纯「新鲜度 > 0」。
    assert!(
        ranked_codes.iter().any(|code| code == "new_release"),
        "无兴趣信号时应给 new_release：{ranked_codes:?}"
    );
}

/// 订阅演员会把「关联到该演员」的影片标出来 —— 并且它构成**兴趣信号**。
#[tokio::test]
async fn subscribed_actors_mark_their_movies_and_lift_cold_start() {
    let db = TestDb::require().await;
    let (liked, _) = seed_movie(&db, 0).await;
    let (other, _) = seed_movie(&db, 0).await;
    let actor = seed_subscribed_actor(&db).await;
    link_actor(&db, liked, actor).await;

    let stats = generate(&db, DAILY_RECOMMENDATION_LIMIT).await;
    assert!(!stats.cold_start, "订阅演员就是兴趣信号");

    let rows = by_movie(snapshot_rows(&db).await);
    let liked_row = rows.get(&liked).expect("关联订阅演员的影片应入选");
    assert_eq!(
        signals_of(liked_row)["subscribed_actor"].as_f64().unwrap(),
        1.0
    );
    assert!(codes_of(liked_row)
        .iter()
        .any(|code| code == "subscribed_actor"));

    let other_row = rows.get(&other).expect("另一部也应入选");
    assert_eq!(
        signals_of(other_row)["subscribed_actor"].as_f64().unwrap(),
        0.0,
        "没有关联订阅演员的影片不该被标"
    );
}

/// 重复生成 = **整体替换**。若退化成 upsert，第二次会撞 `rank` 全表唯一约束。
#[tokio::test]
async fn regeneration_replaces_the_whole_snapshot() {
    let db = TestDb::require().await;
    let (gone, _) = seed_movie(&db, 100).await;
    let (kept, _) = seed_movie(&db, 200).await;

    let first = generate(&db, DAILY_RECOMMENDATION_LIMIT).await;
    assert_eq!(first.stored_items, 2);

    // 把 `gone` 拉黑之后再生成一次。它必须**整行消失**，而不是与被覆盖的
    // 新批次混在一起。
    blacklist(&db, gone).await;
    let second = generate(&db, DAILY_RECOMMENDATION_LIMIT).await;
    assert_eq!(second.stored_items, 1);

    let rows = snapshot_rows(&db).await;
    assert_eq!(rows.len(), 1, "旧批不得残留");
    assert_eq!(rows[0].movie_id, kept);
    assert_eq!(rows[0].rank, 1, "rank 重新从 1 开始");
}

/// 「最近播放」是兴趣信号；Qdrant 缺席时相似度那一路**降级为 0** 且任务成功。
#[tokio::test]
async fn recent_playlist_seeds_are_counted_and_missing_similarity_is_tolerated() {
    let db = TestDb::require().await;
    let (a, _) = seed_movie(&db, 100).await;
    let (b, _) = seed_movie(&db, 50).await;
    seed_recently_played(&db, &[a, b]).await;

    let stats = generate(&db, DAILY_RECOMMENDATION_LIMIT).await;

    assert_eq!(stats.recent_seed_movies, 2, "两部首播都成了种子");
    assert!(!stats.cold_start, "最近播放就是兴趣信号");

    for row in snapshot_rows(&db).await {
        let signals = signals_of(&row);
        assert_eq!(
            signals["similarity"].as_f64().unwrap(),
            0.0,
            "Qdrant 缺席时相似度恒为 0"
        );
        assert!(
            !codes_of(&row)
                .iter()
                .any(|code| code == "similar_recent_play"),
            "没有相似度就不该有相似度理由码"
        );
    }
}
