//! 演员合并的**集成测试**：跑在真实 PostgreSQL 上。
//!
//! # 为什么必须连真库
//!
//! 合并的每一步都是一条手写 SQL，而 SQL 在 Rust 里只是一个 `String`。
//! 下面这些只会在把语句发给数据库时暴露：
//!
//! - `INSERT INTO movie_actor ... SELECT ... ON CONFLICT DO NOTHING` 的列序与
//!   唯一约束；
//! - `field_owners = field_owners || $n::jsonb` 的合并方向；
//! - 事务是否真的覆盖了六步（中途报错要能整体回滚）。
//!
//! # 覆盖点
//!
//! 成功路径（关联搬运 / 别名 / 订阅 / 填空 / 头像 / 墓碑压平）逐条断言，
//! 以及四条**失败与幂等**路径：合并到自身、来源已合并到别处、来源已在本目标
//! 下（幂等）、来源不存在。

use chrono::NaiveDate;
use serde_json::json;
use sm_db::catalog::actor::Actor;
use sm_db::repo::{ActorRepository, MovieActorRepository, MovieRepository, NewActor, NewMovie};
use sm_db::testing::TestDb;
use sm_service::catalog::actor::{ActorService, ActorView};
use sm_service::catalog::actor_merge::{ActorMergeService, INVALID_ACTOR_MERGE};
use sm_service::error::ServiceError;

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// `set_actor` 的赋值全部来自**字面量数组**，没有一条路径能把测试输入拼进去。
/// 门禁要求动态 SQL 显式声明「我审过了」，所以这里包一层。
fn unsafe_sql(sql: String) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(sql)
}

async fn seed_actor(db: &TestDb) -> i32 {
    ActorRepository::new(db.pool().clone())
        .insert(&NewActor {
            javdb_id: format!("AM-{:06}", n()),
            name: format!("演员{}", n()),
        })
        .await
        .expect("insert actor")
        .id
}

async fn seed_movie(db: &TestDb) -> i32 {
    MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: format!("AM-{:06}", n()),
            title: "影片".to_owned(),
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
        .expect("insert movie")
        .id
}

/// 逐列设置演员资料。列名与值都是**字面量**。
async fn set_actor(db: &TestDb, id: i32, assignments: &[(&str, &str)]) {
    let set = assignments
        .iter()
        .map(|(column, value)| format!("{column} = {value}"))
        .collect::<Vec<_>>()
        .join(", ");
    sqlx::query(unsafe_sql(format!("UPDATE actor SET {set} WHERE id = $1")))
        .bind(id)
        .execute(db.pool())
        .await
        .expect("update actor");
}

async fn actor_of(db: &TestDb, id: i32) -> Actor {
    ActorRepository::new(db.pool().clone())
        .find_by_id(id)
        .await
        .expect("query actor")
        .expect("actor 存在")
}

async fn link(db: &TestDb, movie_id: i32, actor_id: i32) {
    MovieActorRepository::new(db.pool().clone())
        .link(movie_id, actor_id)
        .await
        .expect("link movie_actor");
}

/// 某位演员名下已关联的影片 id（升序）。
async fn movie_ids_of(db: &TestDb, actor_id: i32) -> Vec<i32> {
    sqlx::query_scalar("SELECT movie_id FROM movie_actor WHERE actor_id = $1 ORDER BY movie_id")
        .bind(actor_id)
        .fetch_all(db.pool())
        .await
        .expect("query movie_actor")
}

async fn merge(db: &TestDb, target: i32, sources: &[i32]) -> Result<ActorView, ServiceError> {
    ActorMergeService::new(db.pool())
        .merge_actors(target, sources)
        .await
}

fn date(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("日期字面量")
}

// ================================================================ 成功路径

#[tokio::test]
async fn merge_moves_movie_links_and_leaves_tombstones() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let source_a = seed_actor(&db).await;
    let source_b = seed_actor(&db).await;

    let shared = seed_movie(&db).await;
    let only_a = seed_movie(&db).await;
    let only_b = seed_movie(&db).await;
    let only_target = seed_movie(&db).await;

    link(&db, shared, target).await;
    link(&db, shared, source_a).await;
    link(&db, only_a, source_a).await;
    link(&db, only_b, source_b).await;
    link(&db, only_target, target).await;

    let view = merge(&db, target, &[source_a, source_b])
        .await
        .expect("merge");
    assert_eq!(view.actor.id, target, "返回的是保留记录");

    let mut expected = vec![shared, only_a, only_b, only_target];
    expected.sort_unstable();
    assert_eq!(
        movie_ids_of(&db, target).await,
        expected,
        "目标名下应含全部去重后的影片"
    );
    assert!(
        movie_ids_of(&db, source_a).await.is_empty(),
        "来源关联应清空"
    );
    assert!(
        movie_ids_of(&db, source_b).await.is_empty(),
        "来源关联应清空"
    );
    assert_eq!(view.movie_count, 4);

    for source in [source_a, source_b] {
        let row = actor_of(&db, source).await;
        assert_eq!(row.merged_into_id, Some(target), "来源应成为墓碑");
        assert!(!row.is_subscribed);
        assert!(row.subscribed_at.is_none());
    }
}

