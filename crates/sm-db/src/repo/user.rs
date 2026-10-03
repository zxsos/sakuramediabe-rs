//! `users` / `user_refresh_tokens` 表仓储。
//!
//! # 为什么令牌表没有 `user_id` 外键
//!
//! 上游是**单用户部署**：`User.select().order_by(User.id).first()` 是
//! 取用户的固定写法（`sm-core/src/password.rs` 的迁移说明引用了它）。
//! 单用户下「这个令牌属于谁」是恒真的，存一列外键没有信息量。
//!
//! 所以这里**不加** `user_id`，与上游 schema 保持同构。若将来支持多用户，
//! 那是 schema 变更，需要先确认上游怎么做，而不是在这里偷偷加列。
//!
//! # 轮换必须原子 —— 本文件是事务的第一个真实用例
//!
//! `sm-core::refresh_token` 的三个不变量里，第二条是「每次刷新都轮换：
//! 旧行标 `revoked` 并写 `replaced_by_token_id`」。
//!
//! 「吊销旧行」+「插入新行」如果分两次提交，中间崩溃会留下：
//!
//! - 旧行已吊销、新行未插入 → **用户被彻底登出**，且无法自愈
//! - 更糟的反向：新行已插入、旧行仍是 active → **旧令牌依然可用**，
//!   攻击者截获的令牌可以无限次换取新令牌，轮换形同虚设
//!
//! 两种都不是「下次登录就好了」能解决的，所以 [`UserRefreshTokenRepository::rotate`]
//! 把三步（校验 → 吊销旧 → 插入新）放进**同一个事务**。
//!
//! 这是本 crate 第一次真正需要事务。此前 `pool()` 的文档写着「事务场景
//! 需要它」却没有任何方法用上；现在有了。

use chrono::NaiveDateTime;
use sqlx::{PgPool, Postgres, Transaction};

use crate::common::page::{Page, PageRequest};
use crate::common::update::UpdateSet;
use crate::error::DbError;
use crate::paged_list;
use crate::system::user::{RefreshTokenStatus, User, UserRefreshToken};

/// 实体名，用于错误分类。
const ENTITY: &str = "User";
const TOKEN_ENTITY: &str = "UserRefreshToken";

/// 新建一个用户。
#[derive(Debug, Clone)]
pub struct NewUser {
    /// 登录名。**唯一**，也是 `refresh_token_pair` 之外唯一的定位方式。
    pub username: String,
    /// Argon2 PHC 字符串。**明文永不进这里**。
    pub password_hash: String,
}

impl NewUser {
    fn validate(&self) -> Result<(), DbError> {
        let username = self.username.trim();
        if username.is_empty() {
            return Err(DbError::business(ENTITY, "username 不能为空"));
        }
        if self.password_hash.trim().is_empty() {
            return Err(DbError::business(ENTITY, "password_hash 不能为空"));
        }
        Ok(())
    }
}

/// 待插入的刷新令牌。
///
/// `plain_token` **不在这里** —— 明文只在 HTTP 响应里出现一次，
/// 落库的只有 `token_hash`。这个类型的存在就是为了让「明文进库」
/// 在类型层面不可能发生。
#[derive(Debug, Clone)]
pub struct NewRefreshToken {
    /// `hex(16 字节)`，32 字符。唯一。
    pub token_id: String,
    /// `sha256(plain_token)` 的小写 hex。
    pub token_hash: String,
    pub expires_at: NaiveDateTime,
    /// 审计留痕。**可空但应尽量填** —— 旧接口的审计依赖这两列。
    pub client_ip: Option<String>,
    pub user_agent: Option<String>,
}

impl NewRefreshToken {
    fn validate(&self) -> Result<(), DbError> {
        if self.token_id.trim().is_empty() {
            return Err(DbError::business(TOKEN_ENTITY, "token_id 不能为空"));
        }
        if self.token_hash.trim().is_empty() {
            return Err(DbError::business(TOKEN_ENTITY, "token_hash 不能为空"));
        }
        Ok(())
    }
}

/// 轮换的结果。
///
/// 刻意**不**实现 `Debug` 的内容输出之外的任何东西：新令牌的明文
/// 不在这里（见 [`NewRefreshToken`]），只有 `token_id` 与哈希。
#[derive(Debug, Clone)]
pub struct Rotation {
    /// 新令牌行。
    pub fresh: UserRefreshToken,
    /// 被替换的旧行，已标 `revoked` 并指向新令牌。
    pub retired: UserRefreshToken,
}

