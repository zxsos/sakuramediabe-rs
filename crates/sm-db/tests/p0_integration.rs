//! 两个 P0 仓储的集成测试：认证链路与后台队列。
//!
//! 覆盖的是**静态分析证明不了**的东西：
//!
//! | 验证内容 | 为什么必须连真库 |
//! |---|---|
//! | 轮换的原子性 | 需要 `FOR UPDATE` 的真实行锁行为 |
//! | 并发领取不重复 | 需要 `FOR UPDATE SKIP LOCKED` |
//! | `mutex_key` 终态后复用 | 依赖唯一索引 + NULL 不参与约束 |
//! | 租约回收 | 依赖时间比较与状态机 |
//! | 重放被拒 | 依赖事务内的状态可见性 |
//! | `token_id` 唯一约束 | 依赖真实唯一索引 |
//!
//! 没有数据库时全部跳过（`TestDb::create()` 返回 `None`）。

use sm_db::common::time::now_utc;
use sm_db::error::DbError;
use sm_db::repo::{
    BackgroundTaskRunRepository, NewRefreshToken, NewTaskRun, NewUser, TaskOutcome, TaskProgress,
    UserRefreshTokenRepository, UserRepository,
};
use sm_db::system::user::RefreshTokenStatus;
use sm_db::testing::TestDb;

mod fixtures {
    use super::*;

    pub fn user(name: &str) -> NewUser {
        NewUser {
            username: name.to_owned(),
            // 真实场景是 Argon2 PHC 串；仓储只校验非空，不解析格式。
            password_hash: "$argon2id$v=19$m=1,t=1,p=1$aaaa$bbbb".to_owned(),
        }
    }

    pub fn token(id: &str, hash: &str) -> NewRefreshToken {
        NewRefreshToken {
            token_id: id.to_owned(),
            token_hash: hash.to_owned(),
            expires_at: now_utc() + chrono::Duration::days(30),
            client_ip: Some("10.0.0.1".to_owned()),
            user_agent: Some("test-agent".to_owned()),
        }
    }

    pub fn task(key: &str) -> NewTaskRun {
        NewTaskRun {
            task_key: key.to_owned(),
            task_name: format!("{key} 任务"),
            trigger_type: "manual".to_owned(),
            mutex_key: None,
            params: None,
            scheduled_at: None,
        }
    }

    pub fn task_with_mutex(key: &str, mutex: &str) -> NewTaskRun {
        let mut t = task(key);
        t.mutex_key = Some(mutex.to_owned());
        t
    }
}

// ================================================================ 用户

#[tokio::test]
async fn user_insert_and_lookup_by_username() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRepository::new(db.pool().clone());

    let created = repo.insert(&fixtures::user("account")).await.unwrap();
    assert_eq!(created.username, "account");
    assert!(created.last_login_at.is_none(), "新用户无登录时间");
    assert!(created.created_at.is_some());

    let found = repo.find_by_username("account").await.unwrap().unwrap();
    assert_eq!(found.id, created.id);
    // 空白用户名应被 trim 后匹配
    assert!(repo
        .find_by_username("  account  ")
        .await
        .unwrap()
        .is_some());
    assert!(repo.find_by_username("nope").await.unwrap().is_none());
}

#[tokio::test]
async fn username_uniqueness_is_enforced_by_the_database() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRepository::new(db.pool().clone());

    repo.insert(&fixtures::user("account")).await.unwrap();
    let err = repo
        .insert(&fixtures::user("account"))
        .await
        .expect_err("username 唯一约束应拒绝重复");

    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "应归为 409，实际 {err:?}"
    );
}

#[tokio::test]
async fn primary_user_is_the_lowest_id() {
    // 对应上游 `User.select().order_by(User.id).first()`。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRepository::new(db.pool().clone());

    assert!(repo.find_primary().await.unwrap().is_none(), "空库无主用户");

    let first = repo.insert(&fixtures::user("account")).await.unwrap();
    let second = repo.insert(&fixtures::user("admin")).await.unwrap();

    let primary = repo.find_primary().await.unwrap().unwrap();
    assert_eq!(
        primary.id, first.id,
        "单用户假设取 id 最小者，不是最后插入的"
    );
    assert!(second.id > first.id);
}

