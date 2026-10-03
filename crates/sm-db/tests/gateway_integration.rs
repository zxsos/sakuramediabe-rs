//! `MovieOwnershipGateway` 的集成测试。
//!
//! 四个写入口的占位符编号各不相同（字段值 / owner 条件 / 三轮 CASE /
//! jsonb 重建），而这类错误**编译能过、单元测试测不到、只在真实 PG 上
//! 才暴露**。本文件把它们全部钉在真实数据库上。
//!
//! 覆盖的语义（对应上游 `movie_ownership_gateway.py`）：
//!
//! | 场景 | 断言 |
//! |---|---|
//! | revision CAS | 过期 revision 必须整次零修改 |
//! | 字段级 owner 条件 | 一个字段被接管时，**其它字段也要能写** |
//! | 屏蔽已订阅 | `is_blacklisted` 补丁在已订阅影片上返回 false |
//! | NULL-safe 变化检测 | 写入等值不递增 revision、不刷新 updated_at |
//! | 人工覆盖 | `host:manual` 可压过插件 owner |
//! | 释放 owner | 只摘自己的 key，不误伤他人 |

use sm_db::repo::gateway::{FieldPatch, MovieOwnershipGateway};
use sm_db::repo::MovieRepository;
use sm_db::testing::TestDb;

mod helpers {
    use sm_db::repo::gateway::FieldPatch;
    use sm_db::repo::NewMovie;

    pub fn movie(number: &str) -> NewMovie {
        NewMovie {
            movie_number: number.to_owned(),
            title: format!("{number} 标题"),
            ..NewMovie::default()
        }
    }

    /// 只改 title 的补丁。
    pub fn title_patch(value: &str) -> FieldPatch {
        let mut p = FieldPatch::new();
        p.text("title", Some(value));
        p
    }

    /// title + summary 两字段补丁（用于验证字段级独立判定）。
    pub fn two_field_patch(title: &str, summary: &str) -> FieldPatch {
        let mut p = FieldPatch::new();
        p.text("title", Some(title));
        p.text("summary", Some(summary));
        p
    }

    pub fn blacklist_patch(value: bool) -> FieldPatch {
        let mut p = FieldPatch::new();
        p.flag("is_blacklisted", value);
        p
    }
}

use helpers::{blacklist_patch, movie, title_patch, two_field_patch};

/// 直接读列，绕开仓储以确认「数据库里到底是什么」。
async fn raw_field(pool: &sqlx::PgPool, column: &str, id: i32) -> String {
    // column 来自本文件的字面量，不接受外部输入。
    let sql = sqlx::AssertSqlSafe(format!("SELECT {column}::text FROM movie WHERE id = $1"));
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("raw field read")
}

async fn raw_owners(pool: &sqlx::PgPool, id: i32) -> serde_json::Value {
    sqlx::query_scalar("SELECT field_owners FROM movie WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("field_owners read")
}

async fn raw_revision(pool: &sqlx::PgPool, id: i32) -> i64 {
    sqlx::query_scalar("SELECT mutation_revision FROM movie WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("mutation_revision read")
}

// ------------------------------------------------------------------ patch_plugin

#[tokio::test]
async fn patch_plugin_takes_ownership_and_bumps_revision() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());

    let m = repo.insert(&movie("GW-001")).await.unwrap();
    assert_eq!(m.mutation_revision, 0);

    let hit = gw
        .patch_plugin(m.id, "actor-metadata", &title_patch("插件标题"), 0)
        .await
        .expect("patch failed");

    assert!(hit, "revision 匹配时必须命中");
    assert_eq!(raw_field(db.pool(), "title", m.id).await, "插件标题");
    assert_eq!(
        raw_owners(db.pool(), m.id).await["title"],
        "plugin:actor-metadata"
    );
    assert_eq!(raw_revision(db.pool(), m.id).await, 1);
}