#[tokio::test]
async fn merge_combines_aliases_and_takes_the_earliest_subscription() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let source = seed_actor(&db).await;

    set_actor(&db, target, &[("name", "'主名'"), ("alias_name", "''")]).await;
    set_actor(
        &db,
        source,
        &[
            ("name", "'来源名'"),
            ("alias_name", "'来源别名 / 共享'"),
            ("display_name_override", "'覆盖名'"),
            ("is_subscribed", "TRUE"),
            ("subscribed_at", "'2019-06-01 00:00:00'"),
        ],
    )
    .await;
    // 目标也订阅过，但更晚 —— 结果必须是**更早**的那个。
    set_actor(
        &db,
        target,
        &[
            ("is_subscribed", "TRUE"),
            ("subscribed_at", "'2020-01-01 00:00:00'"),
        ],
    )
    .await;

    merge(&db, target, &[source]).await.expect("merge");

    let row = actor_of(&db, target).await;
    assert_eq!(
        row.alias_name, "主名 / 来源名 / 来源别名 / 共享 / 覆盖名",
        "别名顺序：主名 -> 来源主名 -> 来源别名 -> 显示名覆盖 -> 既有别名"
    );
    assert!(row.is_subscribed);
    assert_eq!(
        row.subscribed_at.map(|t| t.date()),
        Some(date("2019-06-01")),
        "订阅时间取最早"
    );
}

#[tokio::test]
async fn merge_fills_only_empty_fields_and_skips_manual_owners() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let first = seed_actor(&db).await;
    let second = seed_actor(&db).await;

    // 目标：身高已填（不该被覆盖），罩杯 / 性别为空。
    set_actor(
        &db,
        target,
        &[("height_cm", "160"), ("cup", "NULL"), ("gender", "0")],
    )
    .await;
    // 第一个来源：身高 170（目标已填，跳过）、罩杯 D、性别 1 —— 但性别归人工。
    set_actor(
        &db,
        first,
        &[
            ("height_cm", "170"),
            ("cup", "'D'"),
            ("gender", "1"),
            (
                "field_owners",
                "'{\"gender\":\"host:manual\",\"cup\":\"host:javdb\"}'::jsonb",
            ),
        ],
    )
    .await;
    // 第二个来源：性别 2 —— 第一个来源的性别被人工归属挡住，轮到它来填。
    set_actor(&db, second, &[("gender", "2")]).await;

    merge(&db, target, &[first, second]).await.expect("merge");

    let row = actor_of(&db, target).await;
    assert_eq!(row.height_cm, Some(160), "目标非空，不该被覆盖");
    assert_eq!(row.cup.as_deref(), Some("D"), "第一个非空来源胜出");
    assert_eq!(row.gender, 2, "人工归属的性别被跳过，由下一个来源补上");
    assert_eq!(
        row.field_owners.get("cup").and_then(|v| v.as_str()),
        Some("host:javdb"),
        "来源的 owner 应被并进目标"
    );
    assert_eq!(row.mutation_revision, 1, "填了字段要推进版本");
}

#[tokio::test]
async fn merge_moves_the_profile_image_and_clears_it_on_the_source() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let source = seed_actor(&db).await;

    let image_id: i32 = sqlx::query_scalar("INSERT INTO image (origin) VALUES ($1) RETURNING id")
        .bind(format!("actors/manual-{}.webp", n()))
        .fetch_one(db.pool())
        .await
        .expect("insert image");
    set_actor(
        &db,
        source,
        &[("profile_image_override_id", &image_id.to_string())],
    )
    .await;

    merge(&db, target, &[source]).await.expect("merge");

    let target_row = actor_of(&db, target).await;
    assert_eq!(target_row.profile_image_override_id, Some(image_id));
    let source_row = actor_of(&db, source).await;
    assert_eq!(
        source_row.profile_image_override_id, None,
        "头像被搬走，来源行上那一列要清掉"
    );
}

#[tokio::test]
async fn merge_clears_full_sync_marker_when_subscribed() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let source = seed_actor(&db).await;
    set_actor(
        &db,
        target,
        &[("subscribed_movies_full_synced_at", "'2024-01-01 00:00:00'")],
    )
    .await;
    set_actor(
        &db,
        source,
        &[
            ("is_subscribed", "TRUE"),
            ("subscribed_at", "'2019-06-01 00:00:00'"),
        ],
    )
    .await;

    merge(&db, target, &[source]).await.expect("merge");

    let row = actor_of(&db, target).await;
    assert!(
        row.subscribed_movies_full_synced_at.is_none(),
        "合并后订阅应触发下一次全量同步"
    );
}