#[tokio::test]
async fn last_login_is_touched_through_a_dedicated_method() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::user("account")).await.unwrap();

    let touched = repo.touch_last_login(created.id).await.unwrap();
    assert!(touched.last_login_at.is_some());
    // 重复触碰应推进时间戳
    let again = repo.touch_last_login(created.id).await.unwrap();
    assert!(again.updated_at >= touched.updated_at);
}

#[tokio::test]
async fn set_password_hash_rejects_blank_but_accepts_rehash() {
    // 密码哈希不可经由通用 update 改，所以必须有专用方法。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRepository::new(db.pool().clone());
    let created = repo.insert(&fixtures::user("account")).await.unwrap();

    // 换 hash（bcrypt -> argon2 的无感升级路径）
    let updated = repo
        .set_password_hash(created.id, "$argon2id$v=19$m=2,t=2,p=2$cccc$dddd")
        .await
        .unwrap();
    assert_ne!(updated.password_hash, created.password_hash);

    // 空白被拒
    let err = repo
        .set_password_hash(created.id, "   ")
        .await
        .expect_err("空哈希应被拒");
    assert!(matches!(err, DbError::Business { .. }));
}

#[tokio::test]
async fn touch_last_login_on_missing_user_reports_not_found() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRepository::new(db.pool().clone());
    let err = repo.touch_last_login(999_999).await.unwrap_err();
    assert!(matches!(err, DbError::NotFound { .. }), "实际 {err:?}");
}

// ================================================================ 令牌

#[tokio::test]
async fn token_defaults_to_active_from_schema() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    let created = repo.insert(&fixtures::token("t1", "h1")).await.unwrap();
    assert_eq!(created.status, "active", "DEFAULT 'active'");
    assert!(created.can_refresh());
    assert!(!created.is_rotated());
    assert!(created.revoked_at.is_none());
    // 审计列必须保留 —— 旧接口依赖它们
    assert_eq!(created.client_ip.as_deref(), Some("10.0.0.1"));
    assert_eq!(created.user_agent.as_deref(), Some("test-agent"));
}

#[tokio::test]
async fn token_id_is_unique() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    repo.insert(&fixtures::token("t1", "h1")).await.unwrap();
    let err = repo
        .insert(&fixtures::token("t1", "h2"))
        .await
        .expect_err("token_id 唯一约束应拒绝重复");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "应归为 409，实际 {err:?}"
    );
}

#[tokio::test]
async fn rotation_retires_old_and_installs_new_atomically() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    let original = repo.insert(&fixtures::token("t1", "h1")).await.unwrap();
    let now = now_utc();

    let result = repo
        .rotate("t1", "h1", &fixtures::token("t2", "h2"), now)
        .await
        .expect("轮换应成功");

    // 新令牌 active、未轮换
    assert_eq!(result.fresh.token_id, "t2");
    assert_eq!(result.fresh.status, "active");
    assert!(result.fresh.can_refresh());
    assert!(!result.fresh.is_rotated());

    // 旧令牌 revoked、指向新令牌
    assert_eq!(result.retired.id, original.id);
    assert_eq!(result.retired.status, "revoked");
    assert_eq!(result.retired.replaced_by_token_id.as_deref(), Some("t2"));
    assert!(result.retired.revoked_at.is_some());
    assert!(!result.retired.can_refresh(), "旧令牌必须立刻失效");

    // 库里状态一致 —— 不是只在返回值里对
    let reloaded = repo.find_by_token_id("t1").await.unwrap().unwrap();
    assert_eq!(reloaded.status, "revoked");
    let fresh = repo.find_by_token_id("t2").await.unwrap().unwrap();
    assert_eq!(fresh.status, "active");
}

