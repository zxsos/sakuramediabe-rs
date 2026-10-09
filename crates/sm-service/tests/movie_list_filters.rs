//! 影片列表筛选/排序查询的集成测试（**真实 PostgreSQL**，直接打仓储层）。
//!
//! # 为什么先测仓储层
//!
//! `GET /movies` 有 15 个可选筛选位，每个都带 0..n 个绑定值。用
//! `QueryBuilder` 拼 SQL 的失败方式是**静默的**：绑定值错位、某个 `AND` 少写、
//! 子查询条件写反 —— 都不会报错，只会「筛出来的东西不太对」。所以这一层要
//! 单独用真库钉住，而不是等 HTTP 层覆盖。
//!
//! 三个最容易写错的点在本文件里各有一条用例：标签的 OR / AND（分组去重计数）、
//! 演员筛选要走**合并链**、检索词的相关度排序。

use sm_db::repo::collection::SortDirection;
use sm_db::repo::movie::{MovieListFilter, MovieListSort};
use sm_db::repo::{MovieRepository, NewMovie};
use sm_db::testing::TestDb;

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

async fn seed_movie(db: &TestDb) -> (i32, String) {
    let number = format!("ML{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: format!("列表影片{number}"),
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

async fn seed_tag(db: &TestDb, name: &str) -> i32 {
    sqlx::query_scalar::<_, i32>("INSERT INTO tag (name) VALUES ($1) RETURNING id")
        .bind(name)
        .fetch_one(db.pool())
        .await
        .expect("insert tag")
}

async fn link_tag(db: &TestDb, movie_id: i32, tag_id: i32) {
    sqlx::query("INSERT INTO movie_tag (movie_id, tag_id) VALUES ($1, $2)")
        .bind(movie_id)
        .bind(tag_id)
        .execute(db.pool())
        .await
        .expect("link tag");
}

async fn seed_actor(db: &TestDb, name: &str) -> i32 {
    sqlx::query_scalar::<_, i32>("INSERT INTO actor (javdb_id, name) VALUES ($1, $2) RETURNING id")
        .bind(format!("ACT-{}", n()))
        .bind(name)
        .fetch_one(db.pool())
        .await
        .expect("insert actor")
}

async fn link_actor(db: &TestDb, movie_id: i32, actor_id: i32) {
    sqlx::query("INSERT INTO movie_actor (movie_id, actor_id) VALUES ($1, $2)")
        .bind(movie_id)
        .bind(actor_id)
        .execute(db.pool())
        .await
        .expect("link actor");
}

/// 跑一次筛选，返回命中的 id（默认按 `movie_number ASC`）。
async fn ids(db: &TestDb, filter: &MovieListFilter) -> Vec<i32> {
    ids_sorted(db, filter, None).await
}

async fn ids_sorted(
    db: &TestDb,
    filter: &MovieListFilter,
    sort: Option<(MovieListSort, SortDirection)>,
) -> Vec<i32> {
    MovieRepository::new(db.pool().clone())
        .list_movie_card_ids(filter, sort, 100, 0)
        .await
        .expect("list movies")
}

/// 总数与当页必须同一口径 —— 分开断言，免得只测了其中一个。
async fn total(db: &TestDb, filter: &MovieListFilter) -> i64 {
    MovieRepository::new(db.pool().clone())
        .count_movies(filter)
        .await
        .expect("count movies")
}

/// 标签：`OR` 命中任一，`AND` 须同时含全部。
#[tokio::test]
async fn tag_match_switches_between_or_and_and() {
    let db = TestDb::require().await;
    let (first_id, _) = seed_movie(&db).await;
    let (second_id, _) = seed_movie(&db).await;
    let (third_id, _) = seed_movie(&db).await;

    let tag_a = seed_tag(&db, &format!("标签A{}", n())).await;
    let tag_b = seed_tag(&db, &format!("标签B{}", n())).await;
    link_tag(&db, first_id, tag_a).await;
    link_tag(&db, second_id, tag_a).await;
    link_tag(&db, second_id, tag_b).await;
    link_tag(&db, third_id, tag_b).await;

    let mut or_filter = MovieListFilter {
        tag_ids: vec![tag_a, tag_b],
        ..Default::default()
    };
    or_filter.tag_match_all = false;
    let mut found = ids(&db, &or_filter).await;
    found.sort_unstable();
    assert_eq!(
        found,
        vec![first_id, second_id, third_id],
        "OR 应当命中任一标签"
    );
    assert_eq!(total(&db, &or_filter).await, 3, "total 与当页同口径");

    let and_filter = MovieListFilter {
        tag_ids: vec![tag_a, tag_b],
        tag_match_all: true,
        ..Default::default()
    };
    assert_eq!(
        ids(&db, &and_filter).await,
        vec![second_id],
        "AND 只该命中同时带两个标签的那部"
    );
    assert_eq!(total(&db, &and_filter).await, 1);
}

/// 演员筛选必须走 `COALESCE(merged_into_id, id)`。
///
/// # 合并后的数据形状
///
/// `POST /actors/{id}/merge` 会把 `movie_actor` 的关联**搬到存活方**，再给
/// 来源打墓碑（`merged_into_id`）。所以合并之后：
///
/// - 影片挂在**存活**演员名下；
/// - 被合并的那个 id 仍然会被客户端拿去筛（用户点的是旧的 URL / 书签）。
///
/// 这条链要做的正是「把旧 id 解析成存活 id」—— 少了它，点开旧演员会得到空列表。
#[tokio::test]
async fn the_actor_filter_follows_the_merge_chain() {
    let db = TestDb::require().await;
    let (movie_id, _) = seed_movie(&db).await;
    let survivor = seed_actor(&db, &format!("存活的演员{}", n())).await;
    let merged = seed_actor(&db, &format!("被合并的演员{}", n())).await;
    // 合并后的形状：关联在存活方。
    link_actor(&db, movie_id, survivor).await;
    sqlx::query("UPDATE actor SET merged_into_id = $2 WHERE id = $1")
        .bind(merged)
        .bind(survivor)
        .execute(db.pool())
        .await
        .expect("merge actor");

    // 用**已被合并**的 id 筛：必须解析到存活 id 才找得到。
    let by_merged = MovieListFilter {
        actor_id: Some(merged),
        ..Default::default()
    };
    assert_eq!(
        ids(&db, &by_merged).await,
        vec![movie_id],
        "旧演员 id 应当被解析到存活演员"
    );

    let by_survivor = MovieListFilter {
        actor_id: Some(survivor),
        ..Default::default()
    };
    assert_eq!(ids(&db, &by_survivor).await, vec![movie_id]);
}

/// 检索词：命中番号的排在前（相关度分档），且默认排序**不带**显式 `sort`。
#[tokio::test]
async fn a_number_search_ranks_the_number_match_first() {
    let db = TestDb::require().await;

    // 两条都含检索串 `ML100000`：一条是**番号**精确命中（相关度最高），
    // 一条只是**标题**里出现（相关度更低）。
    let (exact_id, exact_number) = seed_movie(&db).await;
    let (title_id, _) = seed_movie(&db).await;
    sqlx::query("UPDATE movie SET movie_number = $2 WHERE id = $1")
        .bind(exact_id)
        .bind("ML100000")
        .execute(db.pool())
        .await
        .expect("改番号");
    sqlx::query("UPDATE movie SET title = $2 WHERE id = $1")
        .bind(title_id)
        .bind("ML100000 标题里出现的那个")
        .execute(db.pool())
        .await
        .expect("改标题");

    let filter = MovieListFilter {
        search_terms: vec!["ML100000".to_owned()],
        ..Default::default()
    };

    let found = ids(&db, &filter).await;
    assert_eq!(
        found.first(),
        Some(&exact_id),
        "番号精确命中的应当排第一（{exact_number}）：{found:?}"
    );
    assert!(found.contains(&title_id), "标题命中的也该出现：{found:?}");
    assert_eq!(total(&db, &filter).await, found.len() as i64);
}

/// 排序：显式 `heat:desc` 生效，缺省按 `movie_number ASC`。
#[tokio::test]
async fn sort_by_heat_desc_and_the_default_order_by_number() {
    let db = TestDb::require().await;
    let (low_id, low_number) = seed_movie(&db).await;
    let (high_id, high_number) = seed_movie(&db).await;
    sqlx::query("UPDATE movie SET heat = 5 WHERE id = $1")
        .bind(low_id)
        .execute(db.pool())
        .await
        .expect("set heat");
    sqlx::query("UPDATE movie SET heat = 50 WHERE id = $1")
        .bind(high_id)
        .execute(db.pool())
        .await
        .expect("set heat");

    let filter = MovieListFilter::default();
    let by_heat = ids_sorted(
        &db,
        &filter,
        Some((MovieListSort::Heat, SortDirection::Desc)),
    )
    .await;
    assert_eq!(by_heat.first(), Some(&high_id), "热度高的在前");

    let by_default = ids(&db, &filter).await;
    // 缺省是 `movie_number ASC`：本用例的两条番号是 `MLxxxxxx`，字典序即数字序。
    let expected_first = if low_number < high_number {
        low_id
    } else {
        high_id
    };
    assert_eq!(by_default.first(), Some(&expected_first), "缺省按番号升序");
}

/// 三态筛选：`subscribed` 与 `single_only`、黑名单默认值各管各的。
#[tokio::test]
async fn the_tristate_and_boolean_filters_compose() {
    let db = TestDb::require().await;
    let (subscribed_id, _) = seed_movie(&db).await;
    let (plain_id, _) = seed_movie(&db).await;
    let (blacklisted_id, _) = seed_movie(&db).await;
    sqlx::query("UPDATE movie SET is_subscribed = TRUE, subscribed_at = NOW() WHERE id = $1")
        .bind(subscribed_id)
        .execute(db.pool())
        .await
        .expect("set subscribed");
    sqlx::query("UPDATE movie SET is_collection = TRUE WHERE id = $1")
        .bind(plain_id)
        .execute(db.pool())
        .await
        .expect("set collection");
    sqlx::query("UPDATE movie SET is_blacklisted = TRUE WHERE id = $1")
        .bind(blacklisted_id)
        .execute(db.pool())
        .await
        .expect("set blacklisted");

    // 默认只要不在黑名单的。
    let all = ids(&db, &MovieListFilter::default()).await;
    assert!(all.contains(&subscribed_id) && all.contains(&plain_id));
    assert!(!all.contains(&blacklisted_id), "默认排除黑名单");

    // 只要黑名单里的。
    let only_black = MovieListFilter {
        blacklisted: true,
        ..Default::default()
    };
    assert_eq!(ids(&db, &only_black).await, vec![blacklisted_id]);

    // 已订阅 + 只要单片：合集那部被排除。
    let subscribed_singles = MovieListFilter {
        subscribed: Some(true),
        single_only: true,
        ..Default::default()
    };
    assert_eq!(ids(&db, &subscribed_singles).await, vec![subscribed_id]);
}

// ================================================================ 订阅台账查询

/// 造一部影片并直接设定检索状态字段，返回 id。
async fn seed_subscribed(db: &TestDb, number: &str, state: &str) -> i32 {
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.to_owned(),
            title: format!("订阅-{number}"),
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
    sqlx::query(
        "UPDATE movie SET is_subscribed = TRUE, subscribed_at = NOW(), \
         subscription_search_state = $2 WHERE id = $1",
    )
    .bind(movie.id)
    .bind(state)
    .execute(db.pool())
    .await
    .expect("设定订阅态");
    movie.id
}

const BY_SUBSCRIBED_AT: &str = "m.subscribed_at DESC NULLS LAST, m.id DESC NULLS LAST";

#[tokio::test]
async fn the_subscription_queries_filter_count_and_aggregate() {
    let db = TestDb::require().await;

    let pending = seed_subscribed(&db, &format!("SUBP{:06}", n()), "pending").await;
    let exhausted = seed_subscribed(&db, &format!("SUBE{:06}", n()), "exhausted").await;
    // 未订阅的影片不该出现在任何订阅查询里。
    let outsider = seed_movie(&db).await;
    let _ = outsider;

    // ① 总数 = 已订阅的两部。
    assert_eq!(
        MovieRepository::new(db.pool().clone())
            .count_subscriptions(None, None)
            .await
            .unwrap(),
        2
    );

    // ② 按状态筛：`pending` 一部。
    assert_eq!(
        MovieRepository::new(db.pool().clone())
            .list_subscription_ids(Some("pending"), None, BY_SUBSCRIBED_AT, 100, 0)
            .await
            .unwrap(),
        vec![(pending, "pending".to_owned())]
    );
    assert_eq!(
        MovieRepository::new(db.pool().clone())
            .count_subscriptions(Some("exhausted"), None)
            .await
            .unwrap(),
        1
    );

    // ③ 检索词：番号与片名任一命中（这里是片名）。
    let by_search = MovieRepository::new(db.pool().clone())
        .list_subscription_ids(None, Some("SUBE"), BY_SUBSCRIBED_AT, 100, 0)
        .await
        .unwrap();
    assert_eq!(by_search, vec![(exhausted, "exhausted".to_owned())]);
    // 空白检索词 = 不筛（不是「查不到」）。
    assert_eq!(
        MovieRepository::new(db.pool().clone())
            .count_subscriptions(None, Some("   "))
            .await
            .unwrap(),
        2
    );

    // ④ 三条聚合查询（没数据时空 map，而不是报错）。
    let empty: Vec<String> = Vec::new();
    let movies = MovieRepository::new(db.pool().clone());
    assert!(movies
        .count_media_by_numbers(&empty)
        .await
        .unwrap()
        .is_empty());
    assert!(movies
        .count_failed_tasks_by_numbers(&empty)
        .await
        .unwrap()
        .is_empty());
    assert!(movies
        .latest_import_status_by_numbers(&empty)
        .await
        .unwrap()
        .is_empty());
}