// ================================================================ 墓碑链压平

#[tokio::test]
async fn merge_repoints_existing_tombstones_to_the_new_target() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let source = seed_actor(&db).await;
    let old_tombstone = seed_actor(&db).await;

    // 先造一条指向 source 的旧墓碑：A -> source。
    ActorRepository::new(db.pool().clone())
        .mark_merged(&[old_tombstone], source)
        .await
        .expect("mark merged");

    merge(&db, target, &[source]).await.expect("merge");

    let row = actor_of(&db, old_tombstone).await;
    assert_eq!(
        row.merged_into_id,
        Some(target),
        "指向来源的旧墓碑应被重指向新目标（链压平）"
    );
}

#[tokio::test]
async fn the_target_tombstone_is_followed_one_hop() {
    let db = TestDb::require().await;
    let canonical = seed_actor(&db).await;
    let target_tombstone = seed_actor(&db).await;
    let source = seed_actor(&db).await;
    ActorRepository::new(db.pool().clone())
        .mark_merged(&[target_tombstone], canonical)
        .await
        .expect("mark merged");

    // 用**墓碑**作 target：应被解析到 canonical，并把 source 归并到 canonical。
    let view = merge(&db, target_tombstone, &[source])
        .await
        .expect("merge");
    assert_eq!(view.actor.id, canonical, "目标墓碑应跳一跳");

    assert_eq!(actor_of(&db, source).await.merged_into_id, Some(canonical));
}

// ================================================================ 失败与幂等

#[tokio::test]
async fn merging_into_self_is_422_merge_self() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;

    let err = merge(&db, target, &[target])
        .await
        .expect_err("合并到自身应失败");
    assert_eq!(err.status, 422);
    assert_eq!(err.code(), INVALID_ACTOR_MERGE);
    let details = err.api.details.as_ref().expect("有 details");
    assert_eq!(
        details.get("reason").and_then(|v| v.as_str()),
        Some("merge_self")
    );
    assert_eq!(
        details.get("actor_id").and_then(|v| v.as_i64()),
        Some(i64::from(target))
    );
}

#[tokio::test]
async fn a_source_already_merged_elsewhere_is_422() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let other = seed_actor(&db).await;
    let source = seed_actor(&db).await;
    ActorRepository::new(db.pool().clone())
        .mark_merged(&[source], other)
        .await
        .expect("mark merged");

    let err = merge(&db, target, &[source])
        .await
        .expect_err("已合并到别处应失败");
    assert_eq!(err.status, 422);
    assert_eq!(err.code(), INVALID_ACTOR_MERGE);
    assert_eq!(
        err.api
            .details
            .as_ref()
            .unwrap()
            .get("reason")
            .and_then(|v| v.as_str()),
        Some("source_already_merged")
    );
}

#[tokio::test]
async fn a_source_already_under_this_target_is_idempotent() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let source = seed_actor(&db).await;

    merge(&db, target, &[source]).await.expect("first merge");
    // 重放同一请求：来源已在本目标下，应**成功**而不是 422。
    let view = merge(&db, target, &[source])
        .await
        .expect("re-merge 应幂等成功");
    assert_eq!(view.actor.id, target);
}

#[tokio::test]
async fn a_missing_source_is_404_actor_not_found() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;

    let err = merge(&db, target, &[2_147_000_000])
        .await
        .expect_err("来源不存在应 404");
    assert_eq!(err.status, 404);
    assert_eq!(err.code(), "actor_not_found");
}

#[tokio::test]
async fn merge_rejects_a_missing_target_with_404() {
    let db = TestDb::require().await;
    let err = merge(&db, 2_147_000_000, &[2_147_000_001])
        .await
        .expect_err("目标不存在应 404");
    assert_eq!(err.status, 404);
    assert_eq!(err.code(), "actor_not_found");
}

// 一个只有 ActorService 能提供的旁证：合并后详情端点能看到新别名 ——
// 确保合并写的是**同一张表**，而不是某条旁路。
#[tokio::test]
async fn the_merged_detail_is_visible_through_the_read_service() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let source = seed_actor(&db).await;
    set_actor(&db, target, &[("name", "'甲'"), ("alias_name", "''")]).await;
    set_actor(&db, source, &[("name", "'乙'"), ("alias_name", "''")]).await;

    merge(&db, target, &[source]).await.expect("merge");
    let view = ActorService::new(db.pool())
        .detail(target)
        .await
        .expect("detail");
    assert_eq!(view.actor.alias_name, "甲 / 乙");
    assert_eq!(view.actor.field_owners, json!({}), "无 owner 时保持空对象");
}