#[tokio::test]
async fn replaying_a_rotated_token_is_rejected() {
    // 安全边界：旧令牌必须在第一次轮换后立刻失效。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    repo.insert(&fixtures::token("t1", "h1")).await.unwrap();
    let now = now_utc();
    repo.rotate("t1", "h1", &fixtures::token("t2", "h2"), now)
        .await
        .unwrap();

    // 重放：拿已轮换的旧令牌再换一次
    let err = repo
        .rotate("t1", "h1", &fixtures::token("t3", "h3"), now)
        .await
        .expect_err("已轮换的令牌不可复用");
    assert!(matches!(err, DbError::Business { .. }), "实际 {err:?}");

    // t3 不该被创建 —— 失败必须不留痕迹
    assert!(
        repo.find_by_token_id("t3").await.unwrap().is_none(),
        "失败的轮换不应插入新行"
    );
}

#[tokio::test]
async fn rotation_rejects_a_mismatched_hash() {
    // 有人拿着合法的 token_id 配错误的令牌 —— 可能是重放，也可能是
    // id 泄露后的探测。两种都拒绝。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    repo.insert(&fixtures::token("t1", "h1")).await.unwrap();
    let now = now_utc();

    let err = repo
        .rotate("t1", "wrong-hash", &fixtures::token("t2", "h2"), now)
        .await
        .expect_err("哈希不匹配必须拒绝");
    assert!(err.to_string().contains("哈希不匹配"), "{err}");

    // 旧令牌必须仍可用 —— 拒绝不应该连累它
    assert!(repo
        .find_by_token_id("t1")
        .await
        .unwrap()
        .unwrap()
        .can_refresh());
}

#[tokio::test]
async fn rotation_marks_an_expired_token_as_expired_not_revoked() {
    // 过期与被吊销必须可区分，否则审计时看不出攻击者用的是哪种。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    let mut expired = fixtures::token("t1", "h1");
    expired.expires_at = now_utc() - chrono::Duration::days(1);
    repo.insert(&expired).await.unwrap();

    let err = repo
        .rotate("t1", "h1", &fixtures::token("t2", "h2"), now_utc())
        .await
        .expect_err("过期令牌不可刷新");
    assert!(err.to_string().contains("过期"), "{err}");

    // 状态被写成 expired
    let reloaded = repo.find_by_token_id("t1").await.unwrap().unwrap();
    assert_eq!(reloaded.status, "expired");
    assert_eq!(reloaded.status, RefreshTokenStatus::Expired.as_str());
    // 过期不是「被吊销」—— 不该写 revoked_at
    assert!(reloaded.revoked_at.is_none());
}

#[tokio::test]
async fn rotation_of_missing_token_reports_not_found() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    let err = repo
        .rotate("nope", "h", &fixtures::token("t2", "h2"), now_utc())
        .await
        .unwrap_err();
    assert!(matches!(err, DbError::NotFound { .. }), "实际 {err:?}");
}

#[tokio::test]
async fn rotation_validates_the_new_token_before_touching_the_old_one() {
    // 参数校验必须发生在任何写入之前。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    repo.insert(&fixtures::token("t1", "h1")).await.unwrap();

    let bad_fresh = NewRefreshToken {
        token_id: "  ".to_owned(),
        token_hash: "h2".to_owned(),
        expires_at: now_utc() + chrono::Duration::days(1),
        client_ip: None,
        user_agent: None,
    };
    let err = repo
        .rotate("t1", "h1", &bad_fresh, now_utc())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("token_id"), "{err}");

    // 旧令牌未被影响
    assert!(repo
        .find_by_token_id("t1")
        .await
        .unwrap()
        .unwrap()
        .can_refresh());
}

