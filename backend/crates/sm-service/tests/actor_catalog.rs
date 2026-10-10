//! 演员目录的**集成测试**：跑在真实 PostgreSQL 上。
//!
//! 与 `sm-service` 里的单测分工：那边测纯逻辑（偏移夹取、排序表达式解析、
//! 请求体校验），这边测**SQL 形状** —— 而 SQL 在 Rust 里只是一个 `String`，
//! 类型检查抓不到任何东西。
//!
//! # 这批测试为什么必须连真库
//!
//! 下面每一条都曾以「看起来对」的形式通过 `cargo check` + `clippy` + 单测：
//!
//! - `media.movie` 写成 `media.movie_id` —— `media` 指向 `movie.movie_number`
//!   （字符串主键），DDL 上没有 `movie_id` 这一列；
//! - `NULLIF(UPPER(BTRIM(cup)), '') = ANY($n)` 少写 `::text[]` —— PG 无法从
//!   `= ANY($n)` 两侧推出参数类型，报 `could not determine data type`；
//! - `waist_cm::REAL / NULLIF(hips_cm, 0)` 少写 `::REAL` —— PostgreSQL 的
//!   `integer / integer` 是**整数除法**，35/90 会算成 0，而它是排序键；
//! - `SELECT a.*, 额外列` 配元组返回 —— sqlx 的元组 `FromRow` 按**列序**
//!   解码，不是按列名。
//!
//! 共同点：编译期完全合法，只有把语句发给数据库才会暴露。

use chrono::NaiveDate;
use serde_json::{json, Map};
use sm_db::catalog::actor::{GENDER_FEMALE, GENDER_MALE};
use sm_db::repo::{
    ActorRepository, MediaLibraryRepository, MediaRepository, MovieActorRepository,
    MovieRepository, MovieTagRepository, NewActor, NewMedia, NewMediaLibrary, NewMovie,
    TagRepository,
};
use sm_db::testing::TestDb;
use sm_service::catalog::actor::{ActorListParams, ActorService, INVALID_ACTOR_FILTER};

/// 测试里拼的 SQL 全部来自**字面量数组**（`set_profile` 的调用点全部是
/// `("cup", "'a'")` 这种），没有一条路径能把测试输入拼进去。
/// 门禁要求动态 SQL 显式声明「我审过了」，所以这里包一层。
fn unsafe_sql(sql: String) -> sqlx::AssertSqlSafe<String> {
    sqlx::AssertSqlSafe(sql)
}

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 建一位演员。字段留空，由各测试按需 `UPDATE`。
async fn seed_actor(db: &TestDb) -> i32 {
    ActorRepository::new(db.pool().clone())
        .insert(&NewActor {
            javdb_id: format!("ACT-{:06}", n()),
            name: format!("演员{}", n()),
        })
        .await
        .expect("insert actor")
        .id
}