#[tokio::test]
async fn patch_plugin_rejects_stale_revision_with_zero_modification() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-002")).await.unwrap();

    // revision 传 99，实际是 0
    let hit = gw
        .patch_plugin(m.id, "p", &title_patch("不该写入"), 99)
        .await
        .expect("patch should not error");

    assert!(!hit, "过期 revision 必须返回 false");
    // 整次零修改：值、owner、revision 都不能变
    assert_eq!(raw_field(db.pool(), "title", m.id).await, "GW-002 标题");
    assert!(raw_owners(db.pool(), m.id).await.get("title").is_none());
    assert_eq!(raw_revision(db.pool(), m.id).await, 0);
}

#[tokio::test]
async fn patch_plugin_cannot_steal_a_field_held_by_another_plugin() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-003")).await.unwrap();

    // plugin-a 先接管 title
    assert!(gw
        .patch_plugin(m.id, "plugin-a", &title_patch("A 的标题"), 0)
        .await
        .unwrap());

    // plugin-b 拿着新 revision 想改同一字段 —— 必须被拒
    let rev = raw_revision(db.pool(), m.id).await;
    let hit = gw
        .patch_plugin(m.id, "plugin-b", &title_patch("B 的标题"), rev)
        .await
        .unwrap();

    assert!(!hit, "已被他人接管的字段不能被抢");
    assert_eq!(raw_field(db.pool(), "title", m.id).await, "A 的标题");
    assert_eq!(
        raw_owners(db.pool(), m.id).await["title"],
        "plugin:plugin-a"
    );
}

#[tokio::test]
async fn patch_plugin_fails_when_blacklisting_a_subscribed_movie() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-004")).await.unwrap();

    // 先订阅（走普通 update，is_subscribed 不是受保护字段）
    let mut set = sm_db::common::update::UpdateSet::new();
    set.set("is_subscribed", true);
    repo.update(m.id, set, sm_db::common::guard::WriteSource::Host)
        .await
        .unwrap();

    // 再尝试屏蔽 —— CHECK 与网关条件都会拒绝
    let rev = raw_revision(db.pool(), m.id).await;
    let hit = gw
        .patch_plugin(m.id, "p", &blacklist_patch(true), rev)
        .await
        .unwrap();

    assert!(!hit, "已订阅影片不能被屏蔽");
    assert_eq!(raw_field(db.pool(), "is_blacklisted", m.id).await, "false");
}

#[tokio::test]
async fn patch_plugin_allows_unowned_field_while_another_is_held() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-005")).await.unwrap();

    // plugin-a 接管 title
    assert!(gw
        .patch_plugin(m.id, "plugin-a", &title_patch("A"), 0)
        .await
        .unwrap());

    // plugin-b 改 summary —— summary 没被接管，应该成功
    let mut summary_only = FieldPatch::new();
    summary_only.text("summary", Some("B 的摘要"));
    let rev = raw_revision(db.pool(), m.id).await;
    let hit = gw
        .patch_plugin(m.id, "plugin-b", &summary_only, rev)
        .await
        .unwrap();

    assert!(hit, "未被接管的字段可以写");
    assert_eq!(raw_field(db.pool(), "summary", m.id).await, "B 的摘要");
    // title 的 owner 不能被顺手改掉
    assert_eq!(
        raw_owners(db.pool(), m.id).await["title"],
        "plugin:plugin-a"
    );
    assert_eq!(
        raw_owners(db.pool(), m.id).await["summary"],
        "plugin:plugin-b"
    );
}

// ------------------------------------------------------- update_host_unowned

#[tokio::test]
async fn host_unowned_writes_only_unowned_fields() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-006")).await.unwrap();

    // 插件接管 title
    assert!(gw
        .patch_plugin(m.id, "plugin-a", &title_patch("插件版"), 0)
        .await
        .unwrap());

    // 宿主同时改 title + summary：title 应保留插件值，summary 应被写入
    let affected = gw
        .update_host_unowned(m.id, &two_field_patch("宿主版", "宿主摘要"))
        .await
        .expect("host update failed");

    assert_eq!(affected, 1);
    assert_eq!(
        raw_field(db.pool(), "title", m.id).await,
        "插件版",
        "已被接管的字段必须保留插件值"
    );
    assert_eq!(raw_field(db.pool(), "summary", m.id).await, "宿主摘要");
}