#[tokio::test]
async fn concurrent_rotation_of_the_same_token_admits_exactly_one_winner() {
    // 这是 rotate 用事务 + FOR UPDATE 的**全部理由**。
    //
    // 若吊销与插入不在同一事务，两个并发请求会各自看到 status=active，
    // 各自插入一个新令牌 —— 于是 t2 和 t3 同时有效，轮换失效。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    repo.insert(&fixtures::token("t1", "h1")).await.unwrap();
    let now = now_utc();

    // 两个并发轮换，都基于 t1。
    // 新令牌先绑成具名变量 —— `join!` 会把两个 future 存进临时数组，
    // 直接传 `&fixtures::token(..)` 会让借用活不过宏。
    let candidate_a = fixtures::token("t2", "h2");
    let candidate_b = fixtures::token("t3", "h3");
    let (a, b) = tokio::join!(
        repo.rotate("t1", "h1", &candidate_a, now),
        repo.rotate("t1", "h1", &candidate_b, now),
    );

    // 先把两个结果归约成「谁赢了」，再消费 —— 避免 move 后再用。
    let winner_token_id = match (a, b) {
        (Ok(r), Err(_)) => r.fresh.token_id,
        (Err(_), Ok(r)) => r.fresh.token_id,
        (Ok(r1), Ok(r2)) => panic!(
            "两个轮换都成功了，产生两个有效令牌：{} 与 {}",
            r1.fresh.token_id, r2.fresh.token_id
        ),
        (Err(e1), Err(e2)) => panic!("两个轮换都失败了：{e1} / {e2}"),
    };

    // 库里只应多出一个新令牌
    let active = repo.list_active().await.unwrap();
    assert_eq!(
        active.len(),
        1,
        "不应产生两个有效令牌，实际 {}",
        active.len()
    );
    assert_eq!(active[0].token_id, winner_token_id);
    // 落败的那一个新令牌不该存在
    let loser_id = if winner_token_id == "t2" { "t3" } else { "t2" };
    assert!(
        repo.find_by_token_id(loser_id).await.unwrap().is_none(),
        "落败方的新令牌不应被插入"
    );
    // 旧令牌已失效
    assert!(!repo
        .find_by_token_id("t1")
        .await
        .unwrap()
        .unwrap()
        .can_refresh());
}

#[tokio::test]
async fn revoke_leaves_no_replacement() {
    // 登出与轮换的区别：不创建接替者。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    repo.insert(&fixtures::token("t1", "h1")).await.unwrap();
    let revoked = repo.revoke("t1", now_utc()).await.unwrap();

    assert_eq!(revoked.status, "revoked");
    assert!(revoked.revoked_at.is_some());
    assert!(revoked.replaced_by_token_id.is_none(), "登出不创建接替者");
}

#[tokio::test]
async fn revoke_all_active_leaves_revoked_ones_untouched() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    repo.insert(&fixtures::token("t1", "h1")).await.unwrap();
    repo.insert(&fixtures::token("t2", "h2")).await.unwrap();
    repo.revoke("t2", now_utc()).await.unwrap();

    let count = repo.revoke_all_active(now_utc()).await.unwrap();
    assert_eq!(count, 1, "只该吊销仍 active 的那个");

    let all = repo.list_active().await.unwrap();
    assert!(all.is_empty(), "不应再有 active 令牌");
}

#[tokio::test]
async fn purge_expired_removes_by_expiry_regardless_of_status() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    let mut expired = fixtures::token("t-old", "h1");
    expired.expires_at = now_utc() - chrono::Duration::days(1);
    repo.insert(&expired).await.unwrap();

    // 已过期但状态仍是 active 的 —— 过期清理该负责它
    let mut expired_active = fixtures::token("t-old-active", "h2");
    expired_active.expires_at = now_utc() - chrono::Duration::hours(1);
    repo.insert(&expired_active).await.unwrap();

    repo.insert(&fixtures::token("t-new", "h3")).await.unwrap();

    let removed = repo.purge_expired(now_utc()).await.unwrap();
    assert_eq!(removed, 2, "两个过期行都应被删");
    assert_eq!(repo.list_active().await.unwrap().len(), 1, "只剩未过期那个");
}

#[tokio::test]
async fn token_hash_never_leaves_the_repository_layer() {
    // 类型层面的保证：NewRefreshToken 没有 plain_token 字段。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = UserRefreshTokenRepository::new(db.pool().clone());

    let created = repo
        .insert(&fixtures::token("t1", "sha256-abc"))
        .await
        .unwrap();
    // 库里存的是哈希（测试里我们直接传的字符串就是哈希位）
    assert_eq!(created.token_hash, "sha256-abc");
    // 明文 token 由 sm-core::refresh_token::RefreshTokenMaterial 持有，
    // 仓储层从头到尾接触不到它 —— 这就是 NewRefreshToken 不含该字段
    // 的意义。
}

