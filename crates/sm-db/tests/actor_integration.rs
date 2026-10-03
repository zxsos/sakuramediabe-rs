//! `ActorRepository` 的集成测试。
//!
//! 重点是墓碑链。演员不被删除，合并把来源行标记为墓碑，
//! 所以 `merged_into_id` 构成一个有向图，而 `resolve_canonical`
//! 必须扛住三种数据形状：
//!
//! - 正常链：返回终点与途经 id，调用方据此重指向中间墓碑
//! - 成环：返回 `None`。返回任意一行会把资料写到错误的人身上
//! - 断链：返回 `None`，理由相同，且静默返回半途的行更难发现
//!
//! 这三件事静态检查证明不了 —— 类型对、SQL 合法、编译通过，
//! 只有真的建行、真的走一遍才看得见。

use sm_db::common::page::PageRequest;
use sm_db::repo::{ActorRepository, NewActor};
use sm_db::testing::TestDb;

/// 建一位演员，返回 id。
async fn seed(repo: &ActorRepository, javdb_id: &str, name: &str) -> i32 {
    repo.insert(&NewActor {
        javdb_id: Some(javdb_id.to_owned()),
        name: name.to_owned(),
    })
    .await
    .expect("insert actor")
    .id
}

/// 直接写 `merged_into_id`，模拟未经合并流程的历史数据。
async fn point_at(db: &TestDb, id: i32, target: i32) {
    sqlx::query("UPDATE actor SET merged_into_id = $2 WHERE id = $1")
        .bind(id)
        .bind(target)
        .execute(db.pool())
        .await
        .expect("set merged_into_id");
}

#[tokio::test]
async fn insert_normalises_blank_javdb_id_and_trims_name() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());

    let blank = repo
        .insert(&NewActor {
            javdb_id: Some("   ".to_owned()),
            name: "演员甲".to_owned(),
        })
        .await
        .unwrap();
    assert!(
        blank.javdb_id.is_none(),
        "空白 javdb_id 应归一为 NULL，否则 `WHERE javdb_id = ''` 命中假记录"
    );

    let padded = repo
        .insert(&NewActor {
            javdb_id: Some("  ABC  ".to_owned()),
            name: "  演员乙 ".to_owned(),
        })
        .await
        .unwrap();
    assert_eq!(padded.name, "演员乙", "name 被 trim");
    assert_eq!(padded.alias_name, "演员乙", "alias_name 初始等于 name");
    assert_eq!(padded.javdb_id.as_deref(), Some("ABC"));
    assert!(!padded.is_subscribed);
    assert!(padded.merged_into_id.is_none());
}

#[tokio::test]
async fn find_by_javdb_id_is_the_stable_key() {
    // name 会被合并改写，所以外部同步只能靠 javdb_id 定位。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());
    let id = seed(&repo, "JAVDB-1", "演员甲").await;

    assert_eq!(
        repo.find_by_javdb_id("JAVDB-1").await.unwrap().unwrap().id,
        id
    );
    assert!(
        repo.find_by_javdb_id("  JAVDB-1  ")
            .await
            .unwrap()
            .is_some(),
        "空白应被 trim"
    );
    assert!(repo.find_by_javdb_id("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn find_by_name_matches_aliases_too() {
    // 合并把来源名并进 alias_name —— 搜来源名必须还能命中，
    // 否则一次合并会让人「消失」。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());
    let id = seed(&repo, "J-1", "空").await;

    sqlx::query("UPDATE actor SET alias_name = $2 WHERE id = $1")
        .bind(id)
        .bind("空 / 苍井空 / Aoi")
        .execute(db.pool())
        .await
        .unwrap();

    for needle in ["空", "苍井空", "Aoi"] {
        assert_eq!(
            repo.find_by_name(needle).await.unwrap().unwrap().id,
            id,
            "别名 `{needle}` 应可搜"
        );
    }
    assert!(repo.find_by_name("不存在").await.unwrap().is_none());
}

#[tokio::test]
async fn resolve_canonical_walks_the_chain_and_reports_it() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());

    let a = seed(&repo, "A", "甲").await;
    let b = seed(&repo, "B", "乙").await;
    let c = seed(&repo, "C", "丙").await;
    point_at(&db, a, b).await;
    point_at(&db, b, c).await;

    let (canonical, chain) = repo.resolve_canonical(a).await.unwrap().unwrap();
    assert_eq!(canonical.id, c, "应走到链的终点");
    assert_eq!(chain, vec![a, b], "途经的两个中间节点");
    assert!(canonical.merged_into_id.is_none(), "终点自己不是墓碑");
}

#[tokio::test]
async fn resolve_canonical_stops_on_a_cycle() {
    // 成环必须终止，且不能返回任意一行 —— 那会把资料写到错误的演员上。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());

    let a = seed(&repo, "A", "甲").await;
    let b = seed(&repo, "B", "乙").await;
    point_at(&db, a, b).await;
    point_at(&db, b, a).await;

    assert!(
        repo.resolve_canonical(a).await.unwrap().is_none(),
        "成环时必须返回 None，不能返回任意一行，也不能死循环"
    );
}

#[tokio::test]
async fn resolve_canonical_stops_on_a_broken_chain() {
    // 指针指向不存在的行：数据损坏或并发删库。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());

    let a = seed(&repo, "A", "甲").await;
    point_at(&db, a, 999_999).await;

    assert!(
        repo.resolve_canonical(a).await.unwrap().is_none(),
        "断链应返回 None，不能返回半途的行"
    );
}

