//! 播放列表的**查询编排**集成测试：列表排序 + 成员计数、分辨率档位聚合。
//!
//! 与 [`playlist_service`]（同一目录下）分工：那边测**业务规则**（名称唯一、
//! 保留名、状态码/错误码），这边测**查询形状** —— 排序键、计数来源、聚合
//! 分组。两类缺陷的定位方式不同：规则错会返回错误的码，编排错会返回**看起来
//! 合理但错的列表**（系统列表排到后面、计数恒为 0、8K 影片被算进 2K）。
//!
//! 上游出处：`playlist_service.list_playlists` / `list_playlist_resolutions`
//! 与 `catalog/movie_resolution_service.resolution_level_expression`。
//!
//! # 跑在真实 PostgreSQL 上
//!
//! 档位 `CASE` 表达式里的 `split_part(...)::int` 与 `~ '^\d+x\d+$'` 都是
//! PostgreSQL 特有的行为，单元测试**证明不了**它们 —— 一条脏 `resolution`
//! 就能让整个查询从「少算一档」变成「报 invalid input syntax 而 500」。
//! 这批测试存在的首要理由就是覆盖那两种结局。

use sm_db::repo::{
    MediaLibraryRepository, MediaRepository, MovieRepository, NewMedia, NewMediaLibrary, NewMovie,
    PlaylistMovieRepository, PlaylistRepository,
};
use sm_db::testing::TestDb;
use sm_service::collections::PlaylistService;

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 一个 `media_library`，媒体必须挂在某个库下。
async fn seed_library(db: &TestDb) -> i32 {
    MediaLibraryRepository::new(db.pool().clone())
        .insert(&NewMediaLibrary {
            name: format!("lib-{}", n()),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        })
        .await
        .expect("insert media_library")
        .id
}

/// 建一部影片，返回 `(id, movie_number)`。
async fn seed_movie(db: &TestDb) -> (i32, String) {
    let number = format!("PLL-{:06}", n());
    let movie = MovieRepository::new(db.pool().clone())
        .insert(&NewMovie {
            movie_number: number.clone(),
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
        .expect("insert movie");
    (movie.id, number)
}

/// 造一条挂到影片下的媒体。`resolution` 原样写入 —— 包括脏值，
/// 因为「脏值不该让查询报错」正是要验的东西。
async fn seed_media(db: &TestDb, movie_number: &str, resolution: Option<&str>) {
    let library_id = seed_library(db).await;
    MediaRepository::new(db.pool().clone())
        .insert(&NewMedia {
            library_id,
            file_name: format!("m-{}.mp4", n()),
            file_size_bytes: 1,
            // XOR：给了 movie_number 就不能给 video_item_id
            movie_number: Some(movie_number.to_owned()),
            video_item_id: None,
            storage_ref: None,
            resolution: resolution.map(str::to_owned),
            file_hash: None,
            import_source_identity: None,
            duration_seconds: 1,
            video_info: None,
        })
        .await
        .expect("insert media");
}

// ================================================================ 列表 + 计数

#[tokio::test]
async fn a_fresh_list_reports_zero_movies() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("空列表", None).await.expect("create");

    let count = svc.member_count(playlist.id).await.expect("member_count");
    assert_eq!(count, 0, "新建列表没有任何成员");
}

#[tokio::test]
async fn the_count_reflects_added_movies() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("计数列表", None).await.expect("create");
    for _ in 0..3 {
        let (_, number) = seed_movie(&db).await;
        svc.add_movie(playlist.id, &number).await.expect("add");
    }

    let count = svc.member_count(playlist.id).await.expect("member_count");
    assert_eq!(count, 3, "加了 3 部就该数到 3 —— 恒为 0 是本测试要抓的缺陷");

    // 移出一部后计数跟着走
    let (_, number) = seed_movie(&db).await;
    svc.add_movie(playlist.id, &number).await.expect("add");
    assert_eq!(svc.member_count(playlist.id).await.unwrap(), 4);
    svc.remove_movie(playlist.id, &number)
        .await
        .expect("remove");
    assert_eq!(
        svc.member_count(playlist.id).await.unwrap(),
        3,
        "移出后计数应减少"
    );
}

#[tokio::test]
async fn list_carries_the_count_for_every_playlist() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());

    let empty = svc.create("批量-空", None).await.expect("create");
    let filled = svc.create("批量-有", None).await.expect("create");
    for _ in 0..2 {
        let (_, number) = seed_movie(&db).await;
        svc.add_movie(filled.id, &number).await.expect("add");
    }

    let rows = svc.list(true).await.expect("list");
    // 库里可能有其它测试留下的列表，所以按 id 找而不是按下标。
    let get = |id: i32| {
        rows.iter()
            .find(|r| r.playlist.id == id)
            .map(|r| r.movie_count)
    };
    assert_eq!(get(empty.id), Some(0), "空列表的计数是 0");
    assert_eq!(get(filled.id), Some(2), "批量计数要落到对应的列表上");
}