// ================================================================ 任务队列

#[tokio::test]
async fn enqueue_defaults_to_pending_with_empty_summary() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let created = repo.enqueue(&fixtures::task("probe")).await.unwrap();
    assert_eq!(created.state, "pending", "DEFAULT 'pending'");
    assert_eq!(
        created.result_summary,
        Some("{}".to_owned()),
        "DEFAULT '{{}}'"
    );
    assert!(created.started_at.is_none());
    assert!(created.lease_expires_at.is_none());
    assert!(created.mutex_key.is_none());
    assert!(created.is_claimable(now_utc()));
}

#[tokio::test]
async fn params_roundtrip_as_json_text() {
    // params 是 JsonTextField —— TEXT 列装 JSON 文本，不是 jsonb。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let mut t = fixtures::task("probe");
    t.params = Some(serde_json::json!({"movie": "ABC-001", "n": 3}));
    let created = repo.enqueue(&t).await.unwrap();

    let raw = created.params.as_deref();
    assert!(raw.is_some_and(|s| s.contains("ABC-001")), "got {raw:?}");
    let parsed = sm_db::common::json_text::decode(raw);
    assert_eq!(parsed.unwrap()["n"], 3);
}

#[tokio::test]
async fn blank_required_fields_are_rejected_before_insert() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    for mutate in [
        (|t: &mut NewTaskRun| t.task_key = "  ".to_owned()) as fn(&mut NewTaskRun),
        |t: &mut NewTaskRun| t.task_name = String::new(),
        |t: &mut NewTaskRun| t.trigger_type = " ".to_owned(),
    ] {
        let mut t = fixtures::task("probe");
        mutate(&mut t);
        let err = repo.enqueue(&t).await.expect_err("必填字段为空应被拒");
        assert!(matches!(err, DbError::Business { .. }), "actual {err:?}");
    }
}

#[tokio::test]
async fn claim_moves_pending_to_running_with_a_lease() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let created = repo.enqueue(&fixtures::task("probe")).await.unwrap();

    let claimed = repo
        .claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .expect("应领到任务");
    assert_eq!(claimed.run.id, created.id);
    assert_eq!(claimed.run.state, "running");
    assert!(claimed.run.started_at.is_some(), "领取即置 started_at");
    assert!(claimed.run.has_lease());
    assert_eq!(
        claimed.lease_expires_at,
        claimed.run.lease_expires_at.unwrap()
    );
}

#[tokio::test]
async fn claim_on_empty_queue_returns_none_not_error() {
    // 空闲 worker 反复领取是正常的，不是错误。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    assert!(repo
        .claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn a_claimed_task_is_not_claimed_again() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    repo.enqueue(&fixtures::task("probe")).await.unwrap();
    let first = repo
        .claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.run.state, "running");

    // 第二次领不到 —— 状态已是 running
    assert!(repo
        .claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .is_none());
}

#[tokio::test]
async fn scheduled_tasks_wait_for_their_time() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    // 未来才执行
    let mut future = fixtures::task("later");
    future.scheduled_at = Some(now_utc() + chrono::Duration::hours(1));
    repo.enqueue(&future).await.unwrap();

    // NULL = 立即可领
    repo.enqueue(&fixtures::task("now")).await.unwrap();

    let claimed = repo
        .claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.run.task_key, "now", "只应领到立即可执行的那个");

    // 未来那个还在排队
    let waiting = repo.list_claimable(now_utc(), 10).await.unwrap();
    assert!(waiting.is_empty(), "未来任务当前不可领");
}