#[tokio::test]
async fn host_unowned_does_not_bump_revision_when_nothing_changes() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-007")).await.unwrap();

    // 写入与现值完全相同的内容
    let same = title_patch("GW-007 标题");
    gw.update_host_unowned(m.id, &same)
        .await
        .expect("update failed");

    assert_eq!(
        raw_revision(db.pool(), m.id).await,
        0,
        "值没变就不该递增 revision"
    );
}

#[tokio::test]
async fn host_unowned_bumps_revision_once_per_changed_field() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-008")).await.unwrap();

    gw.update_host_unowned(m.id, &two_field_patch("新标题", "新摘要"))
        .await
        .unwrap();

    assert_eq!(
        raw_revision(db.pool(), m.id).await,
        2,
        "两个字段都变了就该 +2"
    );

    // 只改一个字段 -> +1
    gw.update_host_unowned(m.id, &title_patch("再改"))
        .await
        .unwrap();
    assert_eq!(raw_revision(db.pool(), m.id).await, 3);
}

#[tokio::test]
async fn host_unowned_allows_null_for_text_fields() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-009")).await.unwrap();

    // maker_name 允许 NULL：远端详情缺失是合法数据
    let mut patch = FieldPatch::new();
    patch.text("maker_name", None);
    let affected = gw.update_host_unowned(m.id, &patch).await.unwrap();

    assert_eq!(affected, 1);
    assert!(
        sqlx::query_scalar::<_, Option<String>>("SELECT maker_name FROM movie WHERE id = $1")
            .bind(m.id)
            .fetch_one(db.pool())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn host_unowned_rejects_unprotected_field() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let gw = MovieOwnershipGateway::new(db.pool().clone());

    // watched_count 不在白名单里
    let mut bad = FieldPatch::new();
    bad.text("watched_count", Some("x"));
    let err = gw.update_host_unowned(1, &bad).await.unwrap_err();

    assert!(err.to_string().contains("watched_count"), "{err}");
}

// ------------------------------------------------------- update_host_manual

#[tokio::test]
async fn host_manual_overrides_plugin_owner() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-010")).await.unwrap();

    assert!(gw
        .patch_plugin(m.id, "plugin-a", &title_patch("插件版"), 0)
        .await
        .unwrap());

    // 人工写：特权入口，允许覆盖插件 owner
    let affected = gw
        .update_host_manual(&[m.id], &title_patch("人工版"))
        .await
        .unwrap();

    assert_eq!(affected, 1);
    assert_eq!(raw_field(db.pool(), "title", m.id).await, "人工版");
    assert_eq!(
        raw_owners(db.pool(), m.id).await["title"],
        "host:manual",
        "人工写应把 owner 改成 host:manual"
    );
}

#[tokio::test]
async fn host_manual_updates_many_ids_at_once() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());

    let a = repo.insert(&movie("GW-011")).await.unwrap();
    let b = repo.insert(&movie("GW-012")).await.unwrap();
    let c = repo.insert(&movie("GW-013")).await.unwrap();

    let affected = gw
        .update_host_manual(&[a.id, b.id, c.id], &blacklist_patch(true))
        .await
        .unwrap();

    assert_eq!(affected, 3);
    for id in [a.id, b.id, c.id] {
        assert_eq!(raw_field(db.pool(), "is_blacklisted", id).await, "true");
    }
}

#[tokio::test]
async fn host_manual_with_empty_ids_short_circuits() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let gw = MovieOwnershipGateway::new(db.pool().clone());

    // 空列表必须返回 0 且不发 SQL
    let affected = gw.update_host_manual(&[], &title_patch("x")).await.unwrap();
    assert_eq!(affected, 0);
}

// --------------------------------------------------- release_plugin_owners