/// 没有成员行的列表必须报 0，而不是「查不到」而消失。
#[tokio::test]
async fn a_list_with_no_member_rows_is_still_listed() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("零成员", None).await.expect("create");

    let rows = svc.list(true).await.expect("list");
    let row = rows
        .iter()
        .find(|r| r.playlist.id == playlist.id)
        .expect("列表必须出现，哪怕一个成员都没有");
    assert_eq!(row.movie_count, 0);
}

// ================================================================ 排序

#[tokio::test]
async fn the_system_playlist_comes_first() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());

    // 先建一个自定义列表，再触发系统列表 —— 这样如果排序只看
    // `created_at` 或 `id`，系统列表会落在后面，测试才有区分力。
    svc.create("排序-自定义", None).await.expect("create");
    svc.touch_recently_played(seed_movie(&db).await.0)
        .await
        .expect("touch recently played");

    let rows = svc.list(true).await.expect("list");
    let system_index = rows
        .iter()
        .position(|r| r.playlist.kind == "recently_played")
        .expect("系统列表应出现");
    let custom_index = rows
        .iter()
        .position(|r| r.playlist.name == "排序-自定义")
        .expect("自定义列表应出现");

    assert!(
        system_index < custom_index,
        "系统列表(#{system_index}) 必须排在自定义列表(#{custom_index})之前"
    );
}

#[tokio::test]
async fn include_system_false_drops_the_system_playlist() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let custom = svc.create("过滤-自定义", None).await.expect("create");
    svc.touch_recently_played(seed_movie(&db).await.0)
        .await
        .expect("touch");

    let rows = svc.list(false).await.expect("list");
    assert!(
        rows.iter().all(|r| r.playlist.kind != "recently_played"),
        "include_system=false 时不得返回系统列表"
    );
    assert!(
        rows.iter().any(|r| r.playlist.id == custom.id),
        "自定义列表必须还在"
    );
}

// ================================================================ 分辨率档位

/// 把 `resolution_options` 收成 `HashMap<档位, 计数>`，断言时更直观。
fn buckets(rows: &[sm_service::collections::playlist::ResolutionOption]) -> Vec<(String, i32)> {
    rows.iter()
        .map(|o| (o.resolution.clone(), o.count))
        .collect()
}

#[tokio::test]
async fn a_single_movie_lands_in_exactly_one_bucket() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("档位-单片", None).await.expect("create");
    let (_, number) = seed_movie(&db).await;
    svc.add_movie(playlist.id, &number).await.expect("add");
    seed_media(&db, &number, Some("1920x1080")).await;

    assert_eq!(
        buckets(&svc.resolution_options(playlist.id).await.unwrap()),
        vec![("1080P".to_owned(), 1)]
    );
}

/// 一部影片有多个媒体时，**只计入最高的那一档**，且不重复计数。
///
/// 这是 `MAX(level)` 存在的全部理由：一部 4K + 1080P 的影片应该算 1 部 4K，
/// 而不是「4K 一部、1080P 一部」—— 那样筛选项的 count 之和会超过列表里
/// 的影片数，客户端显示的总数就错了。
#[tokio::test]
async fn a_movie_is_counted_once_at_its_highest_resolution() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("档位-取最高", None).await.expect("create");
    let (_, number) = seed_movie(&db).await;
    svc.add_movie(playlist.id, &number).await.expect("add");
    seed_media(&db, &number, Some("1920x1080")).await;
    seed_media(&db, &number, Some("3840x2160")).await;

    assert_eq!(
        buckets(&svc.resolution_options(playlist.id).await.unwrap()),
        vec![("4K".to_owned(), 1)],
        "4K 影片只能出现在 4K 一档，1080P 那一行不该有它"
    );
}

/// 8K 必须按**宽度**判定，不能被高度先命中而落到 2K。
///
/// `7680x4320` 的高度 4320 ≥ 1440，若 `CASE` 先判高度就会落进 2K 档。
#[tokio::test]
async fn eight_k_is_judged_by_width_not_height() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("档位-8K", None).await.expect("create");
    let (_, number) = seed_movie(&db).await;
    svc.add_movie(playlist.id, &number).await.expect("add");
    seed_media(&db, &number, Some("7680x4320")).await;

    assert_eq!(
        buckets(&svc.resolution_options(playlist.id).await.unwrap()),
        vec![("8K".to_owned(), 1)]
    );
}

