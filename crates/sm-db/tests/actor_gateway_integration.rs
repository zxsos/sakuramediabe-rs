//! `ActorOwnershipGateway` 的集成测试。
//!
//! 与 [`gateway_integration`]（影片侧）同构。为什么必须有这一份：
//!
//! - 三条入口的占位符编号各不相同（`patch_plugin` 是 3n+4，
//!   `update_host_source` 是 7n+2，释放是两轮 owner+字段名），而这类错误
//!   **编译能过、单测测不到、只在真实 PG 上才暴露**；
//! - `update_host_source` 的 `<>` 与 `IS DISTINCT FROM` 分工（NULL 语义不同）
//!   只有真库能确认；
//! - 演员侧的 `release_plugin_owners` **推进 revision**（影片侧不动），
//!   这条差异也要钉住。
//!
//! 覆盖的语义（对应上游 `actor_ownership_gateway.py`）：
//!
//! | 场景 | 断言 |
//! |---|---|
//! | revision CAS | 过期 revision 整次零修改；负 revision 直接报错 |
//! | 字段级 owner 条件 | 一个字段被接管时，其它字段仍能写 |
//! | `None` 显式清空 | 清空后**保留归属** |
//! | 取值域 | `gender` 只认 1/2；身高必须正整数 |
//! | JavDB 补录 | 可顶掉插件 owner，**顶不掉人工** |
//! | 释放 owner | 只摘自己的 key；推进 revision |

use sm_db::repo::gateway::{
    parse_iso_date_exact, ActorOwnershipGateway, FieldPatch, HOST_JAVDB_OWNER,
};
use sm_db::repo::ActorRepository;
use sm_db::testing::TestDb;

mod helpers {
    use sm_db::repo::gateway::FieldPatch;
    use sm_db::repo::NewActor;

    /// 唯一的 `javdb_id`：`actor.javdb_id` 上有 unique 索引，同一测试进程里
    /// 复用同一个号会撞唯一约束（而报错信息指向 `javdb_id`，看着像代码问题）。
    pub fn actor(unique: &str) -> NewActor {
        NewActor {
            javdb_id: format!("javdb-{unique}"),
            name: format!("演员 {unique}"),
        }
    }

    pub fn height_patch(value: Option<i32>) -> FieldPatch {
        let mut p = FieldPatch::new();
        p.int("height_cm", value);
        p
    }

    pub fn gender_patch(value: i32) -> FieldPatch {
        let mut p = FieldPatch::new();
        p.int("gender", Some(value));
        p
    }

    /// 身高 + 杯号两字段补丁（用于验证字段级独立判定）。
    pub fn two_field_patch(height: i32, cup: &str) -> FieldPatch {
        let mut p = FieldPatch::new();
        p.int("height_cm", Some(height));
        p.text("cup", Some(cup));
        p
    }
}

use helpers::{actor, gender_patch, height_patch, two_field_patch};

/// 唯一后缀：同一进程里多次建演员，`javdb_id` 不能重。
fn unique() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static C: AtomicU32 = AtomicU32::new(0);
    let n = C.fetch_add(1, Ordering::Relaxed);
    format!("{}-{n}", std::process::id())
}

/// 直接读列，绕开仓储以确认「数据库里到底是什么」。
async fn raw_text(pool: &sqlx::PgPool, column: &str, id: i32) -> Option<String> {
    // column 来自本文件的字面量，不接受外部输入。
    let sql = sqlx::AssertSqlSafe(format!("SELECT {column}::text FROM actor WHERE id = $1"));
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("raw field read")
}

async fn raw_owners(pool: &sqlx::PgPool, id: i32) -> serde_json::Value {
    sqlx::query_scalar("SELECT field_owners FROM actor WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("field_owners read")
}

async fn raw_revision(pool: &sqlx::PgPool, id: i32) -> i64 {
    sqlx::query_scalar("SELECT mutation_revision FROM actor WHERE id = $1")
        .bind(id)
        .fetch_one(pool)
        .await
        .expect("mutation_revision read")
}

// ----------------------------------------------------------- patch_plugin

#[tokio::test]
async fn patch_plugin_takes_ownership_and_bumps_revision() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());

    let a = repo.insert(&actor(&unique())).await.unwrap();
    assert_eq!(a.mutation_revision, 0);

    let hit = gw
        .patch_plugin(a.id, "actor-metadata", &height_patch(Some(160)), 0)
        .await
        .expect("patch failed");

    assert!(hit, "revision 匹配时必须命中");
    assert_eq!(raw_text(db.pool(), "height_cm", a.id).await.unwrap(), "160");
    assert_eq!(
        raw_owners(db.pool(), a.id).await["height_cm"],
        "plugin:actor-metadata"
    );
    assert_eq!(raw_revision(db.pool(), a.id).await, 1);
}

#[tokio::test]
async fn patch_plugin_rejects_a_stale_revision_with_zero_modification() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    let hit = gw
        .patch_plugin(a.id, "p", &height_patch(Some(170)), 99)
        .await
        .expect("过期 revision 不是错误");

    assert!(!hit, "过期 revision 必须返回 false");
    assert_eq!(raw_text(db.pool(), "height_cm", a.id).await, None);
    assert!(raw_owners(db.pool(), a.id).await.get("height_cm").is_none());
    assert_eq!(raw_revision(db.pool(), a.id).await, 0);
}

