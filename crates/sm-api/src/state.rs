//! 路由层共享状态。

use sm_db::Db;
use sm_service::system::auth::AuthConfig;

/// 所有路由共享的运行时状态。
///
/// # 为什么 `AuthConfig` 放在这里而不是全局
///
/// 上游从 `settings.auth` 读，是进程级单例。Rust 侧如果照搬成 `static`，
/// 测试就没法为每个用例注入不同密钥或不同有效期 —— 而「验签失败」和
/// 「令牌刚签发就过期」正是必须测的路径。放进 state 后，
/// `AppState::new(pool, AuthConfig::new("other-secret"))` 一行就能构造出来。
///
/// `Db` 是 `PgPool`，克隆是 `Arc` 计数而非新建连接，所以 `Clone` 很便宜。
#[derive(Clone)]
pub struct AppState {
    db: Db,
    auth: AuthConfig,
}

impl AppState {
    pub fn new(db: Db, auth: AuthConfig) -> Self {
        Self { db, auth }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn auth(&self) -> &AuthConfig {
        &self.auth
    }
}
