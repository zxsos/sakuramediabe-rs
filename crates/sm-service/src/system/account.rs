//! 账号资料与改密，对应上游 `src/service/system/account_service.py`（37 行）。
//!
//! # 四条规则
//!
//! | 规则 | 上游 | 后果 |
//! |---|---|---|
//! | 用户名已被别人占用 | `existing_user.id != user.id` 才放行 | 409 `username_conflict` |
//! | 空用户名 | **无校验**（`CharField(unique=True)`） | 422 `validation_error`（**刻意偏差**，见下） |
//! | 改密码要先验旧密码 | `bcrypt.checkpw(current)` | 401 `invalid_credentials` |
//! | 改密码后作废所有会话 | `UserRefreshToken.delete().execute()` | 全部 refresh token 立即失效 |
//!
//! # 「改密码后作废全部 refresh token」是这一层唯一有安全含义的规则
//!
//! 改密码的**实际**作用就是让别人的会话失效。不作废的话，改密码只是改了
//! 一个字段 —— 已经拿到 token 的人（含攻击者）照常能用。
//!
//! ## 一处刻意偏差：吊销而非物理删除
//!
//! 上游 `UserRefreshToken.delete().execute()` 把所有行**删掉**。这里改用
//! [`UserRefreshTokenRepository::revoke_all_active`]：
//!
//! - **可观测行为相同** —— 旧 token 立刻不可用（今天没有任何端点列出
//!   refresh token，客户端看不出差别）；
//! - **审计信息保留** —— 行上带 `client_ip` / `user_agent` / `created_at`，
//!   删掉就再也答不出「这个账号上次是从哪台设备登的」。
//!
//! 代价是这些行会留到过期清理（`purge_expired` 已在）。要改回物理删除
//! 只需换一行仓储调用，但那时应先确认没有端点依赖这些行。
//!
//! # 另一处刻意偏差：空用户名 422
//!
//! 上游 `username = CharField(unique=True, index=True)` —— **没有**空白校验，
//! 所以 `PATCH /account {"username": ""}` 在上游会把用户名设成空串，随后
//! `find_by_username("")` 能命中一条在登录界面里无法复现的账号。
//!
//! 这里返回 422。两个理由：本仓库的 `NewUser::validate` **已经**拒绝建号时
//! 的空用户名（若允许改，就出现「不能创建、却能改成」的不对称）；而空用户名
//! 是数据质量问题，不是特性。
//!
//! # 上游是纯静态方法 + 直接 `setattr`
//!
//! ```python
//! for field_name, value in update_data.items():
//!     setattr(user, field_name, value)
//! ```
//!
//! 那是「把请求体的 key 当列名」。将来 `AccountUpdateRequest` 加一个字段，
//! 它会**自动**开始写那一列，而没人 review 过那条路径。
//!
//! 这里显式只处理 `username`：要加可写字段，必须在这里显式加一行，
//! 让「新增可写字段」变成一个需要被看见的动作。

use sm_core::password::{hash_password, verify_password};
use sm_db::repo::{UserRefreshTokenRepository, UserRepository};
use sm_db::system::user::User;
use sm_db::Db;

use crate::error::{details_of, ServiceError};

/// 账号 service。
pub struct AccountService {
    users: UserRepository,
    tokens: UserRefreshTokenRepository,
}

impl AccountService {
    pub fn new(db: &Db) -> Self {
        Self {
            users: UserRepository::new(db.clone()),
            tokens: UserRefreshTokenRepository::new(db.clone()),
        }
    }

    // ---------------------------------------------------------- 资料

    /// 读账号资料。
    ///
    /// 上游 `get_account` 只是把实体转成 resource —— 纯投影，没有规则。
    /// 收进 service 是为了不给路由层留「直接拿仓储」的口子。
    pub async fn get_account(&self, user_id: i32) -> Result<User, ServiceError> {
        self.users
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| Self::user_not_found(user_id))
    }

    /// 改用户名。
    pub async fn update_username(
        &self,
        user_id: i32,
        username: &str,
    ) -> Result<User, ServiceError> {
        let username = username.trim();
        if username.is_empty() {
            return Err(ServiceError::validation(
                "validation_error",
                "username cannot be blank",
            ));
        }
        // 先查重再写：唯一索引也会兜底并发，但那时错误是
        // `DbError::ConstraintViolation`（映射成 500），而不是客户端需要的 409。
        // 这里把「常见原因」提前拦成 409，并发那条窄路留给索引兜底。
        if let Some(existing) = self.users.find_by_username(username).await? {
            if existing.id != user_id {
                return Err(Self::username_taken(username));
            }
        }
        self.users
            .set_username(user_id, username)
            .await?
            .ok_or_else(|| Self::user_not_found(user_id))
    }

    // ---------------------------------------------------------- 密码

    /// 改密码。**成功后作废该用户全部 refresh token。**
    ///
    /// 顺序是三步且必须如此：验旧密码 → 写新哈希 → 作废会话。
    /// 中途失败绝不能让用户以为改成功了。
    pub async fn change_password(
        &self,
        user_id: i32,
        current_password: &str,
        new_password: &str,
    ) -> Result<(), ServiceError> {
        let user = self
            .users
            .find_by_id(user_id)
            .await?
            .ok_or_else(|| Self::user_not_found(user_id))?;

        // 旧密码错 → 401 `invalid_credentials`，与登录同一套错误码：
        // 客户端不需要区分「登录失败」与「改密时旧密码错」。
        verify_password(current_password, &user.password_hash).map_err(|_| {
            ServiceError::unauthorized("invalid_credentials", "Current password is incorrect")
        })?;

        // 上游不校验新密码强度（`bcrypt.hashpw` 不管强度），这里也不加 ——
        // 加了就与上游的接受集合不一致，而收紧密码策略是产品决策不是移植决策。
        let fresh = hash_password(new_password)
            .map_err(|err| ServiceError::validation("validation_error", err.to_string()))?;
        self.users.set_password_hash(user_id, &fresh).await?;

        // 必须在写哈希**之后**：先吊销再写的话，万一写失败用户既改了密码
        // 又被登出（两件坏事同时发生）；反过来最坏情况是「密码已改但旧会话
        // 仍在」，而那本来就只存在于并发窗口里。
        let revoked = self
            .tokens
            .revoke_all_active(sm_db::common::time::now_utc())
            .await?;
        tracing::info!(user_id, revoked, "密码已修改，全部会话已作废");
        Ok(())
    }

    // ---------------------------------------------------------- 错误

    fn user_not_found(user_id: i32) -> ServiceError {
        ServiceError::not_found("user_not_found", "User not found", "user_id", user_id)
    }

    fn username_taken(username: &str) -> ServiceError {
        ServiceError::conflict(
            "username_conflict",
            "Username already exists",
            Some(details_of("username", username)),
        )
    }
}
