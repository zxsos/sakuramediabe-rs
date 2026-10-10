//! `AccountService` 与 bcrypt 无感升级的集成测试（真实 PostgreSQL）。
//!
//! # 这一批最想钉住的行为
//!
//! **存量 bcrypt 用户第一次登录就能进来，且哈希当场变成 Argon2id。**
//! 两件事必须同时成立：
//!
//! - 只验不升 → 每次登录都跑一遍 bcrypt，迁移永远不完成，bcrypt 依赖
//!   也永远删不掉；
//! - 只升不验 → 把用户的密码改成他自己都认不出的东西。
//!
//! 其余用例钉的是错误契约，以及那条唯一有安全意义的规则：
//! **改密码必须作废全部会话。**
//!
//! 上游出处：`src/service/system/account_service.py`、`auth_service.py:29-30`。

use std::sync::atomic::{AtomicU32, Ordering};

use sm_db::repo::{NewUser, UserRepository};
use sm_db::testing::TestDb;
use sm_service::error::ServiceError;
use sm_service::system::account::AccountService;
use sm_service::system::auth::{AuthConfig, AuthService, TokenPair};

const SECRET: &str = "account-secret";
const CURRENT: &str = "current password";
const NEW: &str = "new password";

fn n() -> u32 {
    static C: AtomicU32 = AtomicU32::new(0);
    C.fetch_add(1, Ordering::Relaxed)
}

/// 测试用低 Argon2 参数 —— 默认 19 MiB 每个用例都太贵。
fn fast_hash(password: &str) -> String {
    sm_core::password::hash_password_with(password, 64, 1, 1).expect("哈希")
}

/// bcrypt 侧固定 cost 4（crate 的下限）。
fn bcrypt_of(password: &str) -> String {
    bcrypt::hash(password, 4).expect("bcrypt 哈希")
}

/// 建一个用户，用户名唯一（用计数器而不是时间戳，避免同毫秒碰撞）。
async fn seed_user(db: &TestDb, password_hash: &str) -> (i32, String) {
    let username = format!("acct{:04}", n());
    let id = UserRepository::new(db.pool().clone())
        .insert(&NewUser {
            username: username.clone(),
            password_hash: password_hash.to_owned(),
        })
        .await
        .expect("插入用户")
        .id;
    (id, username)
}

async fn login(db: &TestDb, username: &str, password: &str) -> Result<TokenPair, ServiceError> {
    AuthService::new(db.pool())
        .login(&AuthConfig::new(SECRET), username, password, None, None)
        .await
}

/// 读回密码哈希。用 `async fn` 而不是 `block_on` ——
/// 测试已在 runtime 内，嵌套 `block_on` 会 panic。
async fn password_of(db: &TestDb, user_id: i32) -> String {
    UserRepository::new(db.pool().clone())
        .find_by_id(user_id)
        .await
        .expect("读用户")
        .expect("用户应当存在")
        .password_hash
}

#[track_caller]
fn assert_error(err: &ServiceError, status: u16, code: &str) {
    assert_eq!(
        (err.status, err.code()),
        (status, code),
        "状态码与错误码必须同时对上：客户端按 code 分支、按 status 决定重试"
    );
}

async fn active_token_count(db: &TestDb) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM user_refresh_tokens WHERE status = 'active'")
        .fetch_one(db.pool())
        .await
        .expect("统计 active 令牌")
}

async fn total_token_count(db: &TestDb) -> i64 {
    sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM user_refresh_tokens")
        .fetch_one(db.pool())
        .await
        .expect("统计令牌")
}

// ================================================================ 无感升级

