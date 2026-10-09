//! 插件侧合集 service 的集成测试。
//!
//! # 本批最需要说清的一处：`ensure_*` 与 `set_*` 的区别
//!
//! | | 合集不存在时 |
//! |---|---|
//! | `ensure_playlist` | **创建**（get-or-create），并把名字同步成给定的 |
//! | `set_playlist_movies` | **404** `plugin_collection_not_found` |
//!
//! 混用这两者是危险的：`set_*` 若走 `ensure_*`，一个拼错的 key 会**静默创建
//! 一个空合集**而不是报错，插件会以为设置成功了。
//!
//! 上游出处：`src/service/collections/plugin_collection_service.py`（217 行）。

use sm_db::collections::PluginOwned;
use sm_db::repo::{MovieRepository, NewMovie};
use sm_db::testing::TestDb;
use sm_service::collections::PluginCollectionService;

fn n() -> u32 {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

#[track_caller]
fn assert_error(err: &sm_service::error::ServiceError, status: u16, code: &str) {
    assert_eq!((err.status, err.code()), (status, code), "状态码与错误码");
}

async fn seed_movie(db: &TestDb) -> (i32, String) {
    let number = format!("PLG-{:06}", n());
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

// ================================================================ ensure

#[tokio::test]
async fn ensure_creates_then_resyncs_the_name() {
    // 上游 `_ensure_collection` 找到已存在的之后会**比 name 与 description**，
    // 不同就改。所以插件每次启动调一次 ensure，是把列表重命名成代码里
    // 写的那个名字 —— 显示名由插件决定。
    let db = TestDb::require().await;
    let svc = PluginCollectionService::new(db.pool());
    let key = format!("k{}", n());

    let first = svc
        .ensure_playlist("plugin-a", &key, "插件的列表", Some("第一版"))
        .await
        .unwrap();
    assert_eq!(first.name, "插件的列表");
    assert!(first.is_plugin_owned(), "应带插件归属");
    assert_eq!(first.plugin_key.as_deref(), Some(key.as_str()));

    // 再调一次，同一个 id。
    let second = svc
        .ensure_playlist("plugin-a", &key, "插件的列表", Some("第一版"))
        .await
        .unwrap();
    assert_eq!(first.id, second.id, "ensure 是 get-or-create");

    // 名字与描述都变 —— 应被同步。
    let third = svc
        .ensure_playlist("plugin-a", &key, "新名字", Some("第二版"))
        .await
        .unwrap();
    assert_eq!(third.id, first.id, "仍是同一行");
    assert_eq!(third.name, "新名字", "名字被同步成插件给定的");
    assert_eq!(third.description, "第二版");
}

#[tokio::test]
async fn two_plugins_may_reuse_the_same_key() {
    // 唯一索引是 `(owner_plugin_id, plugin_key)`，不是单列 `plugin_key`
    // —— 所以不同插件用同一个 key 各自独立。
    let db = TestDb::require().await;
    let svc = PluginCollectionService::new(db.pool());
    let key = format!("shared{}", n());

    let a = svc
        .ensure_playlist("plugin-a", &key, "A 的", None)
        .await
        .unwrap();
    let b = svc
        .ensure_playlist("plugin-b", &key, "B 的", None)
        .await
        .expect("不同插件可以用同一个 key");
    assert_ne!(a.id, b.id);
}

#[tokio::test]
async fn a_blank_or_overlong_key_is_a_programmer_error_not_a_422() {
    // 上游在这里抛 `ValueError` 而不是 `ApiError` —— 因为**我们自己的代码**
    // 调用姿势不对（插件 facade 传了空 key），不是用户输入有问题。
    //
    // 映射成 422 会让「服务端自己传错参数」伪装成「用户输入无效」。
    let db = TestDb::require().await;
    let svc = PluginCollectionService::new(db.pool());

    for bad in ["", "   "] {
        let err = svc
            .ensure_playlist("plugin-a", bad, "名字", None)
            .await
            .expect_err("空 key 应被拒");
        assert_error(&err, 500, "programmer_error");
    }

    // 上限 128（plugin_key 列的宽度），不是 255（那是 name 的宽度）。
    let too_long = "k".repeat(129);
    let err = svc
        .ensure_playlist("plugin-a", &too_long, "名字", None)
        .await
        .expect_err("超长 key 应被拒");
    assert_error(&err, 500, "programmer_error");
    assert!(err.api.message.contains("128"), "{}", err.api.message);

    // 名称上限 255，与 key 不同 —— 别抄错。
    let long_name = "n".repeat(256);
    let err = svc
        .ensure_playlist("plugin-a", "ok-key", &long_name, None)
        .await
        .expect_err("超长名称应被拒");
    assert_error(&err, 500, "programmer_error");
    assert!(err.api.message.contains("255"), "{}", err.api.message);
}

// ================================================================ set

#[tokio::test]
async fn set_requires_the_collection_to_exist() {
    // **本批最重要的一条**：`set_*` 走 `require_owned`，找不到就 404 ——
    // 它**不**创建。一个拼错的 key 会静默创建空合集，插件会以为成功了。
    let db = TestDb::require().await;
    let svc = PluginCollectionService::new(db.pool());
    let key = format!("absent{}", n());
    let (_, number) = seed_movie(&db).await;

    let err = svc
        .set_playlist_movies("plugin-a", &key, &[number])
        .await
        .expect_err("合集不存在应 404");
    assert_error(&err, 404, "plugin_collection_not_found");
    assert_eq!(err.api.message, "插件合集不存在");
    assert_eq!(
        err.api.details.as_ref().unwrap().get("plugin_key"),
        Some(&serde_json::json!(key.as_str())),
        "details 带 plugin_key"
    );

    // 确认真的没有创建。
    let after = svc
        .ensure_playlist("plugin-a", &key, "x", None)
        .await
        .unwrap();
    let members = sm_db::repo::PlaylistMovieRepository::new(db.pool().clone())
        .list_by_playlist(after.id)
        .await
        .unwrap();
    assert!(members.is_empty(), "set 不该创建合集或写入成员");
}

#[tokio::test]
async fn set_replaces_the_member_list_and_dedupes_by_movie_id() {
    let db = TestDb::require().await;
    let svc = PluginCollectionService::new(db.pool());
    let key = format!("rep{}", n());
    svc.ensure_playlist("plugin-a", &key, "列表", None)
        .await
        .unwrap();

    let (_id1, n1) = seed_movie(&db).await;
    let (_id2, n2) = seed_movie(&db).await;
    let (_id3, n3) = seed_movie(&db).await;

    svc.set_playlist_movies("plugin-a", &key, &[n1.clone(), n2.clone(), n3.clone()])
        .await
        .unwrap();
    let playlist = svc
        .ensure_playlist("plugin-a", &key, "列表", None)
        .await
        .unwrap();
    let members = sm_db::repo::PlaylistMovieRepository::new(db.pool().clone())
        .list_by_playlist(playlist.id)
        .await
        .unwrap();
    assert_eq!(members.len(), 3, "顺序 = 入参顺序（该表无 position）");

    // 重复番号按 id 去重。
    svc.set_playlist_movies("plugin-a", &key, &[n2.clone(), n1.clone(), n2.clone()])
        .await
        .unwrap();
    let members = sm_db::repo::PlaylistMovieRepository::new(db.pool().clone())
        .list_by_playlist(playlist.id)
        .await
        .unwrap();
    assert_eq!(members.len(), 2, "重复番号被去重");
    // 逐条查番号而不是用 iterator::map —— 后者要闭包返回 Future，
    // 而闭包本身不是 async。
    let mut order: Vec<String> = Vec::new();
    for m in &members {
        let number: String = sqlx::query_scalar("SELECT movie_number FROM movie WHERE id = $1")
            .bind(m.movie_id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        order.push(number);
    }
    assert_eq!(order, vec![n2, n1], "保留首次出现顺序");

    // 传空列表 = 清空。
    svc.set_playlist_movies("plugin-a", &key, &[])
        .await
        .unwrap();
    let members = sm_db::repo::PlaylistMovieRepository::new(db.pool().clone())
        .list_by_playlist(playlist.id)
        .await
        .unwrap();
    assert!(members.is_empty());
}

#[tokio::test]
async fn set_validates_every_number_before_writing() {
    // 全部解析完再写 —— 否则「清空之后才发现有一个番号不存在」。
    let db = TestDb::require().await;
    let svc = PluginCollectionService::new(db.pool());
    let key = format!("val{}", n());
    svc.ensure_playlist("plugin-a", &key, "列表", None)
        .await
        .unwrap();

    let (_id, good) = seed_movie(&db).await;
    svc.set_playlist_movies("plugin-a", &key, std::slice::from_ref(&good))
        .await
        .unwrap();

    let err = svc
        .set_playlist_movies("plugin-a", &key, &["NO-SUCH".to_owned(), good.clone()])
        .await
        .expect_err("不存在的番号应 404");
    assert_error(&err, 404, "movie_not_found");
    assert_eq!(err.api.message, "影片不存在");
    assert_eq!(
        err.api.details.as_ref().unwrap().get("movie_number"),
        Some(&serde_json::json!("NO-SUCH"))
    );

    // 关键：失败后原有成员还在。
    let playlist = svc
        .ensure_playlist("plugin-a", &key, "列表", None)
        .await
        .unwrap();
    let members = sm_db::repo::PlaylistMovieRepository::new(db.pool().clone())
        .list_by_playlist(playlist.id)
        .await
        .unwrap();
    assert_eq!(members.len(), 1, "校验失败不应清空已有成员");
}

#[tokio::test]
async fn a_blank_movie_number_is_a_programmer_error() {
    // 上游 `set_playlist_movies` 对空白番号抛 `ValueError`，不是 ApiError。
    let db = TestDb::require().await;
    let svc = PluginCollectionService::new(db.pool());
    let key = format!("blank{}", n());
    svc.ensure_playlist("plugin-a", &key, "列表", None)
        .await
        .unwrap();

    let err = svc
        .set_playlist_movies("plugin-a", &key, &["   ".to_owned()])
        .await
        .expect_err("空白番号应被拒");
    assert_error(&err, 500, "programmer_error");
    assert!(
        err.api.message.contains("movie_number"),
        "{}",
        err.api.message
    );
}

// ================================================================ moment / clip

#[tokio::test]
async fn plugin_owned_moment_and_clip_follow_the_same_shape() {
    let db = TestDb::require().await;
    let svc = PluginCollectionService::new(db.pool());
    let mkey = format!("m{}", n());
    let ckey = format!("c{}", n());

    let moment = svc
        .ensure_moment("plugin-a", &mkey, "时刻合集", None)
        .await
        .unwrap();
    assert!(moment.is_plugin_owned());
    let clip = svc
        .ensure_clip("plugin-a", &ckey, "片段合集", None)
        .await
        .unwrap();
    assert!(clip.is_plugin_owned());

    // set_* 同样要求已存在。
    let err = svc
        .set_moment_points("plugin-a", &format!("nope{}", n()), &[1])
        .await
        .expect_err("");
    assert_error(&err, 404, "plugin_collection_not_found");

    let err = svc
        .set_clip_clips("plugin-a", &format!("nope{}", n()), &[1])
        .await
        .expect_err("");
    assert_error(&err, 404, "plugin_collection_not_found");

    // 存在时可用（用空列表，不依赖真实成员）。
    svc.set_moment_points("plugin-a", &mkey, &[]).await.unwrap();
    svc.set_clip_clips("plugin-a", &ckey, &[]).await.unwrap();
}