/// [`UserRefreshTokenRepository::rotate_within`] 的结果。
///
/// 需要区分「成功」与「失败但要提交」：把过期令牌标成 `expired` 是
/// 持久化意图（审计要能区分「过期」与「被主动撤销」），与「校验不通过、
/// 一个字节都不该写」是不同的事。
///
/// `Done` 里是 `Box`：它内含两个完整的 `UserRefreshToken` 行（几十个
/// `String` 与 `Option`），而 `CommitThenFail` 只带一个错误信封。装箱让
/// 两个变体大小接近，否则 clippy 的 `large_enum_variant` 会指出这里。
/// 换来的是一次堆分配 —— 这条路径每个 HTTP 请求走一次，不是热循环。
enum RotationOutcome {
    /// 轮换成功，调用方提交。
    Done(Box<Rotation>),
    /// 失败，但事务里有**要保留的**写入，调用方提交后返回该错误。
    CommitThenFail(DbError),
}

/// `users` 表仓储。
#[derive(Debug, Clone)]
pub struct UserRepository {
    pool: PgPool,
}

impl UserRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<User>, DbError> {
        Ok(
            sqlx::query_as::<_, User>("SELECT * FROM users WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按 `username` 查询。这是登录流程的唯一定位方式。
    pub async fn find_by_username(&self, username: &str) -> Result<Option<User>, DbError> {
        Ok(
            sqlx::query_as::<_, User>("SELECT * FROM users WHERE username = $1")
                .bind(username.trim())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 单用户部署的便捷取法。
    ///
    /// 对应上游 `User.select().order_by(User.id).first()`。**有多个用户时
    /// 取 id 最小的那一个** —— 与上游一致，不改成「取最后一个」，
    /// 因为那会让升级后的行为与旧后端不同。
    pub async fn find_primary(&self) -> Result<Option<User>, DbError> {
        Ok(
            sqlx::query_as::<_, User>("SELECT * FROM users ORDER BY id LIMIT 1")
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 插入。
    pub async fn insert(&self, new: &NewUser) -> Result<User, DbError> {
        new.validate()?;
        sqlx::query_as::<_, User>(
            "INSERT INTO users (username, password_hash, created_at, updated_at) \
             VALUES ($1, $2, $3, $3) RETURNING *",
        )
        .bind(new.username.trim())
        .bind(new.password_hash.trim())
        .bind(crate::common::time::now_utc())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))
    }

    /// 更新登录时间。
    ///
    /// 单独成方法而不是走通用 `update`：`last_login_at` 是**宿主行为**，
    /// 而通用 `update` 会让调用方有机会把它设成任意值。
    pub async fn touch_last_login(&self, id: i32) -> Result<User, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, User>(
            "UPDATE users SET last_login_at = $2, updated_at = $2 WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    /// 改密码。
    ///
    /// 密码哈希**不可经由通用 `update` 改** —— 那条路径不校验 PHC 格式，
    /// 写进去一个非 Argon2 串会让所有人永久无法登录，且无法从库里看出来。
    pub async fn set_password_hash(&self, id: i32, hash: &str) -> Result<User, DbError> {
        if hash.trim().is_empty() {
            return Err(DbError::business(ENTITY, "password_hash 不能为空"));
        }
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, User>(
            "UPDATE users SET password_hash = $2, updated_at = $3 WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(hash.trim())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))
    }
}

/// `user_refresh_tokens` 表仓储。
#[derive(Debug, Clone)]
pub struct UserRefreshTokenRepository {
    pool: PgPool,
}

impl UserRefreshTokenRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按 `token_id` 查询（唯一索引）。
    pub async fn find_by_token_id(
        &self,
        token_id: &str,
    ) -> Result<Option<UserRefreshToken>, DbError> {
        Ok(sqlx::query_as::<_, UserRefreshToken>(
            "SELECT * FROM user_refresh_tokens WHERE token_id = $1",
        )
        .bind(token_id.trim())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 插入一个令牌。
    ///
    /// `status` **显式写入** `"active"`，不依赖数据库 DEFAULT。
    ///
    /// 此前这里省略 `status` 并注释说明「走数据库 DEFAULT `"active"`,
    /// 让初始状态住在 schema 里」。但 DDL 里**没有**这个 DEFAULT：
    ///
    /// ```sql
    /// status varchar(32) NOT NULL,     -- 没有 DEFAULT
    /// ```
    ///
    /// 上游确实声明了 `default=RefreshTokenStatus.ACTIVE.value`，但那是
    /// **属性引用**而不是字面量，`parity/schema_contract.py` 的
    /// `literal()` 解析不出来，于是 `gen_ddl.py` 把它丢了。
    ///
    /// 依赖一个不存在的 DEFAULT 的后果是每次插入都违反 NOT NULL。这正是
    /// 本文件那批集成测试第一次真正执行时暴露的 —— 它们从未在 CI 里跑过。
    /// 显式写入让插入不依赖任何 schema 假设。
    pub async fn insert(&self, new: &NewRefreshToken) -> Result<UserRefreshToken, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, UserRefreshToken>(
            "INSERT INTO user_refresh_tokens ( \
                 token_id, token_hash, status, expires_at, client_ip, user_agent, \
                 created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $7) RETURNING *",
        )
        .bind(new.token_id.trim())
        .bind(new.token_hash.trim())
        .bind(RefreshTokenStatus::Active.as_str())
        .bind(new.expires_at)
        .bind(new.client_ip.as_deref())
        .bind(new.user_agent.as_deref())
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(TOKEN_ENTITY))
    }

    /// 轮换令牌。**本文件的核心方法。**
    ///
    /// 三步在一个事务里：校验旧令牌 → 吊销旧行 → 插入新行。
    ///
    /// # 校验顺序有讲究
    ///
    /// 1. 存在性 —— 404
    /// 2. 状态可刷新 —— `status = 'active'`，**且** `expires_at > now`
    /// 3. 哈希匹配 —— 防伪造
    ///
    /// 未知状态按**拒绝**处理（`can_refresh` 返回 false），不降级放行 ——
    /// 把失效令牌当有效是安全事故。
    ///
    /// # 为什么要检查 `is_rotated`
    ///
    /// 已轮换的旧行 `status` 已经是 `revoked`，第 2 步就会拦下。
    /// 这里额外断言一次，是为了让「重放」这个意图在代码里显式可见 ——
    /// 这是安全边界，值得单独占一行。
    ///
    /// # 事务边界
    ///
    /// 吊销与插入之间**不能有任何可见中间态**。若这里不是原子的，
    /// 「新行已插入、旧行仍 active」会让被截获的旧令牌无限续期。
    ///
    /// # 为什么每条错误路径都显式 `rollback().await`
    ///
    /// 依赖 `Transaction` 的 `Drop` 隐式回滚在这里**会死锁**。
    ///
    /// 测试用的连接池是 `max_connections(1)` —— 唯一那条连接正被这个
    /// 事务占着，而 `Drop` 触发的回滚要等它自己。表现为测试挂住、
    /// 永不返回，且没有任何错误信息。
    ///
    /// 那个池只有一条连接是刻意的（`search_path` 是会话级设置，见
    /// `testing::maybe_pool`），所以「靠 drop 回滚」这条在别处能用的
    /// 惯用法在这里不可用。显式回滚在任何池配置下都正确。
    pub async fn rotate(
        &self,
        token_id: &str,
        presented_hash: &str,
        fresh: &NewRefreshToken,
        now: NaiveDateTime,
    ) -> Result<Rotation, DbError> {
        let mut tx: Transaction<'_, Postgres> = self.pool.begin().await?;

        let outcome = self
            .rotate_within(&mut tx, token_id, presented_hash, fresh, now)
            .await;

        match outcome {
            Ok(RotationOutcome::Done(rotation)) => {
                tx.commit().await?;
                Ok(*rotation)
            }
            // 「过期」那条路径：标记已写入，提交它，然后如实报告失败。
            Ok(RotationOutcome::CommitThenFail(err)) => {
                tx.commit().await?;
                Err(err)
            }
            Err(err) => {
                // 回滚失败不掩盖原本的业务错误 —— 后者才是调用方要处理的。
                let _ = tx.rollback().await;
                Err(err)
            }
        }
    }

    /// [`Self::rotate_within`] 的结果类型见模块内的 [`RotationOutcome`]。
    ///
    /// [`Self::rotate`] 的事务内主体。**不**自己提交或回滚。
    async fn rotate_within(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        token_id: &str,
        presented_hash: &str,
        fresh: &NewRefreshToken,
        now: NaiveDateTime,
    ) -> Result<RotationOutcome, DbError> {
        fresh.validate()?;
        let token_id = token_id.trim().to_owned();

        // 1. 取出旧行。用 FOR UPDATE 拿行锁 —— 两个并发刷新请求
        //    只有一个能过这关，另一个会等到事务结束再读到 revoked 状态。
        let retired = sqlx::query_as::<_, UserRefreshToken>(
            "SELECT * FROM user_refresh_tokens WHERE token_id = $1 FOR UPDATE",
        )
        .bind(&token_id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(|| DbError::not_found(TOKEN_ENTITY, &token_id))?;

        // 2. 状态与有效期。未知状态在这里被拒。
        if !retired.can_refresh() {
            return Err(DbError::business(
                TOKEN_ENTITY,
                format!("令牌状态 {} 不可用于刷新", retired.status),
            ));
        }
        if retired.expires_at <= now {
            // 过期是**终态**，不是「暂时不可用」：把行标成 expired，
            // 让「过期」与「被吊销」在库里可区分，否则审计时看不出
            // 攻击者用的是过期令牌还是被主动撤销的。
            sqlx::query(
                "UPDATE user_refresh_tokens SET status = $2, updated_at = $3 WHERE id = $1",
            )
            .bind(retired.id)
            .bind(RefreshTokenStatus::Expired.as_str())
            .bind(now)
            .execute(&mut **tx)
            .await?;
            // 把过期令牌标成 `expired` 是**持久化意图** —— 审计要能区分
            // 「过期」与「被主动撤销」，所以这一条路径要提交而不是回滚。
            //
            // 用返回值告诉外层「提交我」，而不是在这里 `tx.commit()`：
            // 这里的 tx 是 `&mut Transaction`，commit 会消耗它。
            return Ok(RotationOutcome::CommitThenFail(DbError::business(
                TOKEN_ENTITY,
                "令牌已过期（已标记为 expired）",
            )));
        }

        // 3. 哈希匹配。放在状态检查之后 —— 一个过期的令牌不值得
        //    再花一次哈希比较，早失败更省。
        if retired.token_hash != presented_hash.trim() {
            // 哈希不匹配但 token_id 命中，说明**有人拿着合法的 token_id
            // 配错误的令牌**。这可能是重放，也可能是 id 泄露后的探测，
            // 两种都按拒绝处理并保留审计列。
            return Err(DbError::business(TOKEN_ENTITY, "令牌哈希不匹配"));
        }

        // 4. 插入新行（先插，这样 FK/唯一约束失败会早于状态变更暴露）。
        //
        // `status` 显式写 `"active"`，理由同 `insert`：DDL 里没有 DEFAULT。
        let inserted = sqlx::query_as::<_, UserRefreshToken>(
            "INSERT INTO user_refresh_tokens ( \
                 token_id, token_hash, status, expires_at, client_ip, user_agent, \
                 created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $7) RETURNING *",
        )
        .bind(fresh.token_id.trim())
        .bind(fresh.token_hash.trim())
        .bind(RefreshTokenStatus::Active.as_str())
        .bind(fresh.expires_at)
        .bind(fresh.client_ip.as_deref())
        .bind(fresh.user_agent.as_deref())
        .bind(now)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| DbError::from(e).with_entity(TOKEN_ENTITY))?;

        // 5. 吊销旧行并指向新令牌。顺序与上面相反：先拿新行的 id
        //    填进 replaced_by_token_id，再改状态。
        let retired_after = sqlx::query_as::<_, UserRefreshToken>(
            "UPDATE user_refresh_tokens \
             SET status = $2, revoked_at = $3, replaced_by_token_id = $4, updated_at = $3 \
             WHERE id = $1 RETURNING *",
        )
        .bind(retired.id)
        .bind(RefreshTokenStatus::Revoked.as_str())
        .bind(now)
        .bind(&inserted.token_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|e| DbError::from(e).with_entity(TOKEN_ENTITY))?;

        debug_assert!(
            retired_after.is_rotated(),
            "轮换后旧行必须指向新令牌，否则重放拦不住"
        );

        // 提交由调用方（`rotate`）负责 —— 它需要先看清成功还是失败，
        // 才能决定 commit 还是 rollback。
        Ok(RotationOutcome::Done(Box::new(Rotation {
            fresh: inserted,
            retired: retired_after,
        })))
    }

    /// 吊销单个令牌（登出）。
    ///
    /// 与 [`UserRefreshTokenRepository::rotate`] 的区别：**不创建**接替者，
    /// 因此 `replaced_by_token_id` 留 `None`。
    pub async fn revoke(
        &self,
        token_id: &str,
        now: NaiveDateTime,
    ) -> Result<UserRefreshToken, DbError> {
        let row = sqlx::query_as::<_, UserRefreshToken>(
            "UPDATE user_refresh_tokens \
             SET status = $2, revoked_at = $3, updated_at = $3 \
             WHERE token_id = $1 RETURNING *",
        )
        .bind(token_id.trim())
        .bind(RefreshTokenStatus::Revoked.as_str())
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(TOKEN_ENTITY, token_id.trim()))?;
        Ok(row)
    }

    /// 吊销某用户的**全部**活跃令牌。
    ///
    /// 没有 `user_id` 列（单用户部署，见模块文档），所以这条方法的
    /// 语义是「吊销所有活跃令牌」—— 在单用户下等价于「吊销该用户的」，
    /// 但**不要**把它当成多用户 API 使用。
    pub async fn revoke_all_active(&self, now: NaiveDateTime) -> Result<u64, DbError> {
        let result = sqlx::query(
            "UPDATE user_refresh_tokens \
             SET status = $1, revoked_at = $2, updated_at = $2 \
             WHERE status = $3",
        )
        .bind(RefreshTokenStatus::Revoked.as_str())
        .bind(now)
        .bind(RefreshTokenStatus::Active.as_str())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// 清理过期令牌。返回删除行数。
    ///
    /// 删的是**已过期**的，无论 `status` 是什么 —— 一个 `status=active`
    /// 但 `expires_at` 已过的行是过期清理该负责的，吊销逻辑不该越界。
    pub async fn purge_expired(&self, now: NaiveDateTime) -> Result<u64, DbError> {
        let result = sqlx::query("DELETE FROM user_refresh_tokens WHERE expires_at <= $1")
            .bind(now)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected())
    }

    paged_list! {
        /// 列出活跃令牌（「你的登录设备」列表）。**分页。**
        ///
        /// 单用户下返回该用户全部活跃令牌。
        ///
        /// 分页的理由不是「数量可能很多」——活跃令牌每个设备一条，通常
        /// 是个位数——而是**统一**：这是一个列表端点，客户端会按分页协议
        /// 消费它。给它一个不分页的特例，等于让这个端点成为唯一一个
        /// 形状不同的。
        pub async fn list_active(
            &self,
            status: &str,
        ) -> Result<Page<UserRefreshToken>, DbError> {
            count = "SELECT COUNT(*) FROM user_refresh_tokens WHERE status = $1",
            items = "SELECT * FROM user_refresh_tokens WHERE status = $1 \
                     ORDER BY created_at, id LIMIT $2 OFFSET $3",
        }
    }
}

/// 令牌轮换时可以顺手更新用户的辅助方法。
///
/// 之所以放在 `UpdateSet` 之外：密码哈希**不能**经由通用 update 改，
/// 理由见 [`UserRepository::set_password_hash`]。
pub fn password_update_set(hash: &str) -> UpdateSet<'static> {
    let mut set = UpdateSet::new();
    set.set("password_hash", hash.to_owned());
    set
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_user(name: &str, hash: &str) -> NewUser {
        NewUser {
            username: name.to_owned(),
            password_hash: hash.to_owned(),
        }
    }

    fn new_token(id: &str, hash: &str) -> NewRefreshToken {
        NewRefreshToken {
            token_id: id.to_owned(),
            token_hash: hash.to_owned(),
            expires_at: crate::common::time::now_utc() + chrono::Duration::days(30),
            client_ip: Some("127.0.0.1".to_owned()),
            user_agent: None,
        }
    }

    #[test]
    fn blank_username_or_hash_is_rejected() {
        assert!(new_user("account", "h").validate().is_ok());
        assert!(new_user("   ", "h").validate().is_err(), "空白用户名");
        assert!(new_user("a", "  ").validate().is_err(), "空哈希");
    }

    #[test]
    fn blank_token_material_is_rejected() {
        assert!(new_token("t", "h").validate().is_ok());
        assert!(new_token("  ", "h").validate().is_err());
        assert!(new_token("t", "").validate().is_err());
    }

    #[test]
    fn token_material_carries_no_plaintext_field() {
        // 这是类型层面的保证：NewRefreshToken 没有 plain_token 字段，
        // 所以「明文落库」需要主动构造一个不存在的字段才能做到。
        // 编译通过即证明断言成立。
        let t = new_token("t", "h");
        assert_eq!(t.token_id, "t");
        assert!(
            !format!("{t:?}").contains("plain"),
            "调试输出里也不该有明文概念"
        );
    }
}
