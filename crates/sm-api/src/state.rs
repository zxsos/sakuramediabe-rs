//! 路由层共享状态。

use sm_db::Db;
use sm_service::system::auth::AuthConfig;
use sm_service::system::config::ConfigService;

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
    /// 配置服务。
    ///
    /// # 为什么状态里放服务而不是路径
    ///
    /// 因为路径是**服务的内部状态**：读盘、合并、原子写盘都围着它转。状态里
    /// 只放路径的话，每个 handler 都要现造一个服务，而测试要指向临时目录 ——
    /// 那就得把路径也做成参数，等于把同一件事拆成两处。
    ///
    /// `ConfigService` 的 `Clone` 只复制一个 `PathBuf`，很便宜。
    config: ConfigService,
}

impl AppState {
    /// 三个参数缺一不可：`config` 也要，因为 `PATCH /config` 没有它就没法
    /// 写回文件，而「写不进文件」的表现是「改了没反应」，最难排查。
    pub fn new(db: Db, auth: AuthConfig, config: ConfigService) -> Self {
        Self { db, auth, config }
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    pub fn auth(&self) -> &AuthConfig {
        &self.auth
    }

    /// 配置服务。`PATCH /config` 通过它写盘。
    pub fn config(&self) -> &ConfigService {
        &self.config
    }
}