#[tokio::test]
async fn concurrent_claims_do_not_hand_out_the_same_task_twice() {
    // 这是 FOR UPDATE SKIP LOCKED 的**全部理由**。
    // 「先 SELECT 再 UPDATE」会让两个 worker 读到同一行 pending。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    // 只放一个任务 —— 如果领取不是排他的，两个 worker 会拿到同一个
    let only = repo.enqueue(&fixtures::task("solo")).await.unwrap();

    let (a, b) = tokio::join!(
        repo.claim(chrono::Duration::minutes(5)),
        repo.claim(chrono::Duration::minutes(5)),
    );
    // claim 返回 Result<Option<ClaimedTask>>：外层是错误、内层才是「领没领到」。
    let mut got: Vec<i32> = Vec::new();
    for outcome in [a, b] {
        if let Some(claimed) = outcome.unwrap() {
            got.push(claimed.run.id);
        }
    }

    assert_eq!(got.len(), 1, "只放一个任务时只能有一个 worker 领到");
    assert_eq!(got[0], only.id);
}

#[tokio::test]
async fn lease_expiry_makes_a_task_claimable_again() {
    // 没有回收，崩溃的 worker 会让任务永久卡在 running。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let created = repo.enqueue(&fixtures::task("probe")).await.unwrap();
    repo.claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .unwrap();

    // 租约未到期 —— 不可回收
    let still_held = repo.list_stale_leases(now_utc()).await.unwrap();
    assert!(still_held.is_empty(), "租约有效期内不该被回收");

    // 租约到期（用未来时刻模拟时间流逝）
    let future = now_utc() + chrono::Duration::minutes(10);
    let stale = repo.list_stale_leases(future).await.unwrap();
    assert_eq!(stale.len(), 1);
    assert!(stale[0].is_stale_lease(future));

    let reclaimed = repo.reclaim_stale(future).await.unwrap();
    assert_eq!(reclaimed, 1);

    // 回到 pending，可以重新领取
    let after = repo.find_by_id(created.id).await.unwrap().unwrap();
    assert_eq!(after.state, "pending");
    assert!(after.started_at.is_none(), "回收应清掉 started_at");
    assert!(after.lease_expires_at.is_none(), "回收应清掉租约");
    assert!(repo
        .claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn reclaim_does_not_touch_tasks_with_a_valid_lease() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    repo.enqueue(&fixtures::task("probe")).await.unwrap();
    repo.claim(chrono::Duration::hours(1))
        .await
        .unwrap()
        .unwrap();

    // 租约 1 小时后到期，10 分钟后回收不该动它
    let reclaimed = repo
        .reclaim_stale(now_utc() + chrono::Duration::minutes(10))
        .await
        .unwrap();
    assert_eq!(reclaimed, 0, "有效租约不该被回收");

    let still = repo.find_by_id(1).await.unwrap().unwrap();
    assert_eq!(still.state, "running");
}

#[tokio::test]
async fn renew_lease_fails_after_reclaim() {
    // 若回收后仍能续租，会「复活」一个别人正在跑的任务。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let created = repo.enqueue(&fixtures::task("probe")).await.unwrap();
    repo.claim(chrono::Duration::minutes(1))
        .await
        .unwrap()
        .unwrap();

    // 续租成功（仍在 running）
    let renewed = repo
        .renew_lease(created.id, chrono::Duration::minutes(30))
        .await
        .unwrap();
    assert!(renewed.has_lease());

    // 回收后 —— 已被别人领走，续租必须失败
    let future = now_utc() + chrono::Duration::hours(2);
    repo.reclaim_stale(future).await.unwrap();
    let err = repo
        .renew_lease(created.id, chrono::Duration::minutes(30))
        .await
        .expect_err("已回收的任务不应能续租");
    assert!(err.to_string().contains("running"), "{err}");
}

#[tokio::test]
async fn mutex_key_blocks_concurrent_enqueue_and_is_released_on_finish() {
    // 这是 mutex_key 单列唯一索引的核心行为。
    //
    // 唯一索引不含 state，所以**不释放**的话同键的下一个任务永久无法
    // 创建。finish 把它置 NULL，靠「NULL 不参与唯一约束」释放。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    // 互斥生效：同键第二个插不进去
    let first = repo
        .enqueue(&fixtures::task_with_mutex("sync", "plugin:actor:sync"))
        .await
        .unwrap();
    assert_eq!(first.mutex_key.as_deref(), Some("plugin:actor:sync"));
    assert!(first.is_mutex_guarded());

    let err = repo
        .enqueue(&fixtures::task_with_mutex(
            "sync-again",
            "plugin:actor:sync",
        ))
        .await
        .expect_err("同互斥键的任务应被唯一约束挡下");
    assert!(
        matches!(err, DbError::ConstraintViolation { .. }),
        "应归为 409，实际 {err:?}"
    );

    // 完成 -> 释放互斥键 -> 同键可以再排
    let claimed = repo
        .claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.run.id, first.id);

    let finished = repo
        .finish(claimed.run.id, &TaskOutcome::default())
        .await
        .unwrap();
    assert_eq!(finished.state, "succeeded");
    assert!(
        !finished.is_mutex_guarded(),
        "完成后必须释放互斥键，否则同键任务永久无法创建"
    );
    assert!(finished.mutex_key.is_none());

    // 同键现在可以排了 —— 这是定时任务每小时跑一次的前提
    let second = repo
        .enqueue(&fixtures::task_with_mutex("sync-2", "plugin:actor:sync"))
        .await
        .expect("释放后同键应可复用");
    assert!(second.is_mutex_guarded());
    assert!(second.id > first.id);
}