/// 建一部影片，返回 `(id, movie_number)`。
async fn seed_movie(db: &TestDb, release_date: Option<NaiveDate>) -> (i32, String) {
    let number = format!("ACT-{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
            title: "影片".to_owned(),
            javdb_id: None,
            summary: String::new(),
            maker_name: None,
            director_name: None,
            release_date: release_date.map(|d| d.and_hms_opt(0, 0, 0).unwrap()),
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

/// 建一个媒体库。媒体必须挂在某个库下。
async fn seed_library(db: &TestDb) -> i32 {
    MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("act-lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id
}

/// 给影片挂一条 `valid = true` 的媒体 —— 「可播放」的判定依赖它。
async fn seed_playable_media(db: &TestDb, movie_number: &str) {
    let library_id = seed_library(db).await;
    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("m-{}.mp4", n()),
            file_size_bytes: 1,
            movie_number: Some(movie_number.to_owned()),
            video_item_id: None,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 1,
            video_info: None,
        })
        .await
        .expect("insert media");
}

/// 逐列设置演员资料。
async fn set_profile(db: &TestDb, id: i32, assignments: &[(&str, &str)]) {
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

/// 默认参数。
///
/// `page_size` 给 10_000 而不是 20：集成测试**并行**跑、共享同一个库，
/// 别的用例造的演员也在表里。默认 20 的话「刚建的演员应当出现」这类断言
/// 会因为**别的用例塞满了第一页**而失败 —— 一个与被测代码无关的偶发失败，
/// 比没有断言更糟。要测分页的地方显式把 `page_size` 改小。
fn params() -> ActorListParams {
    ActorListParams {
        page: 1,
        page_size: 10_000,
        ..ActorListParams::default()
    }
}

fn date(text: &str) -> NaiveDate {
    NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("日期字面量")
}

// ================================================================ 列表：基线

#[tokio::test]
async fn an_empty_table_lists_nothing() {
    let db = TestDb::require().await;
    let page = ActorService::new(db.pool())
        .list(&params())
        .await
        .expect("list");
    assert_eq!(page.items.len(), 0);
    assert_eq!(page.total, 0);
}

#[tokio::test]
async fn a_fresh_actor_appears_with_zero_movies() {
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;

    let page = ActorService::new(db.pool())
        .list(&params())
        .await
        .expect("list");
    let item = page
        .items
        .iter()
        .find(|item| item.actor.id == id)
        .expect("刚建的演员应当出现");
    assert_eq!(item.movie_count, 0, "没有关联影片时计数是 0");
    assert_eq!(item.age, None, "没有生日时年龄是 null");
    assert!(item.image_id.is_none(), "没有头像");
}

#[tokio::test]
async fn movie_count_is_computed_from_the_link_table() {
    let db = TestDb::require().await;
    let actor_id = seed_actor(&db).await;
    let links = MovieActorRepository::new(db.pool().clone());
    for _ in 0..3 {
        let (movie_id, _) = seed_movie(&db, None).await;
        links.link(movie_id, actor_id).await.expect("link");
    }

    let page = ActorService::new(db.pool())
        .list(&params())
        .await
        .expect("list");
    let item = page
        .items
        .iter()
        .find(|item| item.actor.id == actor_id)
        .expect("演员应当出现");
    assert_eq!(item.movie_count, 3, "影片数按 movie_actor 实时数");
}

// ================================================================ 列表：筛选

#[tokio::test]
async fn gender_filter_selects_exactly_one_of_the_two() {
    let db = TestDb::require().await;
    let female = seed_actor(&db).await;
    let male = seed_actor(&db).await;
    set_profile(&db, female, &[("gender", "1")]).await;
    set_profile(&db, male, &[("gender", "2")]).await;

    let mut p = params();
    p.gender = Some(GENDER_FEMALE);
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert!(
        page.items.iter().any(|i| i.actor.id == female),
        "女演员应当在结果里"
    );
    assert!(
        !page.items.iter().any(|i| i.actor.id == male),
        "男演员不该出现在 gender=1 的结果里"
    );

    p.gender = Some(GENDER_MALE);
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert!(page.items.iter().any(|i| i.actor.id == male));
    assert!(!page.items.iter().any(|i| i.actor.id == female));
}

#[tokio::test]
async fn subscription_filter_splits_both_ways() {
    let db = TestDb::require().await;
    let subscribed = seed_actor(&db).await;
    let plain = seed_actor(&db).await;
    ActorRepository::new(db.pool().clone())
        .set_subscribed(subscribed, true)
        .await
        .expect("subscribe");

    let mut p = params();
    p.subscribed = Some(true);
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert!(page.items.iter().any(|i| i.actor.id == subscribed));
    assert!(!page.items.iter().any(|i| i.actor.id == plain));

    p.subscribed = Some(false);
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert!(
        !page.items.iter().any(|i| i.actor.id == subscribed),
        "已订阅不该出现在未订阅结果里"
    );
    assert!(page.items.iter().any(|i| i.actor.id == plain));
}

#[tokio::test]
async fn cup_filter_matches_the_normalized_form() {
    // 库里存的是带空白、小写的 " a "，筛选项给的是大写 "A"。
    // 上游是 `NULLIF(UPPER(BTRIM(cup)), '') IN (...)`，两端都要归一。
    let db = TestDb::require().await;
    let messy = seed_actor(&db).await;
    let other = seed_actor(&db).await;
    set_profile(&db, messy, &[("cup", "' a '")]).await;
    set_profile(&db, other, &[("cup", "'B'")]).await;

    let mut p = params();
    p.cups = vec!["A".to_owned()];
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert!(
        page.items.iter().any(|i| i.actor.id == messy),
        "归一后应当命中 ' a '"
    );
    assert!(
        !page.items.iter().any(|i| i.actor.id == other),
        "罩杯不同的不该命中"
    );
}

#[tokio::test]
async fn height_range_is_inclusive_on_both_ends() {
    let db = TestDb::require().await;
    let short = seed_actor(&db).await;
    let tall = seed_actor(&db).await;
    set_profile(&db, short, &[("height_cm", "150")]).await;
    set_profile(&db, tall, &[("height_cm", "180")]).await;

    let mut p = params();
    p.height_min = Some(150);
    p.height_max = Some(180);
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert!(page.items.iter().any(|i| i.actor.id == short));
    assert!(page.items.iter().any(|i| i.actor.id == tall));

    p.height_min = Some(160);
    p.height_max = Some(179);
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert!(!page.items.iter().any(|i| i.actor.id == short));
    assert!(!page.items.iter().any(|i| i.actor.id == tall));
}

#[tokio::test]
async fn age_bounds_take_the_lower_edge_inclusive_and_the_upper_edge_exclusive() {
    // 上游把年龄区间翻译成两条生日比较：
    //   age >= age_min  <=>  birthday <= years_before(today, age_min)
    //   age <= age_max  <=>  birthday >  years_before(today, age_max + 1)
    // 上界靠 +1 变成开区间，所以 age_min = age_max 是一个**单点**而不是空区间。
    //
    // 基准日 2026-10-04，三位演员：
    //   1996-10-04 -> 30 周岁（生日当天算满岁）
    //   1995-10-04 -> 31 周岁
    //   1997-10-05 -> 29 周岁（生日未到 10/4 减一）
    let today = date("2026-10-04");
    let db = TestDb::require().await;
    let age_30 = seed_actor(&db).await;
    let age_31 = seed_actor(&db).await;
    let age_29 = seed_actor(&db).await;
    set_profile(&db, age_30, &[("birthday", "'1996-10-04'")]).await;
    set_profile(&db, age_31, &[("birthday", "'1995-10-04'")]).await;
    set_profile(&db, age_29, &[("birthday", "'1997-10-05'")]).await;

    let ids_of = |page: &sm_service::catalog::actor::ActorPage| -> Vec<i32> {
        page.items.iter().map(|i| i.actor.id).collect()
    };

    // 单点区间：只有恰好 30 岁那位
    let mut p = params();
    p.age_min = Some(30);
    p.age_max = Some(30);
    let ids = ids_of(
        &ActorService::new(db.pool())
            .list_on(&p, today)
            .await
            .expect("list"),
    );
    assert!(ids.contains(&age_30), "恰好 30 岁必须命中下界");
    assert!(!ids.contains(&age_31), "31 岁超出上界");
    assert!(!ids.contains(&age_29), "29 岁低于下界");

    // 只有下界：30 与 31 都命中
    p.age_max = None;
    let ids = ids_of(
        &ActorService::new(db.pool())
            .list_on(&p, today)
            .await
            .expect("list"),
    );
    assert!(ids.contains(&age_30));
    assert!(
        ids.contains(&age_31),
        "age_min=30 是「>= 30」，31 岁应当命中"
    );

    // 只有上界：29 与 30 都命中，31 不命中
    p.age_min = None;
    p.age_max = Some(30);
    let ids = ids_of(
        &ActorService::new(db.pool())
            .list_on(&p, today)
            .await
            .expect("list"),
    );
    assert!(ids.contains(&age_30), "30 岁命中上界");
    assert!(!ids.contains(&age_31), "age_max=30 是「<= 30」");
    assert!(ids.contains(&age_29));
}

#[tokio::test]
async fn age_of_a_leap_day_birthday_is_computed_against_today() {
    // 2 月 29 日出生的人在 3 月 1 日之后已经过完了当年的生日。
    let today = date("2026-10-04");
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    set_profile(&db, id, &[("birthday", "'2000-02-29'")]).await;

    let page = ActorService::new(db.pool())
        .list_on(&params(), today)
        .await
        .expect("list");
    let item = page.items.iter().find(|i| i.actor.id == id).expect("演员");
    assert_eq!(item.age, Some(26), "2000-02-29 到 2026-10-04 是 26 周岁");
}

#[tokio::test]
async fn has_playable_movies_follows_media_valid() {
    // 这一条验的是 `media.movie_number`（字符串外键）而不是 `media.movie_id`，
    // 以及 `media.valid` 真的参与了判定。
    let db = TestDb::require().await;
    let with_playable = seed_actor(&db).await;
    let with_dead = seed_actor(&db).await;
    let links = MovieActorRepository::new(db.pool().clone());

    let (movie_id, number) = seed_movie(&db, None).await;
    links.link(movie_id, with_playable).await.expect("link");
    seed_playable_media(&db, &number).await;

    let (dead_movie_id, dead_number) = seed_movie(&db, None).await;
    links.link(dead_movie_id, with_dead).await.expect("link");
    seed_playable_media(&db, &dead_number).await;
    // 把那条媒体判死 —— 只挂 media 行但 valid=false 不算「可播放」
    sqlx::query("UPDATE media SET valid = FALSE WHERE movie_number = $1")
        .bind(&dead_number)
        .execute(db.pool())
        .await
        .expect("kill media");

    let mut p = params();
    p.has_playable_movies = true;
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert!(
        page.items.iter().any(|i| i.actor.id == with_playable),
        "有 valid 媒体关联的演员应当命中"
    );
    assert!(
        !page.items.iter().any(|i| i.actor.id == with_dead),
        "只有 valid=false 媒体的演员不该命中"
    );
}

#[tokio::test]
async fn search_terms_are_anded_and_match_aliases_too() {
    let db = TestDb::require().await;
    let by_name = seed_actor(&db).await;
    let by_alias = seed_actor(&db).await;
    let neither = seed_actor(&db).await;
    sqlx::query("UPDATE actor SET name = $1, alias_name = $2 WHERE id = $3")
        .bind("小泽")
        .bind("小泽 / 沢さん")
        .bind(by_alias)
        .execute(db.pool())
        .await
        .expect("设置别名");
    assert_ne!(by_name, by_alias);

    let mut p = params();
    p.query = Some("小泽".to_owned());
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    let ids: Vec<i32> = page.items.iter().map(|i| i.actor.id).collect();
    assert!(ids.contains(&by_alias), "别名里命中也算");
    assert!(!ids.contains(&neither), "不含该词的演员不该命中");

    // 两个词之间是 AND：加上一个命中不到的条件就应当整体落空
    p.query = Some("小泽 不存在的词".to_owned());
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert!(
        !page.items.iter().any(|i| i.actor.id == by_alias),
        "词之间是 AND，多一个词就应当落空"
    );
    let _ = by_name;
}

#[tokio::test]
async fn tombstones_never_show_up_in_the_list() {
    let db = TestDb::require().await;
    let alive = seed_actor(&db).await;
    let tombstone = seed_actor(&db).await;
    ActorRepository::new(db.pool().clone())
        .mark_merged(&[tombstone], alive)
        .await
        .expect("mark merged");

    let page = ActorService::new(db.pool())
        .list(&params())
        .await
        .expect("list");
    let ids: Vec<i32> = page.items.iter().map(|i| i.actor.id).collect();
    assert!(ids.contains(&alive));
    assert!(
        !ids.contains(&tombstone),
        "墓碑不是独立实体，不该出现在列表里"
    );
}

// ================================================================ 列表：排序

#[tokio::test]
async fn age_sort_uses_the_birthday_in_the_opposite_direction() {
    // 「年龄升序」= 年龄小 = 生日**新**。漏掉取反的话结果是全反的，
    // 而且每一行看着都合理 —— 只是顺序错了。
    let db = TestDb::require().await;
    let older = seed_actor(&db).await;
    let younger = seed_actor(&db).await;
    set_profile(&db, older, &[("birthday", "'1980-01-01'")]).await;
    set_profile(&db, younger, &[("birthday", "'2000-01-01'")]).await;

    let mut p = params();
    p.sort = Some("age:asc".to_owned());
    let page = ActorService::new(db.pool())
        .list_on(&p, date("2026-10-04"))
        .await
        .expect("list");
    let ids: Vec<i32> = page.items.iter().map(|i| i.actor.id).collect();
    let pos_older = ids.iter().position(|id| *id == older).expect("年长的演员");
    let pos_younger = ids
        .iter()
        .position(|id| *id == younger)
        .expect("年少的演员");
    assert!(
        pos_younger < pos_older,
        "age:asc 应当把年龄小的排前面（生日新的在前）"
    );

    p.sort = Some("age:desc".to_owned());
    let page = ActorService::new(db.pool())
        .list_on(&p, date("2026-10-04"))
        .await
        .expect("list");
    let ids: Vec<i32> = page.items.iter().map(|i| i.actor.id).collect();
    assert!(
        ids.iter().position(|id| *id == older) < ids.iter().position(|id| *id == younger),
        "age:desc 应当反过来"
    );
}

#[tokio::test]
async fn name_sort_puts_nulls_last_only_for_nullable_keys() {
    // `subscribed_at` 在可空排序键集合里，空值必须垫后。
    let db = TestDb::require().await;
    let with_time = seed_actor(&db).await;
    let without = seed_actor(&db).await;
    ActorRepository::new(db.pool().clone())
        .set_subscribed(with_time, true)
        .await
        .expect("subscribe");

    let mut p = params();
    p.sort = Some("subscribed_at:desc".to_owned());
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    let ids: Vec<i32> = page.items.iter().map(|i| i.actor.id).collect();
    assert!(
        ids.iter().position(|id| *id == without) > ids.iter().position(|id| *id == with_time),
        "空订阅时间应排在有订阅时间的后面（NULLS LAST）"
    );
}

#[tokio::test]
async fn search_relevance_ranks_the_exact_name_first() {
    let db = TestDb::require().await;
    let exact = seed_actor(&db).await;
    let prefixed = seed_actor(&db).await;
    let aliased = seed_actor(&db).await;
    set_profile(&db, exact, &[("name", "'小泽'")]).await;
    set_profile(&db, prefixed, &[("name", "'小泽遥'")]).await;
    set_profile(&db, aliased, &[("name", "'泽'"), ("alias_name", "'小泽'")]).await;

    let mut p = params();
    p.query = Some("小泽".to_owned());
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    let ids: Vec<i32> = page.items.iter().map(|i| i.actor.id).collect();
    let pos = |id: i32| ids.iter().position(|x| *x == id).expect("演员应当在列表里");
    assert!(
        pos(exact) < pos(prefixed),
        "完全同名（0 分）应当排在姓名前缀（1 分）之前"
    );
    assert!(
        pos(prefixed) < pos(aliased),
        "姓名前缀（1 分）应当排在别名命中（3 分）之前"
    );
}

#[tokio::test]
async fn an_explicit_sort_beats_the_search_relevance() {
    let db = TestDb::require().await;
    let exact = seed_actor(&db).await;
    let prefixed = seed_actor(&db).await;
    set_profile(&db, exact, &[("name", "'小泽'")]).await;
    set_profile(&db, prefixed, &[("name", "'小泽遥'")]).await;

    let mut p = params();
    p.query = Some("小泽".to_owned());
    // 显式给出排序时以排序为准：name 降序 = 「小泽遥」在前（「小泽」是它的前缀）
    p.sort = Some("name:desc".to_owned());
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    let ids: Vec<i32> = page.items.iter().map(|i| i.actor.id).collect();
    assert!(
        ids.iter().position(|id| *id == prefixed) < ids.iter().position(|id| *id == exact),
        "给了排序就不该再按相关度"
    );
}

#[tokio::test]
async fn an_unknown_sort_key_is_rejected_before_touching_the_database() {
    let db = TestDb::require().await;
    let mut p = params();
    p.sort = Some("nope:asc".to_owned());
    let err = ActorService::new(db.pool())
        .list(&p)
        .await
        .expect_err("非法排序键应被拒");
    assert_eq!(err.status, 422);
    assert_eq!(err.code(), INVALID_ACTOR_FILTER);
}

// ================================================================ 列表：分页

#[tokio::test]
async fn total_counts_the_filtered_set_and_pages_do_not_overlap() {
    let db = TestDb::require().await;
    for _ in 0..5 {
        seed_actor(&db).await;
    }

    let mut p = params();
    p.page_size = 2;
    p.sort = Some("name:asc".to_owned());
    let first = ActorService::new(db.pool()).list(&p).await.expect("list");
    // 恰好 5 个演员、无筛选，所以 total 必须**精确**是 5。
    // 写成 `>= 5` 会放过「total 算错集合」这类 bug —— 那正是这个用例要防的。
    assert_eq!(first.total, 5, "total 是过滤后的总数，不是本页条数");
    assert_eq!(first.items.len(), 2, "本页应当恰好取 page_size 条");
    assert_eq!(first.page, 1);
    assert_eq!(first.page_size, 2);

    p.page = 2;
    let second = ActorService::new(db.pool()).list(&p).await.expect("list");
    let overlap: Vec<i32> = first
        .items
        .iter()
        .map(|i| i.actor.id)
        .filter(|id| second.items.iter().any(|i| i.actor.id == *id))
        .collect();
    assert!(overlap.is_empty(), "两页不该重叠，实际重叠 {overlap:?}");
}

#[tokio::test]
async fn a_non_positive_page_is_clamped_to_the_first_one() {
    // 上游 `start = max(page - 1, 0) * page_size`，而 page 原样回显。
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let mut p = params();
    p.page = 0;
    p.sort = Some("name:asc".to_owned());
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert_eq!(page.page, 0, "page 原样回显，不夹取");
    assert!(
        page.items.iter().any(|i| i.actor.id == id),
        "page=0 应当返回第一页数据"
    );
}

#[tokio::test]
async fn a_page_size_above_one_hundred_is_honoured() {
    // 刻意不校验：上游允许 page_size=500，而 validate_page 会给 422。
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let mut p = params();
    p.page_size = 500;
    let page = ActorService::new(db.pool()).list(&p).await.expect("list");
    assert_eq!(page.page_size, 500);
    assert!(page.items.iter().any(|i| i.actor.id == id));
}

// ================================================================ 筛选项聚合

#[tokio::test]
async fn filter_options_turn_birthdays_into_an_age_range_with_the_right_direction() {
    let db = TestDb::require().await;
    let young = seed_actor(&db).await;
    let old = seed_actor(&db).await;
    set_profile(&db, young, &[("birthday", "'2000-01-01'")]).await;
    set_profile(&db, old, &[("birthday", "'1980-01-01'")]).await;

    let options = ActorService::new(db.pool())
        .filter_options(None, None, date("2026-10-04"))
        .await
        .expect("filter options");
    assert!(
        options.age.min.unwrap() <= options.age.max.unwrap(),
        "年龄区间的最小值不该大于最大值（方向反了会让筛选落空）"
    );
    assert_eq!(options.as_of_date, date("2026-10-04"));
    assert!(options.age.populated_count >= 2);
    let _ = (young, old);
}

#[tokio::test]
async fn filter_options_count_only_rows_that_have_a_value() {
    let db = TestDb::require().await;
    let with_height = seed_actor(&db).await;
    let without = seed_actor(&db).await;
    set_profile(&db, with_height, &[("height_cm", "165")]).await;

    let options = ActorService::new(db.pool())
        .filter_options(None, None, date("2026-10-04"))
        .await
        .expect("filter options");
    assert!(options.actor_count >= 2, "演员总数含没有身高的");
    assert!(
        options.height_cm.populated_count < options.actor_count,
        "populated_count 是 COUNT(height_cm)，必须小于 COUNT(*)"
    );
    assert_eq!(options.height_cm.min, Some(165));
    let _ = (with_height, without);
}

#[tokio::test]
async fn cup_options_group_by_the_normalized_value() {
    let db = TestDb::require().await;
    let first = seed_actor(&db).await;
    let second = seed_actor(&db).await;
    set_profile(&db, first, &[("cup", "'a'")]).await;
    set_profile(&db, second, &[("cup", "' A '")]).await;

    let options = ActorService::new(db.pool())
        .filter_options(None, None, date("2026-10-04"))
        .await
        .expect("filter options");
    let bucket_a = options
        .cups
        .iter()
        .find(|(value, _)| value == "A")
        .expect("归一后应当聚成同一个桶");
    assert!(bucket_a.1 >= 2, "'a' 与 ' A ' 应当合并计数");
    assert!(
        !options.cups.iter().any(|(value, _)| value.is_empty()),
        "空罩杯不该出现成一个桶"
    );
}

// ================================================================ 详情与墓碑

#[tokio::test]
async fn detail_reports_a_missing_actor_as_404_with_the_actor_id() {
    let db = TestDb::require().await;
    let err = ActorService::new(db.pool())
        .detail(999_999)
        .await
        .expect_err("不存在的演员应当 404");
    assert_eq!(err.status, 404);
    assert_eq!(err.code(), "actor_not_found");
    assert_eq!(
        err.api.details.unwrap().get("actor_id"),
        Some(&json!(999_999))
    );
}

#[tokio::test]
async fn detail_follows_one_hop_of_the_tombstone_pointer() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let tombstone = seed_actor(&db).await;
    set_profile(&db, target, &[("name", "'保留记录'")]).await;
    ActorRepository::new(db.pool().clone())
        .mark_merged(&[tombstone], target)
        .await
        .expect("mark merged");

    let view = ActorService::new(db.pool())
        .detail(tombstone)
        .await
        .expect("detail");
    assert_eq!(
        view.actor.id, target,
        "查墓碑应当返回保留记录（上游 _require_actor 跳一跳）"
    );
    assert_eq!(view.actor.name, "保留记录");
}

#[tokio::test]
async fn a_dangling_tombstone_pointer_is_refused_by_the_foreign_key() {
    // 原以为「跳一跳指向的行不存在时退回原行」这个回退分支能用真库测到，
    // 写完才发现**造不出那种数据**：`merged_into_id` 上有外键，
    // 指向不存在的行会被 PostgreSQL 直接拒掉。
    //
    // 所以这条断言反过来钉住「这种数据进不来」—— 那才是回退分支不可达的原因。
    // （`sm-db` 侧已有同名断言 `foreign_key_forbids_a_dangling_tombstone_pointer`；
    // 这里再钉一次是因为「回退分支不可达」这个结论属于 service 层。）
    let db = TestDb::require().await;
    let tombstone = seed_actor(&db).await;
    let err = sqlx::query("UPDATE actor SET merged_into_id = 999999 WHERE id = $1")
        .bind(tombstone)
        .execute(db.pool())
        .await
        .expect_err("外键应当拒绝悬空指针");
    let message = err.to_string();
    assert!(
        message.contains("merged_into_id"),
        "报的应当是那个外键，实际：{message}"
    );
}

#[tokio::test]
async fn detail_reports_movie_count_and_the_effective_profile_image() {
    let db = TestDb::require().await;
    let actor_id = seed_actor(&db).await;
    let (movie_id, _) = seed_movie(&db, None).await;
    MovieActorRepository::new(db.pool().clone())
        .link(movie_id, actor_id)
        .await
        .expect("link");

    // 库图 + 覆盖图各一张：生效头像必须是**覆盖**那张
    let library: (i32,) = sqlx::query_as("INSERT INTO image (origin) VALUES ($1) RETURNING id")
        .bind(format!("actors/javdb-{}.webp", n()))
        .fetch_one(db.pool())
        .await
        .expect("insert library image");
    let manual: (i32,) = sqlx::query_as("INSERT INTO image (origin) VALUES ($1) RETURNING id")
        .bind(format!("actors/manual-{}.webp", n()))
        .fetch_one(db.pool())
        .await
        .expect("insert manual image");
    sqlx::query(
        "UPDATE actor SET profile_image_id = $1, profile_image_override_id = $2 WHERE id = $3",
    )
    .bind(library.0)
    .bind(manual.0)
    .bind(actor_id)
    .execute(db.pool())
    .await
    .expect("set images");

    let view = ActorService::new(db.pool())
        .detail(actor_id)
        .await
        .expect("detail");
    assert_eq!(view.movie_count, 1);
    assert_eq!(view.image_id, Some(manual.0), "覆盖图优先于库图");
    assert!(view.actor.has_profile_image_override());
}

#[tokio::test]
async fn manual_fields_are_exposed_sorted() {
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    sqlx::query("UPDATE actor SET field_owners = $1 WHERE id = $2")
        .bind(json!({ "cup": "host:manual", "bust_cm": "host:manual", "gender": "host:javdb" }))
        .bind(id)
        .execute(db.pool())
        .await
        .expect("set owners");

    let view = ActorService::new(db.pool())
        .detail(id)
        .await
        .expect("detail");
    assert_eq!(view.manual_fields, vec!["bust_cm", "cup"]);
}

// ================================================================ 资料修改

#[tokio::test]
async fn updating_a_profile_claims_ownership_and_bumps_the_revision() {
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let body = json!({ "cup": "D", "height_cm": 168 })
        .as_object()
        .unwrap()
        .clone();

    ActorService::new(db.pool())
        .update_profile(id, &body, date("2026-10-04"))
        .await
        .expect("update");

    let after = ActorRepository::new(db.pool().clone())
        .require_by_id(id)
        .await
        .expect("reload");
    assert_eq!(after.cup.as_deref(), Some("D"), "罩杯已归一为大写");
    assert_eq!(after.height_cm, Some(168));
    let owners = after.field_owners.as_object().expect("owners");
    assert_eq!(
        owners.get("cup").and_then(|v| v.as_str()),
        Some("host:manual")
    );
    assert_eq!(
        owners.get("height_cm").and_then(|v| v.as_str()),
        Some("host:manual")
    );
    assert_eq!(after.mutation_revision, 1, "资料变更要推进版本");
}

#[tokio::test]
async fn renaming_the_display_name_does_not_claim_ownership_or_bump_the_revision() {
    // 上游 `scalar_fields = changes & (EDITABLE - {"display_name_override"})`。
    // 少做这一步会让「人工改过显示名」被误当成「人工改过资料」，
    // 而合并时的填空规则会因此跳过人工值。
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let body = json!({ "display_name_override": " 艺名 " })
        .as_object()
        .unwrap()
        .clone();

    let view = ActorService::new(db.pool())
        .update_profile(id, &body, date("2026-10-04"))
        .await
        .expect("update");
    assert_eq!(view.actor.display_name_override.as_deref(), Some("艺名"));
    assert!(
        view.manual_fields.is_empty(),
        "display_name_override 不该进 field_owners"
    );
    assert_eq!(view.actor.mutation_revision, 0, "版本不该被推进");
}

#[tokio::test]
async fn an_empty_update_is_rejected_with_its_own_code() {
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let err = ActorService::new(db.pool())
        .update_profile(id, &Map::new(), date("2026-10-04"))
        .await
        .expect_err("空更新应当被拒");
    assert_eq!(err.status, 422);
    assert_eq!(err.code(), "empty_actor_update");
}

#[tokio::test]
async fn a_future_birthday_is_rejected() {
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let body = json!({ "birthday": "2030-01-01" })
        .as_object()
        .unwrap()
        .clone();
    let err = ActorService::new(db.pool())
        .update_profile(id, &body, date("2026-10-04"))
        .await
        .expect_err("未来的生日应当被拒");
    assert_eq!(err.status, 422);
    assert_eq!(err.code(), "invalid_actor_profile");
}

#[tokio::test]
async fn a_todays_birthday_is_accepted() {
    // 边界是 `<=`：今天就是生日必须能存。
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let body = json!({ "birthday": "2026-10-04" })
        .as_object()
        .unwrap()
        .clone();
    ActorService::new(db.pool())
        .update_profile(id, &body, date("2026-10-04"))
        .await
        .expect("今天应当可存");
}

#[tokio::test]
async fn an_unknown_field_alone_collapses_into_the_empty_update_error() {
    // pydantic 默认忽略未知键，所以它既不报错也不写入 —— 请求体因此是空的。
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let body = json!({ "nickname": "x" }).as_object().unwrap().clone();
    let err = ActorService::new(db.pool())
        .update_profile(id, &body, date("2026-10-04"))
        .await
        .expect_err("只有未知键时应当落到 empty_actor_update");
    assert_eq!(err.code(), "empty_actor_update");
}

#[tokio::test]
async fn updating_a_missing_actor_is_404_even_with_an_invalid_body() {
    // 上游 `update_profile` 开头就 `_require_actor`，所以 404 优先于请求体校验。
    let db = TestDb::require().await;
    let body = json!({ "cup": "TOOLONG" }).as_object().unwrap().clone();
    let err = ActorService::new(db.pool())
        .update_profile(999_999, &body, date("2026-10-04"))
        .await
        .expect_err("不存在的演员");
    assert_eq!(err.status, 404);
}

#[tokio::test]
async fn clearing_a_field_writes_null_rather_than_comparing_against_null() {
    // `col = NULL` 在 SQL 里是 UNKNOWN，**永远不成立** —— 清空必须写 NULL 字面量。
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    set_profile(&db, id, &[("cup", "'D'")]).await;
    let body = json!({ "cup": null }).as_object().unwrap().clone();

    ActorService::new(db.pool())
        .update_profile(id, &body, date("2026-10-04"))
        .await
        .expect("清空罩杯");
    let after = ActorRepository::new(db.pool().clone())
        .require_by_id(id)
        .await
        .expect("reload");
    assert_eq!(after.cup, None, "null 必须真的写进去");
}

// ================================================================ 订阅

#[tokio::test]
async fn subscribing_sets_the_timestamp_and_resubscribing_keeps_it() {
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let svc = ActorService::new(db.pool());

    svc.set_subscription(id, true).await.expect("subscribe");
    let first = ActorRepository::new(db.pool().clone())
        .require_by_id(id)
        .await
        .expect("reload");
    let first_at = first.subscribed_at.expect("首次订阅要写时间");
    assert!(first.is_subscribed);

    // 人为把时间往回拨，再订阅一次：不应被刷新
    sqlx::query("UPDATE actor SET subscribed_at = $1 WHERE id = $2")
        .bind(
            NaiveDate::from_ymd_opt(2020, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
        .bind(id)
        .execute(db.pool())
        .await
        .expect("回拨");

    svc.set_subscription(id, true).await.expect("resubscribe");
    let after = ActorRepository::new(db.pool().clone())
        .require_by_id(id)
        .await
        .expect("reload");
    assert_ne!(
        after.subscribed_at,
        Some(first_at),
        "测试前提：时间确实被改过"
    );
    assert_eq!(
        after.subscribed_at,
        Some(
            NaiveDate::from_ymd_opt(2020, 1, 1)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap()
        ),
        "重复订阅不刷新 subscribed_at，否则「按订阅时间倒序」每次点按都会置顶"
    );

    svc.set_subscription(id, false).await.expect("unsubscribe");
    let cleared = ActorRepository::new(db.pool().clone())
        .require_by_id(id)
        .await
        .expect("reload");
    assert!(!cleared.is_subscribed);
    assert_eq!(cleared.subscribed_at, None, "退订要清空时间");
}

#[tokio::test]
async fn subscribing_a_missing_actor_is_404() {
    let db = TestDb::require().await;
    let err = ActorService::new(db.pool())
        .set_subscription(999_999, true)
        .await
        .expect_err("不存在的演员");
    assert_eq!(err.status, 404);
    assert_eq!(err.code(), "actor_not_found");
}

#[tokio::test]
async fn subscribing_through_a_tombstone_acts_on_the_preserved_record() {
    let db = TestDb::require().await;
    let target = seed_actor(&db).await;
    let tombstone = seed_actor(&db).await;
    ActorRepository::new(db.pool().clone())
        .mark_merged(&[tombstone], target)
        .await
        .expect("mark merged");

    ActorService::new(db.pool())
        .set_subscription(tombstone, true)
        .await
        .expect("订阅");
    let after = ActorRepository::new(db.pool().clone())
        .require_by_id(target)
        .await
        .expect("reload");
    assert!(after.is_subscribed, "订阅要落在保留记录上");
}

// ================================================================ 关联查询

#[tokio::test]
async fn movie_ids_are_returned_in_id_order() {
    let db = TestDb::require().await;
    let actor_id = seed_actor(&db).await;
    let links = MovieActorRepository::new(db.pool().clone());
    let mut ids = Vec::new();
    for _ in 0..3 {
        let (movie_id, _) = seed_movie(&db, None).await;
        links.link(movie_id, actor_id).await.expect("link");
        ids.push(movie_id);
    }
    ids.sort_unstable();

    let got = ActorService::new(db.pool())
        .movie_ids(actor_id)
        .await
        .expect("movie ids");
    assert_eq!(got, ids, "按影片 id 升序返回");
}

#[tokio::test]
async fn tags_are_deduplicated_across_movies_and_sorted_by_name() {
    let db = TestDb::require().await;
    let actor_id = seed_actor(&db).await;
    let links = MovieActorRepository::new(db.pool().clone());
    let tags = TagRepository::new(db.pool().clone());
    let movie_tags = MovieTagRepository::new(db.pool().clone());

    let tag_a = tags
        .upsert_by_name(&format!("标签A{}", n()))
        .await
        .expect("tag");
    let tag_b = tags
        .upsert_by_name(&format!("标签B{}", n()))
        .await
        .expect("tag");

    // 两部影片都挂上同一个标签 -> 去重后应当只有一条
    for _ in 0..2 {
        let (movie_id, _) = seed_movie(&db, None).await;
        links.link(movie_id, actor_id).await.expect("link");
        movie_tags.link(movie_id, tag_a.id).await.expect("tag link");
    }
    let (movie_id, _) = seed_movie(&db, None).await;
    links.link(movie_id, actor_id).await.expect("link");
    movie_tags.link(movie_id, tag_b.id).await.expect("tag link");

    let got = ActorService::new(db.pool())
        .tags(actor_id)
        .await
        .expect("tags");
    let got_ids: Vec<i32> = got.iter().map(|t| t.tag_id).collect();
    assert_eq!(got_ids.len(), 2, "跨影片的同一标签只出现一次");
    let names: Vec<&str> = got.iter().map(|t| t.name.as_str()).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    assert_eq!(names, sorted, "按标签名升序");
}

#[tokio::test]
async fn years_are_grouped_and_sorted_descending_skipping_null_release_dates() {
    let db = TestDb::require().await;
    let actor_id = seed_actor(&db).await;
    let links = MovieActorRepository::new(db.pool().clone());

    for (count, year) in [(2, 2020), (1, 2023)] {
        for _ in 0..count {
            let (movie_id, _) = seed_movie(&db, NaiveDate::from_ymd_opt(year, 6, 1)).await;
            links.link(movie_id, actor_id).await.expect("link");
        }
    }
    // 2021 年一部，但 release_date 为空 -> 不进年份分布
    let (movie_id, _) = seed_movie(&db, None).await;
    links.link(movie_id, actor_id).await.expect("link");

    let years = ActorService::new(db.pool())
        .years(actor_id)
        .await
        .expect("years");
    let got: Vec<(i32, i64)> = years.iter().map(|y| (y.year, y.movie_count)).collect();
    assert_eq!(
        got,
        vec![(2023, 1), (2020, 2)],
        "按年份降序；release_date 为空的影片被排除"
    );
}

#[tokio::test]
async fn related_queries_404_for_a_missing_actor() {
    let db = TestDb::require().await;
    let svc = ActorService::new(db.pool());
    for err in [
        svc.movie_ids(999_999).await.expect_err("movie ids"),
        svc.tags(999_999).await.expect_err("tags"),
        svc.years(999_999).await.expect_err("years"),
        svc.detail(999_999).await.expect_err("detail"),
    ] {
        assert_eq!(err.status, 404, "关联查询也要先确认演员存在");
        assert_eq!(err.code(), "actor_not_found");
    }
}

// ================================================================ 头像清除

#[tokio::test]
async fn clearing_the_profile_image_is_idempotent() {
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let manual: (i32,) = sqlx::query_as("INSERT INTO image (origin) VALUES ($1) RETURNING id")
        .bind(format!("actors/manual-{}.webp", n()))
        .fetch_one(db.pool())
        .await
        .expect("insert image");
    sqlx::query("UPDATE actor SET profile_image_override_id = $1 WHERE id = $2")
        .bind(manual.0)
        .bind(id)
        .execute(db.pool())
        .await
        .expect("set override");

    let view = ActorService::new(db.pool())
        .clear_profile_image(id)
        .await
        .expect("clear");
    assert_eq!(view.actor.profile_image_override_id, None);
    assert!(!view.actor.has_profile_image_override());

    // 再清一次：上游在「本来就没有覆盖」时提前返回，不动 updated_at、不报错
    let again = ActorService::new(db.pool())
        .clear_profile_image(id)
        .await
        .expect("第二次清除应当是幂等的");
    assert_eq!(again.actor.profile_image_override_id, None);
}

#[tokio::test]
async fn clearing_the_profile_image_keeps_the_library_image() {
    let db = TestDb::require().await;
    let id = seed_actor(&db).await;
    let library: (i32,) = sqlx::query_as("INSERT INTO image (origin) VALUES ($1) RETURNING id")
        .bind(format!("actors/javdb-{}.webp", n()))
        .fetch_one(db.pool())
        .await
        .expect("insert library image");
    let manual: (i32,) = sqlx::query_as("INSERT INTO image (origin) VALUES ($1) RETURNING id")
        .bind(format!("actors/manual-{}.webp", n()))
        .fetch_one(db.pool())
        .await
        .expect("insert manual image");
    sqlx::query(
        "UPDATE actor SET profile_image_id = $1, profile_image_override_id = $2 WHERE id = $3",
    )
    .bind(library.0)
    .bind(manual.0)
    .bind(id)
    .execute(db.pool())
    .await
    .expect("set images");

    let view = ActorService::new(db.pool())
        .clear_profile_image(id)
        .await
        .expect("clear");
    assert_eq!(
        view.actor.profile_image_id,
        Some(library.0),
        "库图不该被清掉"
    );
    assert_eq!(
        view.image_id,
        Some(library.0),
        "清除覆盖后生效头像回落���库图"
    );
}