/// 输出顺序必须是从高到低，且丢掉计数为 0 的档位。
#[tokio::test]
async fn options_are_high_to_low_and_zero_counts_are_dropped() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("档位-顺序", None).await.expect("create");

    for resolution in ["7680x4320", "1920x1080", "854x480"] {
        let (_, number) = seed_movie(&db).await;
        svc.add_movie(playlist.id, &number).await.expect("add");
        seed_media(&db, &number, Some(resolution)).await;
    }

    assert_eq!(
        buckets(&svc.resolution_options(playlist.id).await.unwrap()),
        vec![
            ("8K".to_owned(), 1),
            ("1080P".to_owned(), 1),
            ("480P".to_owned(), 1),
        ],
        "必须按 8K→1080P→480P 排列，且 4K/2K/720P/360P 不出现（计数为 0）"
    );
}

/// 脏 `resolution` 值必须被**排除**，而不是让整个查询报错。
///
/// `1920*1080` 能通过人眼但匹配不上 `^\d+x\d+$`；如果过滤条件写错，
/// `split_part('1920*1080','x',1)::int` 会得到 `1920*1080` 并抛
/// `invalid input syntax for type integer` —— 整个端点 500。
/// 这是本文件最重要的一条。
#[tokio::test]
async fn a_dirty_resolution_value_is_excluded_rather_than_failing_the_query() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("档位-脏值", None).await.expect("create");

    // 干净的一部，保证查询有东西可返回
    let (_, good) = seed_movie(&db).await;
    svc.add_movie(playlist.id, &good).await.expect("add");
    seed_media(&db, &good, Some("1920x1080")).await;

    // 三种脏值：非 WxH、带分隔符非 x、空值形态
    for dirty in ["1920*1080", "HD", "0x0"] {
        let (_, number) = seed_movie(&db).await;
        svc.add_movie(playlist.id, &number).await.expect("add");
        seed_media(&db, &number, Some(dirty)).await;
    }

    // 不 panic、不 Err，且只有干净那部被计入
    let options = svc
        .resolution_options(playlist.id)
        .await
        .expect("脏值不得让查询失败");
    assert_eq!(
        buckets(&options),
        vec![("1080P".to_owned(), 1)],
        "只有 1920x1080 该被计入；0x0 是可解析但无档位，也不该出现"
    );
}

/// 列表不存在时 404，且**先于**聚合查询。
#[tokio::test]
async fn an_unknown_playlist_is_404() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    // 借用一个必然不存在的 id：列表 id 是 serial，跟已有行不撞。
    let missing = i32::MAX;

    let err = svc
        .resolution_options(missing)
        .await
        .expect_err("不存在的列表应 404");
    assert_eq!((err.status, err.code()), (404, "playlist_not_found"));
}

/// 空列表没有档位 —— 返回**空数组**而不是 404 或 null。
#[tokio::test]
async fn an_empty_playlist_has_no_resolution_options() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("档位-空列表", None).await.expect("create");

    assert!(svc
        .resolution_options(playlist.id)
        .await
        .expect("空列表不应报错")
        .is_empty());
}

/// 没有媒体的影片**不出现在任何档位里**，但计入 `movie_count`。
///
/// 成员数与档位数是两个不同的口径：前者数「列表里放了多少部」，后者数
/// 「有多少部能判定分辨率」。混起来会让客户端以为列表坏了。
#[tokio::test]
async fn a_movie_without_media_counts_for_membership_but_not_for_a_bucket() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("档位-无媒体", None).await.expect("create");
    let (_, number) = seed_movie(&db).await;
    svc.add_movie(playlist.id, &number).await.expect("add");
    // 故意不 seed_media

    assert_eq!(
        svc.member_count(playlist.id).await.unwrap(),
        1,
        "成员数是 1"
    );
    assert!(
        svc.resolution_options(playlist.id)
            .await
            .unwrap()
            .is_empty(),
        "没有媒体就没有可判定的档位"
    );
}

/// 判死（`valid = false`）的媒体不参与档位。
#[tokio::test]
async fn an_invalid_media_does_not_contribute_a_bucket() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("档位-判死", None).await.expect("create");
    let (_, number) = seed_movie(&db).await;
    svc.add_movie(playlist.id, &number).await.expect("add");
    seed_media(&db, &number, Some("1920x1080")).await;

    // 直接改库把这条媒体判死 —— 仓储层没有「判死」这个业务动作，
    // 它由清理流程触发，不属于本次要测的接口面。
    sqlx::query("UPDATE media SET valid = FALSE WHERE movie_number = $1")
        .bind(&number)
        .execute(db.pool())
        .await
        .expect("mark media invalid");

    assert!(
        svc.resolution_options(playlist.id)
            .await
            .unwrap()
            .is_empty(),
        "valid=false 的媒体不该贡献档位"
    );
}

// ================================================================ 跨列表隔离