#[tokio::test]
async fn failure_also_releases_the_mutex_key() {
    // 保留失败的 key 会让重试永远撞唯一约束，而重试正是失败后最该做的事。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let first = repo
        .enqueue(&fixtures::task_with_mutex("sync", "k"))
        .await
        .unwrap();
    let claimed = repo
        .claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .unwrap();
    let failed = repo.fail(claimed.run.id, "boom").await.unwrap();

    assert_eq!(failed.state, "failed");
    assert_eq!(failed.error_message.as_deref(), Some("boom"));
    assert!(failed.mutex_key.is_none(), "失败也要释放，否则重试撞约束");

    let retry = repo
        .enqueue(&fixtures::task_with_mutex("sync-retry", "k"))
        .await
        .expect("失败后同键必须可复用");
    assert!(retry.id > first.id);
}

#[tokio::test]
async fn tasks_without_a_mutex_key_never_conflict() {
    // NULL 不参与唯一约束，所以无互斥需求的任务可以共存。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    for i in 0..3 {
        repo.enqueue(&fixtures::task(&format!("plain-{i}")))
            .await
            .unwrap();
    }
    let all = repo.list_claimable(now_utc(), 10).await.unwrap();
    assert_eq!(all.len(), 3, "无互斥键的任务应全部共存");
}

#[tokio::test]
async fn finish_and_fail_require_running_state() {
    // 否则一个 pending 任务可以被凭空置为 succeeded。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let created = repo.enqueue(&fixtures::task("probe")).await.unwrap();

    let err = repo
        .finish(created.id, &TaskOutcome::default())
        .await
        .expect_err("pending 不应能被置为成功");
    assert!(err.to_string().contains("running"), "{err}");

    let err = repo
        .fail(created.id, "x")
        .await
        .expect_err("pending 不应能被置为失败");
    assert!(err.to_string().contains("running"), "{err}");

    // 行未被改动
    assert_eq!(
        repo.find_by_id(created.id).await.unwrap().unwrap().state,
        "pending"
    );
}

#[tokio::test]
async fn progress_requires_a_positive_total() {
    // 模型注释：进度三件套要么都不填，要么都填。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let created = repo.enqueue(&fixtures::task("probe")).await.unwrap();
    let claimed = repo
        .claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.run.id, created.id);

    // 完整进度
    let progressed = repo
        .report_progress(
            created.id,
            &TaskProgress {
                current: Some(3),
                total: Some(4),
                text: Some("处理中".to_owned()),
            },
        )
        .await
        .unwrap();
    assert_eq!(progressed.progress_ratio(), Some(0.75));

    // total = 0 -> 不做除法，清空三件套
    let cleared = repo
        .report_progress(
            created.id,
            &TaskProgress {
                current: Some(3),
                total: Some(0),
                text: Some("x".to_owned()),
            },
        )
        .await
        .unwrap();
    assert_eq!(cleared.progress_ratio(), None, "总量为 0 不做除法");
    assert!(cleared.progress_current.is_none());
    assert!(cleared.progress_total.is_none());
    assert!(cleared.progress_text.is_none(), "不完整组合应被清空");
}