#[tokio::test]
async fn resolve_canonical_of_a_missing_actor_is_none() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());
    assert!(repo.resolve_canonical(999_999).await.unwrap().is_none());
}

#[tokio::test]
async fn merge_marks_sources_as_tombstones_and_clears_subscription() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());

    let target = seed(&repo, "T", "保留").await;
    let source = seed(&repo, "S", "来源").await;
    repo.set_subscribed(source, true).await.unwrap();

    let marked = repo.mark_merged(&[source], target).await.unwrap();
    assert_eq!(marked, 1);

    let after = repo.require_by_id(source).await.unwrap();
    assert_eq!(after.merged_into_id, Some(target));
    assert!(
        !after.is_subscribed,
        "墓碑必须清掉订阅，否则订阅列表出现重复"
    );
    assert!(after.subscribed_at.is_none());

    assert!(
        repo.require_by_id(target)
            .await
            .unwrap()
            .merged_into_id
            .is_none(),
        "保留记录不受影响"
    );
}

#[tokio::test]
async fn mark_merged_is_idempotent() {
    // 重复合并同一来源不应改写它已指向的墓碑。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());

    let t1 = seed(&repo, "T1", "保留一").await;
    let t2 = seed(&repo, "T2", "保留二").await;
    let source = seed(&repo, "S", "来源").await;

    repo.mark_merged(&[source], t1).await.unwrap();
    let second = repo.mark_merged(&[source], t2).await.unwrap();
    assert_eq!(second, 0, "已是墓碑的行不应被改写");
    assert_eq!(
        repo.require_by_id(source).await.unwrap().merged_into_id,
        Some(t1),
        "首次合并的结果保持不变"
    );
}

#[tokio::test]
async fn redirect_flattens_the_chain() {
    // A 合并进 B 后，若还有 C 指向 B，必须一并改成指向 A：
    // 否则链多一跳，且 B 若被清理就成了悬空指针。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());

    let a = seed(&repo, "A", "甲").await;
    let b = seed(&repo, "B", "乙").await;
    let c = seed(&repo, "C", "丙").await;

    repo.mark_merged(&[b], a).await.unwrap();
    point_at(&db, c, b).await;

    let redirected = repo.redirect_tombstones(&[b], a).await.unwrap();
    assert_eq!(redirected, 1, "指向 B 的墓碑应被重指向 A");

    assert_eq!(repo.require_by_id(c).await.unwrap().merged_into_id, Some(a));
    let (canonical, chain) = repo.resolve_canonical(c).await.unwrap().unwrap();
    assert_eq!(canonical.id, a);
    assert_eq!(chain, vec![c], "只途经自己，说明链已被压平");
}

#[tokio::test]
async fn set_subscribed_keeps_the_earliest_timestamp() {
    // 重复订阅不刷新时间 —— 它回答的是「首次订阅于何时」。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());
    let id = seed(&repo, "S", "演员").await;

    let first = repo.set_subscribed(id, true).await.unwrap();
    let first_at = first.subscribed_at.expect("首次订阅应有时间");

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let again = repo.set_subscribed(id, true).await.unwrap();
    assert_eq!(again.subscribed_at, Some(first_at), "重复订阅不刷新时间");

    let off = repo.set_subscribed(id, false).await.unwrap();
    assert!(!off.is_subscribed);
    assert!(off.subscribed_at.is_none(), "退订清空时间");
}

#[tokio::test]
async fn list_excludes_tombstones_unless_asked() {
    // 墓碑不再是独立实体，混进列表会让同一演算出多行。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());

    let keep = seed(&repo, "K", "保留").await;
    let gone = seed(&repo, "G", "已成墓碑").await;
    repo.mark_merged(&[gone], keep).await.unwrap();

    let active = repo
        .list(false, PageRequest::new(1, 50).unwrap())
        .await
        .unwrap();
    assert_eq!(active.items.len(), 1, "默认只列活跃演员");
    assert_eq!(active.total, 1);

    let all = repo
        .list(true, PageRequest::new(1, 50).unwrap())
        .await
        .unwrap();
    assert_eq!(all.items.len(), 2, "显式要求时墓碑也出现");
    assert_eq!(all.total, 2);
}

#[tokio::test]
async fn invalidate_full_sync_forces_the_next_run_to_rebuild() {
    // 合并后必须作废全量同步时间，否则来源 ID 的历史影片补不齐。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());
    let id = seed(&repo, "S", "演员").await;

    sqlx::query("UPDATE actor SET subscribed_movies_full_synced_at = $2 WHERE id = $1")
        .bind(id)
        .bind(sm_db::common::time::now_utc())
        .execute(db.pool())
        .await
        .unwrap();

    let after = repo.invalidate_full_sync(id).await.unwrap();
    assert!(
        after.subscribed_movies_full_synced_at.is_none(),
        "作废后必须为 NULL，否则同步任务会以为已经全量过"
    );
}

#[tokio::test]
async fn sync_state_projection_avoids_pulling_the_whole_row() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = ActorRepository::new(db.pool().clone());
    let id = seed(&repo, "S", "演员").await;
    repo.set_subscribed(id, true).await.unwrap();

    let state = repo.load_sync_state(id).await.unwrap().unwrap();
    assert!(state.is_subscribed);
    assert!(state.subscribed_at.is_some());
    assert!(
        state.subscribed_movies_full_synced_at.is_none(),
        "从未全量同步过"
    );
    assert!(repo.load_sync_state(999_999).await.unwrap().is_none());
}