/// 一个字段被别的插件接管，**不能**连累同一批里其它字段。
#[tokio::test]
async fn patch_plugin_is_decided_per_field() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    // plugin-a 接管 height_cm
    assert!(gw
        .patch_plugin(a.id, "a", &height_patch(Some(155)), 0)
        .await
        .unwrap());

    // plugin-b 想把 height_cm 与 cup 一起写：整次零修改（含没被接管的 cup）
    let hit = gw
        .patch_plugin(a.id, "b", &two_field_patch(180, "C"), 1)
        .await
        .unwrap();
    assert!(!hit, "有字段被抢时整次零修改");
    assert_eq!(raw_text(db.pool(), "height_cm", a.id).await.unwrap(), "155");
    assert_eq!(
        raw_text(db.pool(), "cup", a.id).await,
        None,
        "同批的 cup 也不能写"
    );
    assert_eq!(raw_revision(db.pool(), a.id).await, 1, "revision 不动");

    // 只写没被接管的 cup —— 应该成功
    let mut cup_only = FieldPatch::new();
    cup_only.text("cup", Some("C"));
    assert!(gw.patch_plugin(a.id, "b", &cup_only, 1).await.unwrap());
    assert_eq!(raw_text(db.pool(), "cup", a.id).await.unwrap(), "C");
}

/// ★ 演员侧允许 `None`：**显式清空并保留归属**（影片侧插件路径拒绝 `None`）。
#[tokio::test]
async fn patch_plugin_can_clear_a_field_while_keeping_ownership() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    assert!(gw
        .patch_plugin(a.id, "p", &height_patch(Some(160)), 0)
        .await
        .unwrap());
    // 同一个插件再写 NULL
    assert!(gw
        .patch_plugin(a.id, "p", &height_patch(None), 1)
        .await
        .unwrap());

    assert_eq!(raw_text(db.pool(), "height_cm", a.id).await, None, "已清空");
    assert_eq!(
        raw_owners(db.pool(), a.id).await["height_cm"],
        "plugin:p",
        "清空值不等于放弃归属"
    );
}

/// `birthday` 走真库往返：写进去的是 `date`，读出来 `::text` 是严格 ISO。
#[tokio::test]
async fn patch_plugin_round_trips_a_date() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    let mut patch = FieldPatch::new();
    let birthday = parse_iso_date_exact("1990-07-04").expect("严格 ISO");
    patch.date("birthday", Some(birthday));

    assert!(gw.patch_plugin(a.id, "p", &patch, 0).await.unwrap());
    // `::text` 读回来必须是同一个 ISO 串（库列是 `date`，不会带时间）。
    assert_eq!(
        raw_text(db.pool(), "birthday", a.id).await.unwrap(),
        "1990-07-04"
    );
}

/// `gender` 只认 `{1, 2}`，`0` 与 `3` 都必须在**写入前**被拒（库里有 NOT NULL，
/// 但没有 CHECK —— 靠网关守）。
#[tokio::test]
async fn patch_plugin_enforces_the_gender_domain() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    for bad in [0, 3] {
        assert!(
            gw.patch_plugin(a.id, "p", &gender_patch(bad), 0)
                .await
                .is_err(),
            "gender={bad} 该被拒"
        );
    }
    assert!(gw
        .patch_plugin(a.id, "p", &gender_patch(2), 0)
        .await
        .unwrap());
    assert_eq!(raw_text(db.pool(), "gender", a.id).await.unwrap(), "2");
    assert_eq!(raw_revision(db.pool(), a.id).await, 1, "被拒的那两次没写库");
}

/// 负 revision 是**调用方的 bug**（上游显式拒绝），不是「没命中」。
#[tokio::test]
async fn patch_plugin_rejects_a_negative_revision_outright() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    assert!(gw
        .patch_plugin(a.id, "p", &height_patch(Some(160)), -1)
        .await
        .is_err());
    assert_eq!(raw_text(db.pool(), "height_cm", a.id).await, None);
}

// ------------------------------------------------------- update_host_source

#[tokio::test]
async fn host_source_overwrites_a_plugin_owner() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    assert!(gw
        .patch_plugin(a.id, "p", &height_patch(Some(150)), 0)
        .await
        .unwrap());

    let hit = gw
        .update_host_source(a.id, &height_patch(Some(165)), HOST_JAVDB_OWNER)
        .await
        .unwrap();

    assert!(hit, "宿主权威来源可以顶掉插件");
    assert_eq!(raw_text(db.pool(), "height_cm", a.id).await.unwrap(), "165");
    assert_eq!(
        raw_owners(db.pool(), a.id).await["height_cm"],
        HOST_JAVDB_OWNER
    );
    assert_eq!(raw_revision(db.pool(), a.id).await, 2);
}