#[tokio::test]
async fn release_removes_only_the_target_plugins_keys() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-014")).await.unwrap();

    // plugin-a 接管 title，plugin-b 接管 summary
    assert!(gw
        .patch_plugin(m.id, "plugin-a", &title_patch("A"), 0)
        .await
        .unwrap());
    let mut summary = FieldPatch::new();
    summary.text("summary", Some("B"));
    let rev = raw_revision(db.pool(), m.id).await;
    assert!(gw
        .patch_plugin(m.id, "plugin-b", &summary, rev)
        .await
        .unwrap());

    // 只释放 plugin-a
    let affected = gw
        .release_plugin_owners("plugin-a", Some(&["title"]))
        .await
        .unwrap();
    assert_eq!(affected, 1);

    let owners = raw_owners(db.pool(), m.id).await;
    assert!(owners.get("title").is_none(), "目标 key 应被摘除：{owners}");
    assert_eq!(
        owners["summary"], "plugin:plugin-b",
        "不能误摘他人接管的字段：{owners}"
    );
}

#[tokio::test]
async fn release_all_clears_every_key_owned_by_that_plugin() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-015")).await.unwrap();

    // 同一插件接管两个字段
    assert!(gw
        .patch_plugin(m.id, "p1", &two_field_patch("T", "S"), 0)
        .await
        .unwrap());

    let affected = gw.release_plugin_owners("p1", None).await.unwrap();
    assert_eq!(affected, 1);

    let owners = raw_owners(db.pool(), m.id).await;
    assert_eq!(owners.as_object().map(|o| o.len()), Some(0), "{owners}");
}

#[tokio::test]
async fn release_leaves_field_values_and_revision_untouched() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-016")).await.unwrap();

    assert!(gw
        .patch_plugin(m.id, "p1", &title_patch("保留的值"), 0)
        .await
        .unwrap());
    let rev_before = raw_revision(db.pool(), m.id).await;

    gw.release_plugin_owners("p1", Some(&["title"]))
        .await
        .unwrap();

    assert_eq!(
        raw_field(db.pool(), "title", m.id).await,
        "保留的值",
        "释放 owner 不该动字段值"
    );
    assert_eq!(
        raw_revision(db.pool(), m.id).await,
        rev_before,
        "释放 owner 不该动 revision"
    );
}

#[tokio::test]
async fn release_rejects_unprotected_field_list() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let gw = MovieOwnershipGateway::new(db.pool().clone());

    let err = gw
        .release_plugin_owners("p1", Some(&["watched_count"]))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("watched_count"), "{err}");
}

// ------------------------------------------------------------ 组合场景

#[tokio::test]
async fn full_lifecycle_plugin_then_host_then_release() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = MovieRepository::new(db.pool().clone());
    let gw = MovieOwnershipGateway::new(db.pool().clone());
    let m = repo.insert(&movie("GW-017")).await.unwrap();

    // 1) 插件接管
    assert!(gw
        .patch_plugin(m.id, "p1", &title_patch("v1"), 0)
        .await
        .unwrap());
    assert_eq!(raw_owners(db.pool(), m.id).await["title"], "plugin:p1");

    // 2) 宿主自动写被挡住
    gw.update_host_unowned(m.id, &title_patch("v2"))
        .await
        .unwrap();
    assert_eq!(raw_field(db.pool(), "title", m.id).await, "v1");

    // 3) 人工覆盖成功
    gw.update_host_manual(&[m.id], &title_patch("v3"))
        .await
        .unwrap();
    assert_eq!(raw_field(db.pool(), "title", m.id).await, "v3");
    assert_eq!(raw_owners(db.pool(), m.id).await["title"], "host:manual");

    // 4) 宿主自动写继续被挡（host:manual 也算被接管）
    gw.update_host_unowned(m.id, &title_patch("v4"))
        .await
        .unwrap();
    assert_eq!(raw_field(db.pool(), "title", m.id).await, "v3");

    // 5) 释放后宿主可写
    gw.release_plugin_owners("p1", None).await.unwrap();
    gw.update_host_unowned(m.id, &title_patch("v5"))
        .await
        .unwrap();
    assert_eq!(raw_field(db.pool(), "title", m.id).await, "v5");
}