#[tokio::test]
async fn a_bcrypt_user_logs_in_and_the_hash_is_upgraded_in_place() {
    let db = TestDb::require().await;
    let (user_id, username) = seed_user(&db, &bcrypt_of(CURRENT)).await;
    assert!(
        sm_core::password::is_bcrypt(&password_of(&db, user_id).await),
        "前置条件：库里应当是 bcrypt 哈希"
    );

    login(&db, &username, CURRENT)
        .await
        .expect("存量 bcrypt 用户必须能登录");

    let after = password_of(&db, user_id).await;
    assert!(
        after.starts_with("$argon2id$"),
        "第一次登录后应升级为 Argon2id，实际 {after}"
    );
    assert!(!sm_core::password::is_bcrypt(&after));
    // 升级后的哈希仍能验证同一个密码 —— 否则等于把密码改了。
    assert!(sm_core::password::verify_password(CURRENT, &after).is_ok());
}

#[tokio::test]
async fn upgrading_happens_once_not_on_every_login() {
    // 否则每次登录都产生新盐 + 一条 UPDATE，updated_at 永远在动。
    let db = TestDb::require().await;
    let (user_id, username) = seed_user(&db, &bcrypt_of(CURRENT)).await;

    login(&db, &username, CURRENT).await.unwrap();
    let after_first = password_of(&db, user_id).await;
    login(&db, &username, CURRENT).await.unwrap();

    assert_eq!(
        password_of(&db, user_id).await,
        after_first,
        "已是 Argon2id 的哈希不该被再次改写"
    );
}

#[tokio::test]
async fn a_wrong_password_neither_logs_in_nor_upgrades() {
    // 顺序反了就是漏洞：失败的登录改写哈希。
    let db = TestDb::require().await;
    let (user_id, username) = seed_user(&db, &bcrypt_of(CURRENT)).await;
    let before = password_of(&db, user_id).await;

    let err = login(&db, &username, "not the password")
        .await
        .expect_err("错误密码必须失败");
    assert_error(&err, 401, "invalid_credentials");
    assert_eq!(
        password_of(&db, user_id).await,
        before,
        "验证失败绝不能改写哈希"
    );
}

#[tokio::test]
async fn an_argon2_user_is_not_rewritten_on_login() {
    // 新部署路径：写侧就是 Argon2id，登录不该产生任何写操作。
    let db = TestDb::require().await;
    let (user_id, username) = seed_user(&db, &fast_hash(CURRENT)).await;
    let before = password_of(&db, user_id).await;

    login(&db, &username, CURRENT).await.unwrap();
    assert_eq!(password_of(&db, user_id).await, before);
}

// ================================================================ 账号资料

#[tokio::test]
async fn a_blank_username_is_rejected() {
    // 上游没有这条校验（`CharField(unique=True)`，能设成空串）。
    // 这里是刻意收紧，理由见 account.rs 模块文档。
    let db = TestDb::require().await;
    let svc = AccountService::new(db.pool());
    let (id, _) = seed_user(&db, &fast_hash(CURRENT)).await;

    for blank in ["", "   ", "\t"] {
        let err = svc.update_username(id, blank).await.unwrap_err();
        assert_error(&err, 422, "validation_error");
        assert_eq!(err.api.message, "username cannot be blank");
    }
}

#[tokio::test]
async fn a_duplicate_username_is_409_carrying_the_name() {
    let db = TestDb::require().await;
    let svc = AccountService::new(db.pool());
    let (other, taken) = seed_user(&db, &fast_hash(CURRENT)).await;
    svc.update_username(other, &taken).await.unwrap();
    let (mine, _) = seed_user(&db, &fast_hash(CURRENT)).await;

    let err = svc.update_username(mine, &taken).await.unwrap_err();
    assert_error(&err, 409, "username_conflict");
    assert_eq!(err.api.message, "Username already exists");
    assert_eq!(
        err.api.details.as_ref().unwrap().get("username"),
        Some(&serde_json::json!(taken.as_str())),
        "details 必须带上是哪个名字"
    );
}

#[tokio::test]
async fn keeping_your_own_username_is_not_a_conflict() {
    // 少了 exclude-self 判断，改名成当前值会撞自己。
    let db = TestDb::require().await;
    let svc = AccountService::new(db.pool());
    let (id, username) = seed_user(&db, &fast_hash(CURRENT)).await;

    let updated = svc
        .update_username(id, &username)
        .await
        .expect("同名应当放行");
    assert_eq!(updated.username, username);
}