/// ★ 人工 owner **顶不掉**：`host:javdb` 的补录遇到 `host:manual` 必须不命中。
///
/// 注：`host:manual` **不能**经本网关写入（上游 `:89` 显式拒绝，演员侧没有
/// 人工写入口）。所以这里直接摆一个已有的 manual owner —— 它模拟的是
/// 合并链路 / 人工编辑留下的标记，而本方法只需要「看见它就不命中」。
#[tokio::test]
async fn host_source_never_overwrites_a_manual_owner() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    sqlx::query(
        "UPDATE actor SET height_cm = 158, \
         field_owners = '{\"height_cm\": \"host:manual\"}'::jsonb, \
         mutation_revision = 3 \
         WHERE id = $1",
    )
    .bind(a.id)
    .execute(db.pool())
    .await
    .expect("摆一个 manual owner");

    let hit = gw
        .update_host_source(a.id, &height_patch(Some(180)), HOST_JAVDB_OWNER)
        .await
        .unwrap();

    assert!(!hit, "人工 owner 不可被自动来源覆盖");
    assert_eq!(raw_text(db.pool(), "height_cm", a.id).await.unwrap(), "158");
    assert_eq!(
        raw_owners(db.pool(), a.id).await["height_cm"],
        "host:manual"
    );
    assert_eq!(raw_revision(db.pool(), a.id).await, 3, "整次零修改");
}

/// owner 参数必须是 `host:*`（且**不是** `host:manual`）。
#[tokio::test]
async fn host_source_rejects_a_non_host_owner() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    for bad in ["plugin:x", "javdb", "", "host:manual"] {
        assert!(
            gw.update_host_source(a.id, &height_patch(Some(160)), bad)
                .await
                .is_err(),
            "owner={bad:?} 该被拒"
        );
    }
}

/// ★ 写入**等值**不算变化：不命中，且 revision / updated_at 都不动。
///
/// 这条覆盖上游的 `changed_conditions`（值或归属任一变了才算变化）——
/// 少了它，每轮补录都会把 revision 推一格，插件手里的快照就永远过期。
#[tokio::test]
async fn host_source_is_a_no_op_when_nothing_changes() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    assert!(gw
        .update_host_source(a.id, &height_patch(Some(160)), HOST_JAVDB_OWNER)
        .await
        .unwrap());
    let revision = raw_revision(db.pool(), a.id).await;

    // 再写同一个值
    let hit = gw
        .update_host_source(a.id, &height_patch(Some(160)), HOST_JAVDB_OWNER)
        .await
        .unwrap();

    assert!(!hit, "值没变就不该命中");
    assert_eq!(raw_revision(db.pool(), a.id).await, revision);
}

// ------------------------------------------------- release_plugin_owners

#[tokio::test]
async fn release_drops_only_the_target_plugins_keys() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    // ⚠️ 第一个参数是**插件 id**，网关自己拼 `plugin:{id}` —— 传 `"plugin-a"`
    // 会得到 owner `plugin:plugin-a`（写错过一次，断言直接把它揪出来了）。
    let mut height = FieldPatch::new();
    height.int("height_cm", Some(160));
    assert!(gw.patch_plugin(a.id, "a", &height, 0).await.unwrap());

    let mut cup = FieldPatch::new();
    cup.text("cup", Some("C"));
    assert!(gw.patch_plugin(a.id, "b", &cup, 1).await.unwrap());

    let revision_before = raw_revision(db.pool(), a.id).await;
    let released = gw
        .release_plugin_owners("a", Some(&["height_cm"]))
        .await
        .unwrap();

    assert_eq!(released, 1, "只该命中那一行");
    let owners = raw_owners(db.pool(), a.id).await;
    assert!(owners.get("height_cm").is_none(), "plugin-a 的 key 被摘掉");
    assert_eq!(owners["cup"], "plugin:b", "别的插件不受影响");
    // ★ 演员侧**推进** revision（影片侧不动）—— 上游如此，别「统一」掉。
    assert_eq!(
        raw_revision(db.pool(), a.id).await,
        revision_before + 1,
        "释放归属要让手里的 snapshot 失效"
    );
}

#[tokio::test]
async fn release_all_of_a_plugin_clears_every_key_it_holds() {
    let db = TestDb::require().await;
    let repo = ActorRepository::new(db.pool().clone());
    let gw = ActorOwnershipGateway::new(db.pool().clone());
    let a = repo.insert(&actor(&unique())).await.unwrap();

    assert!(gw
        .patch_plugin(a.id, "a", &two_field_patch(160, "C"), 0)
        .await
        .unwrap());

    let released = gw.release_plugin_owners("a", None).await.unwrap();
    assert_eq!(released, 1);
    assert_eq!(
        raw_owners(db.pool(), a.id).await,
        serde_json::json!({}),
        "field_owners 应回到空对象（不是 NULL）"
    );
}

#[tokio::test]
async fn release_rejects_an_empty_or_unprotected_field_list() {
    let db = TestDb::require().await;
    let gw = ActorOwnershipGateway::new(db.pool().clone());

    assert!(gw.release_plugin_owners("p", Some(&[])).await.is_err());
    assert!(gw
        .release_plugin_owners("p", Some(&["javdb_id"]))
        .await
        .is_err());
}
