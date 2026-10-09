//! 路由层共享状态。

use std::sync::Arc;

use sm_db::Db;
use sm_service::discovery::ranking::RankingSourceCatalog;
use sm_service::playback::provider_helpers::StorageGateway;
use sm_service::system::auth::AuthConfig;
use sm_service::system::config::ConfigService;
use sm_service::system::JobCatalog;

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
    /// 任务目录（内建 + 插件）。**快照**：插件表由组合根持有，而 API 层不能
    /// 反向依赖组合根，所以只在插件加载/重建之后换一份新的。
    ///
    /// 缺省为空目录 —— 只有组合根会填它，而没填时「任务中心」应当什么都没有，
    /// 而不是把内建任务凭空编出来。
    jobs: JobCatalog,
    /// 排行源目录（来自插件注册表）。**快照**，理由与 `jobs` 完全一样：
    ///
    /// `sm-plugins -> sm-scheduler -> sm-service` 已是一条链，所以 `sm-service`
    /// 与 `sm-api` 都**不能**依赖 `sm-plugins`（前者会成环，后者本来就不
    /// 依赖）。只有组合根 `sm-server` 读得到注册表，所以它读完塞进来。
    ///
    /// 缺省为空 —— 没装排行插件时 `GET /ranking-sources` 返回空列表，与
    /// 「装了插件但没配排行源」表现一致。
    ranking: RankingSourceCatalog,
    /// provider **数据面**（删远端文件、生成缩略图）。
    ///
    /// # 与上面两个快照不同：这是**活的**
    ///
    /// `jobs` / `ranking` 是加载期转好的快照，而这个必须指向**活的**注册表：
    /// 插件重启会换控制面端口（见 `sm_server::plugins::Plugins::provider_registry`
    /// 的文档），快照会带着旧端点继续发请求。
    ///
    /// `Option` 而不是必填：**没注入 = 一个插件都没装**，调用方据此报 503
    /// `provider_not_installed`（`require_provider` 的文档）。缺省就是它 ——
    /// 单测里不注入也能构造 `AppState`。
    storage: Option<Arc<dyn StorageGateway>>,
}

impl AppState {
    /// 三个参数缺一不可：`config` 也要，因为 `PATCH /config` 没有它就没法
    /// 写回文件，而「写不进文件」的表现是「改了没反应」，最难排查。
    pub fn new(db: Db, auth: AuthConfig, config: ConfigService) -> Self {
        Self {
            db,
            auth,
            config,
            jobs: JobCatalog::default(),
            ranking: RankingSourceCatalog::default(),
            storage: None,
        }
    }

    /// 挂上任务目录。只有组合根会调 —— 它才知道有哪些插件任务。
    pub fn with_jobs(mut self, jobs: JobCatalog) -> Self {
        self.jobs = jobs;
        self
    }

    /// 任务目录。
    /// 挂上排行源目录。**只有组合根会调。**
    pub fn with_ranking_sources(mut self, ranking: RankingSourceCatalog) -> Self {
        self.ranking = ranking;
        self
    }

    /// 排行源目录。
    pub fn ranking(&self) -> &RankingSourceCatalog {
        &self.ranking
    }

    /// 挂上 provider 数据面。**只有组合根会调** —— 只有它看得见 `sm-plugins`。
    pub fn with_storage_gateway(mut self, storage: Arc<dyn StorageGateway>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// provider 数据面。`None` 表示组合根没注入（等价于「没装任何插件」）。
    ///
    /// 返回 `Arc` 而不是 `&dyn`：调用方常常要把它**交给一个服务持有**
    /// （`MediaService::with_gateway`），那时需要克隆所有权。只需借用时用
    /// `.map(|gateway| gateway.as_ref())`。
    pub fn storage_gateway(&self) -> Option<&Arc<dyn StorageGateway>> {
        self.storage.as_ref()
    }

    /// 媒体服务。**每个路由在家自己拼**，别在这里缓存。
    ///
    /// # 为什么放在 `AppState` 上
    ///
    /// 「`MediaService` 要怎么拼」有两个容易漏的点：它要 `Db` + `ConfigService`，
    /// 还要**把 provider 网关接上**（不接就是「没装插件」，删除链路第一步 503）。
    /// 两处（`routes::media` 与 `routes::videos`，后者要删条目）各写一遍就会在
    /// 「谁记得 `with_gateway`」上分叉 —— 忘了的那一处，行为是「文件删不掉但
    /// 不报错」那种最难查的形态。
    ///
    /// 不能缓存成字段：`Arc<dyn StorageGateway>` 每次都要克隆进去，而
    /// `MediaService` 本身是廉价的值类型（几个仓储 + 两个 `Arc`）。
    pub fn media_service(&self) -> sm_service::playback::media::MediaService {
        let service = sm_service::playback::media::MediaService::new(self.db(), self.config());
        match self.storage_gateway() {
            Some(gateway) => service.with_gateway(Arc::clone(gateway)),
            None => service,
        }
    }

    pub fn jobs(&self) -> &JobCatalog {
        &self.jobs
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