#[tokio::test]
async fn a_username_is_stripped() {
    let db = TestDb::require().await;
    let svc = AccountService::new(db.pool());
    let (id, _) = seed_user(&db, &fast_hash(CURRENT)).await;
    let padded = format!("  spaced{}  ", n());

    let updated = svc.update_username(id, &padded).await.unwrap();
    assert_eq!(updated.username, padded.trim());
}

#[tokio::test]
async fn a_missing_user_is_404() {
    let db = TestDb::require().await;
    let svc = AccountService::new(db.pool());
    let err = svc.get_account(999_999).await.unwrap_err();
    assert_error(&err, 404, "user_not_found");
    assert_eq!(
        err.api.details.as_ref().unwrap().get("user_id"),
        Some(&serde_json::json!(999_999))
    );
}

// ================================================================ 改密码

#[tokio::test]
async fn changing_the_password_requires_the_current_one() {
    let db = TestDb::require().await;
    let svc = AccountService::new(db.pool());
    let (id, _) = seed_user(&db, &fast_hash(CURRENT)).await;

    let err = svc
        .change_password(id, "not it", NEW)
        .await
        .expect_err("旧密码错必须被拒");
    assert_error(&err, 401, "invalid_credentials");
    assert_eq!(err.api.message, "Current password is incorrect");
    // 密码没被改
    assert!(sm_core::password::verify_password(CURRENT, &password_of(&db, id).await).is_ok());
}

#[tokio::test]
async fn changing_the_password_works_from_a_bcrypt_hash_too() {
    // 存量用户改密之后就该是 Argon2id，不需要先登录一次。
    let db = TestDb::require().await;
    let svc = AccountService::new(db.pool());
    let (id, _) = seed_user(&db, &bcrypt_of(CURRENT)).await;
    assert!(sm_core::password::is_bcrypt(&password_of(&db, id).await));

    svc.change_password(id, CURRENT, NEW).await.expect("改密");

    let after = password_of(&db, id).await;
    assert!(after.starts_with("$argon2id$"), "改密后应是 Argon2id");
    assert!(sm_core::password::verify_password(NEW, &after).is_ok());
    assert!(
        sm_core::password::verify_password(CURRENT, &after).is_err(),
        "旧密码必须失效"
    );
}

#[tokio::test]
async fn changing_the_password_revokes_every_refresh_token() {
    // 这一条是改密码的**唯一**安全意义。不作废的话，改密码只是改了个字段。
    let db = TestDb::require().await;
    let svc = AccountService::new(db.pool());
    let (id, username) = seed_user(&db, &fast_hash(CURRENT)).await;

    for _ in 0..2 {
        login(&db, &username, CURRENT).await.unwrap();
    }
    assert!(
        active_token_count(&db).await >= 2,
        "前置条件：应当有多个 active 令牌"
    );

    svc.change_password(id, CURRENT, NEW).await.unwrap();

    assert_eq!(
        active_token_count(&db).await,
        0,
        "改密码后不应再有 active 令牌 —— 否则改密码等于没改"
    );
    assert_eq!(
        total_token_count(&db).await,
        2,
        "行本身保留（吊销而非物理删除，保留 client_ip / user_agent 审计）"
    );
}

#[tokio::test]
async fn the_old_password_stops_working_after_the_change() {
    let db = TestDb::require().await;
    let svc = AccountService::new(db.pool());
    let (id, username) = seed_user(&db, &fast_hash(CURRENT)).await;

    svc.change_password(id, CURRENT, NEW).await.unwrap();

    assert_error(
        &login(&db, &username, CURRENT).await.unwrap_err(),
        401,
        "invalid_credentials",
    );
    login(&db, &username, NEW).await.expect("新密码应当能登录");
}