#[tokio::test]
async fn outcome_summary_is_stored_as_json_text() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let created = repo.enqueue(&fixtures::task("probe")).await.unwrap();
    repo.claim(chrono::Duration::minutes(5))
        .await
        .unwrap()
        .unwrap();

    let finished = repo
        .finish(
            created.id,
            &TaskOutcome {
                summary: Some(serde_json::json!({"processed": 42})),
                text: Some("完成".to_owned()),
            },
        )
        .await
        .unwrap();

    let parsed = sm_db::common::json_text::decode(finished.result_summary.as_deref());
    assert_eq!(parsed.unwrap()["processed"], 42);
    assert_eq!(finished.result_text.as_deref(), Some("完成"));
    // 成功时不该有 error_message
    assert!(finished.error_message.is_none());
}

#[tokio::test]
async fn list_by_task_key_orders_by_recency() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    repo.enqueue(&fixtures::task("probe")).await.unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    repo.enqueue(&fixtures::task("probe")).await.unwrap();

    let runs = repo.list_by_task_key("probe", 10).await.unwrap();
    assert_eq!(runs.len(), 2);
    assert!(runs[0].id > runs[1].id, "应按 created_at 倒序（最新在前）");
}

#[tokio::test]
async fn update_metadata_rejects_state_and_mutex_columns() {
    // 状态迁移必须走专用方法，互斥键必须由 finish/fail 释放。
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let created = repo
        .enqueue(&fixtures::task_with_mutex("probe", "k"))
        .await
        .unwrap();

    // 状态、互斥键、租约都不该能经通用更新改 —— 各自需要不同的伴随写入，
    // 通用路径给不出。类型别名只为绕过 clippy 的 complex_type。
    type Setter<'a> = Box<dyn Fn(&mut sm_db::common::update::UpdateSet<'a>) + 'a>;
    let forbidden: Vec<(&str, Setter<'_>)> = vec![
        (
            "state",
            Box::new(|s: &mut sm_db::common::update::UpdateSet<'_>| {
                s.set("state", "succeeded");
            }),
        ),
        (
            "mutex_key",
            Box::new(|s: &mut sm_db::common::update::UpdateSet<'_>| {
                s.set("mutex_key", "other");
            }),
        ),
        (
            "lease_expires_at",
            Box::new(|s: &mut sm_db::common::update::UpdateSet<'_>| {
                s.set(
                    "lease_expires_at",
                    chrono::NaiveDate::from_ymd_opt(2030, 1, 1)
                        .unwrap()
                        .and_hms_opt(0, 0, 0)
                        .unwrap(),
                );
            }),
        ),
    ];
    for (column, apply) in forbidden {
        let mut set = sm_db::common::update::UpdateSet::new();
        apply(&mut set);
        let err = repo
            .update_metadata(created.id, set)
            .await
            .expect_err(format!("`{column}` 不应能经 update_metadata 修改").as_str());
        assert!(err.to_string().contains(column), "{err}");
    }

    // 允许的字段可以改
    let mut set = sm_db::common::update::UpdateSet::new();
    set.set("task_name", "改名后".to_owned());
    let updated = repo.update_metadata(created.id, set).await.unwrap();
    assert_eq!(updated.task_name, "改名后");
    // 互斥键不受影响
    assert!(updated.is_mutex_guarded());
}

#[tokio::test]
async fn update_metadata_of_missing_row_reports_not_found() {
    let Some(db) = TestDb::create().await else {
        return;
    };
    let repo = BackgroundTaskRunRepository::new(db.pool().clone());

    let mut set = sm_db::common::update::UpdateSet::new();
    set.set("task_name", "x".to_owned());
    let err = repo.update_metadata(999_999, set).await.unwrap_err();
    assert!(matches!(err, DbError::NotFound { .. }), "actual {err:?}");
}