/// 档位聚合只算**本列表内**的影片。
///
/// 漏掉 `pm.playlist = $1` 的话，库里所有影片的分辨率都会算进每一个列表，
/// 而这个缺陷在任何单列表测试里都看不出来。
#[tokio::test]
async fn buckets_only_count_movies_in_this_playlist() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let mine = svc.create("隔离-甲", None).await.expect("create");
    let other = svc.create("隔离-乙", None).await.expect("create");

    let (_, in_mine) = seed_movie(&db).await;
    svc.add_movie(mine.id, &in_mine).await.expect("add");
    seed_media(&db, &in_mine, Some("3840x2160")).await;

    // 另一部 1080P 的影片只在「乙」里
    let (_, in_other) = seed_movie(&db).await;
    svc.add_movie(other.id, &in_other).await.expect("add");
    seed_media(&db, &in_other, Some("1920x1080")).await;

    assert_eq!(
        buckets(&svc.resolution_options(mine.id).await.unwrap()),
        vec![("4K".to_owned(), 1)],
        "甲只该看到自己那部 4K"
    );
    assert_eq!(
        buckets(&svc.resolution_options(other.id).await.unwrap()),
        vec![("1080P".to_owned(), 1)]
    );
}

/// 直接验仓储层：多部影片各自归组，而不是被 `GROUP BY` 压成一行。
#[tokio::test]
async fn the_repository_groups_per_movie() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("仓储-分组", None).await.expect("create");

    let mut ids = Vec::new();
    for resolution in ["1920x1080", "3840x2160", "1280x720"] {
        let (id, number) = seed_movie(&db).await;
        svc.add_movie(playlist.id, &number).await.expect("add");
        seed_media(&db, &number, Some(resolution)).await;
        ids.push(id);
    }

    let rows = MovieRepository::new(db.pool().clone())
        .max_resolution_levels_by_playlist(playlist.id)
        .await
        .expect("aggregate");

    assert_eq!(rows.len(), 3, "三部影片必须是三行，不能被压成一行");
    // 仓储层返回 `(movie_id, max_level)` 元组；这里按 id 对齐。
    let got: std::collections::HashMap<i32, i32> = rows.into_iter().collect();
    let want = [(ids[0], 4), (ids[1], 6), (ids[2], 3)];
    for (movie_id, level) in want {
        assert_eq!(
            got.get(&movie_id),
            Some(&level),
            "影片 {movie_id} 的档位序号应是 {level}"
        );
    }
}

/// 同一列表里没有媒体时，仓储层返回空数组（不报错）。
#[tokio::test]
async fn the_repository_returns_nothing_for_a_playlist_without_media() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("仓储-空", None).await.expect("create");
    let (_, number) = seed_movie(&db).await;
    svc.add_movie(playlist.id, &number).await.expect("add");

    let rows = MovieRepository::new(db.pool().clone())
        .max_resolution_levels_by_playlist(playlist.id)
        .await
        .expect("空结果不是错误");
    assert!(rows.is_empty());
}

/// `count_by_playlists` 的空输入不发查询、返回空 map。
#[tokio::test]
async fn counting_an_empty_id_list_is_an_empty_map() {
    let db = TestDb::require().await;
    let repo = PlaylistMovieRepository::new(db.pool().clone());
    let counts = repo.count_by_playlists(&[]).await.expect("空输入");
    assert!(counts.is_empty());
}

/// 重复加入同一部影片**不会**让计数虚增。
///
/// `add_movie` 走 `ON CONFLICT DO NOTHING`，所以「幂等」必须在计数上
/// 也成立 —— 否则 UI 连点两下「加入」就会显示 2 部影片。
#[tokio::test]
async fn adding_the_same_movie_twice_does_not_inflate_the_count() {
    let db = TestDb::require().await;
    let svc = PlaylistService::new(db.pool());
    let playlist = svc.create("幂等-计数", None).await.expect("create");
    let (_, number) = seed_movie(&db).await;

    svc.add_movie(playlist.id, &number).await.expect("add");
    svc.add_movie(playlist.id, &number).await.expect("re-add");

    assert_eq!(
        svc.member_count(playlist.id).await.unwrap(),
        1,
        "重复加入是幂等的，计数不能变成 2"
    );
}

/// `list_ordered` 在没有任何列表时不报错。
#[tokio::test]
async fn the_repository_lists_nothing_when_there_are_no_playlists() {
    let db = TestDb::require().await;
    let repo = PlaylistRepository::new(db.pool().clone());
    // 库里必然已有其它测试留下的列表，所以只断言「不报错」，
    // 具体的行数由 service 层的测试按 id 断言。
    repo.list_ordered(true).await.expect("list");
    repo.list_ordered(false).await.expect("list");
}
