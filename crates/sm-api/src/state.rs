//! 路由层共享状态。

use std::sync::Arc;

use sm_db::Db;
use sm_service::discovery::ranking::RankingSourceCatalog;
use sm_service::error::ServiceError;
use sm_service::playback::media_library::{MediaLibraryRegistry, MediaLibraryService};
use sm_service::playback::provider_helpers::StorageGateway;
use sm_service::system::auth::AuthConfig;
use sm_service::system::config::ConfigService;
use sm_service::system::plugins::PluginAdmin;
use sm_service::system::JobCatalog;
use sm_service::transfers::download_client::DownloadClientService;

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
    /// provider 的**宿主侧工厂**（浏览 / 转存等走 `sm-plugin-api` 契约的操作）。
    ///
    /// 与 `storage` 同一个理由：**活的** `Option`，缺省 = 没装插件 → 调用方报
    /// 503 `provider_not_installed`。实现（`sm-plugins` 的
    /// `RegistryProviderFactory`）由组合根注入 —— 只有它看得见注册表。
    ///
    /// # 与 `storage` 的分工
    ///
    /// `storage` 是**数据面**（删远端文件、缩略图，走 `sm-service` 自定的
    /// `StorageGateway` trait）；这个是**控制面**（浏览、转存，走
    /// `sm-plugin-api` 的 `HostProviderFactory` 契约）。两者都拿活的注册表，
    /// 插件重启换端点时不受影响。
    provider_factory: Option<Arc<dyn sm_plugin_api::host::HostProviderFactory>>,
    /// provider 的**播放投递能力**（`plan_playback` / `plan_merged_playback`）。
    ///
    /// 与 `storage` 同一个理由：**活的** `Option`，缺省 = 没装插件 → 503
    /// `provider_not_installed`。
    ///
    /// ★ 但**能力缺失**（插件装了、却不支持这个投递方式）**不是** 503 —— 那是
    /// `ProviderFailure.code == "unsupported"`：调用方要**换行为**（跳过 / 拒绝），
    /// 不是重试。见 `docs/adr/2026-10-08-provider-seam.md` D2。
    playback: Option<Arc<dyn sm_service::playback::provider_helpers::PlaybackGateway>>,
    /// provider 的**下载能力**（`config_fields` / `prepare_client` / `test_client`）。
    ///
    /// 与 `storage` 同一个理由：**活的** `Option`，缺省 = 没装插件 → 写方法报
    /// 503 `provider_not_installed`（不是「假装配置合法」）。
    downloads: Option<Arc<dyn sm_service::transfers::download_client::DownloadCapabilityRegistry>>,
    /// provider 的**媒体库能力**（`library_config_fields` / `prepare_library`）。
    ///
    /// 与 `downloads` 同一个理由：**活的** `Option`，缺省 = 没装插件 → 写方法报
    /// 503 `provider_not_installed`、provider 目录返回空表（不是「假装配置合法」）。
    media_libraries: Option<Arc<dyn MediaLibraryRegistry>>,
    /// 插件**管理**（列表 / 详情 / 启停 / 安装 / 卸载）。
    ///
    /// 与 `storage` / `downloads` / `media_libraries` 同一套理由：实现要碰
    /// 插件目录与配置，只有组合根看得见 `sm-plugins`。
    ///
    /// # 与上面三个的一处差别：`None` 不是「功能不可用」
    ///
    /// 那三个是**能力**（没装 provider 插件 = 真的没有这个能力），而这个是
    /// **管理通道**：它永远应该可用，`None` 只意味着组合根漏了接线。
    /// 所以读它的地方用 [`AppState::plugin_admin`]，它会把 `None` 报成 500
    /// 而不是让插件页面空着。
    plugins: Option<Arc<dyn PluginAdmin>>,
    /// 影片相似度的 Qdrant 存储。`None` = **没启用**（`movie_similarity_enabled`
    /// 为假）或端点没配。
    ///
    /// # 为什么是「活的」而不是快照
    ///
    /// 它内部缓存「别名是否就绪」，而别名会被原子替换 —— 每次请求现造一个
    /// store 会让那个缓存永远从 false 开始，于是**每次请求都多探一次就绪**。
    ///
    /// `None` 的语义是「这台机器没开影片相似度」，调用方据此**返回空列表**
    /// 而不是报错（上游的降级语义，见
    /// `sm_service::discovery::recommendation::search_similar_movies`）。
    similarity: Option<Arc<sm_service::discovery::qdrant::similarity::MovieSimilarityStore>>,
    /// 图搜（以图搜图 / 以文搜图）的**检索**服务。`None` = 没启用。
    ///
    /// # 与 `similarity` 的一处关键差别：`None` 要报 **409** 而不是降级
    ///
    /// `similarity` 是**推荐信号**之一，读不到就少一个理由、别的照常，所以
    /// 它 `None` 时端点返回空列表。而图搜是**用户主动发起的整个功能**：上游
    /// 把 `require_image_search()` 挂成了 **router 级依赖**
    /// （`image_search.py:30`），未启用时六个端点统一 409 `feature_disabled`
    /// （`optional_services.py:22-24`）—— 不是「返回空结果」。
    ///
    /// 返回空结果会让用户以为「搜过了、没有」，而真相是「这台机器没开」。
    ///
    /// # 为什么是「活的」而不是每次请求构造
    ///
    /// 它持有 `DenseStore::connect` 出来的 Qdrant 连接与 `EmbeddingClient`
    /// 的连接池 —— 两者都是**网络资源**。每次请求现造等于每次请求重新建连，
    /// 与 `similarity` 那条注释同源。
    image_search: Option<Arc<sm_service::discovery::image_search::ImageSearchService>>,
    /// 元数据搜索（人工重试的候选来源：JavDB + 启用的插件源）。
    /// **组合根装配**；`None` = 插件平台没起。见 [`Self::metadata_search`]。
    metadata_search:
        Option<Arc<sm_service::catalog::movie_metadata_search::MovieMetadataSearchService>>,
    /// 元数据刷新（覆盖式，覆盖式刷新端点用）。**组合根装配**；`None`
    /// = 插件平台没起。见 [`Self::metadata_refresh`]。
    metadata_refresh:
        Option<Arc<sm_service::catalog::movie_metadata_refresh::MovieMetadataRefreshService>>,
    /// 演员搜索流式导入（`POST /actors/search/javdb/stream` 用）。**组合根装配**；
    /// `None` = 插件平台没起。见 [`Self::actor_javdb_stream`]。
    actor_javdb_stream:
        Option<Arc<sm_service::catalog::actor_javdb_stream::ActorJavdbStreamService>>,
    /// 剧情图搜的检索服务。理由与 `image_search` 完全一致（同一组 router 依赖）。
    plot_image_search:
        Option<Arc<sm_service::discovery::plot_image_search::MoviePlotImageSearchService>>,
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
            provider_factory: None,
            playback: None,
            downloads: None,
            media_libraries: None,
            plugins: None,
            similarity: None,
            image_search: None,
            plot_image_search: None,
            metadata_search: None,
            metadata_refresh: None,
            actor_javdb_stream: None,
        }
    }

    /// 挂上插件管理。**只有组合根会调** —— 只有它看得见 `sm-plugins`。
    pub fn with_plugin_admin(mut self, admin: Arc<dyn PluginAdmin>) -> Self {
        self.plugins = Some(admin);
        self
    }

    /// 插件管理。
    ///
    /// # 没注入时**报错**，不返回空列表
    ///
    /// 返回空列表会让「组合根忘了接线」与「真的一台插件都没装」长得一模一样，
    /// 而后者是正常的、前者是故障。区分它们只需要一个明确的错误码
    /// （`plugin_admin_unavailable`）。
    pub fn plugin_admin(&self) -> Result<&dyn PluginAdmin, ServiceError> {
        self.plugins
            .as_deref()
            .ok_or_else(sm_service::system::plugins::plugin_admin_unavailable)
    }

    /// 挂上影片相似度存储。**只有组合根会调**。
    pub fn with_movie_similarity(
        mut self,
        store: Arc<sm_service::discovery::qdrant::similarity::MovieSimilarityStore>,
    ) -> Self {
        self.similarity = Some(store);
        self
    }

    /// 影片相似度存储。`None` = 没启用 —— 调用方返回**空列表**，不报错。
    pub fn movie_similarity(
        &self,
    ) -> Option<&Arc<sm_service::discovery::qdrant::similarity::MovieSimilarityStore>> {
        self.similarity.as_ref()
    }

    /// 挂上图搜检索服务。**只有组合根会调**。
    pub fn with_image_search(
        mut self,
        service: Arc<sm_service::discovery::image_search::ImageSearchService>,
    ) -> Self {
        self.image_search = Some(service);
        self
    }

    /// 挂上元数据搜索。**只有组合根会调** —— 只有它看得见插件注册表。
    pub fn with_metadata_search(
        mut self,
        search: Arc<sm_service::catalog::movie_metadata_search::MovieMetadataSearchService>,
    ) -> Self {
        self.metadata_search = Some(search);
        self
    }

    /// 挂上元数据刷新。**只有组合根会调**。
    pub fn with_metadata_refresh(
        mut self,
        refresh: Arc<sm_service::catalog::movie_metadata_refresh::MovieMetadataRefreshService>,
    ) -> Self {
        self.metadata_refresh = Some(refresh);
        self
    }

    /// 元数据刷新服务。未装配 → **503 `provider_not_installed`**（理由同
    /// [`Self::metadata_search`]）。
    pub fn metadata_refresh(
        &self,
    ) -> Result<
        &sm_service::catalog::movie_metadata_refresh::MovieMetadataRefreshService,
        ServiceError,
    > {
        self.metadata_refresh
            .as_deref()
            .ok_or_else(sm_service::system::plugins::provider_not_installed)
    }

    /// 挂上演员搜索流式导入。**只有组合根会调**。
    pub fn with_actor_javdb_stream(
        mut self,
        stream: Arc<sm_service::catalog::actor_javdb_stream::ActorJavdbStreamService>,
    ) -> Self {
        self.actor_javdb_stream = Some(stream);
        self
    }

    /// 演员搜索流式导入服务。未装配 → **503 `provider_not_installed`**（理由
    /// 同 [`Self::metadata_refresh`]：它依赖插件平台，平台没起时这个能力不
    /// 存在 —— 而不是「搜了没有」）。
    pub fn actor_javdb_stream(
        &self,
    ) -> Result<&sm_service::catalog::actor_javdb_stream::ActorJavdbStreamService, ServiceError>
    {
        self.actor_javdb_stream
            .as_deref()
            .ok_or_else(sm_service::system::plugins::provider_not_installed)
    }

    /// 元数据搜索服务。
    ///
    /// 未装配 → **503 `provider_not_installed`**：它依赖插件平台，平台没起时
    /// 这个能力就不存在 —— 而不是「搜了没有」。
    ///
    /// ★ 与 [`Self::plugin_admin`] 的 500 `plugin_admin_unavailable` **不是
    /// 同一件事**：那个是组合根漏了接线（重试无用），这里是能力当前不可用。
    /// 2026-10-09 之前这三处复用前者，而文档写着 503 —— 测试（
    /// `media_import_retry_http.rs` 的 `without_the_search_service_the_route_is_503`）
    /// 把差别暴露出来后改成 503。
    pub fn metadata_search(
        &self,
    ) -> Result<&sm_service::catalog::movie_metadata_search::MovieMetadataSearchService, ServiceError>
    {
        self.metadata_search
            .as_deref()
            .ok_or_else(sm_service::system::plugins::provider_not_installed)
    }

    /// 图搜检索服务。`None` = 未启用。
    ///
    /// 调用方**必须**报 409 `feature_disabled`，不能降级成空结果 —— 理由见
    /// 字段的文档（上游是 router 级依赖，六个端点统一拒绝）。
    pub fn image_search(
        &self,
    ) -> Option<&Arc<sm_service::discovery::image_search::ImageSearchService>> {
        self.image_search.as_ref()
    }

    /// 挂上剧情图搜检索服务。只有组合根会调。
    pub fn with_plot_image_search(
        mut self,
        service: Arc<sm_service::discovery::plot_image_search::MoviePlotImageSearchService>,
    ) -> Self {
        self.plot_image_search = Some(service);
        self
    }

    /// 剧情图搜检索服务。`None` = 未启用 —— 同 [`Self::image_search`]，报 409。
    pub fn plot_image_search(
        &self,
    ) -> Option<&Arc<sm_service::discovery::plot_image_search::MoviePlotImageSearchService>> {
        self.plot_image_search.as_ref()
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

    /// 挂上 provider 宿主侧工厂。**只有组合根会调** —— 理由与
    /// `with_storage_gateway` 一致。
    pub fn with_provider_factory(
        mut self,
        factory: Arc<dyn sm_plugin_api::host::HostProviderFactory>,
    ) -> Self {
        self.provider_factory = Some(factory);
        self
    }

    /// provider 宿主侧工厂。`None` = 没装插件 —— 调用方据此报 503
    /// `provider_not_installed`（service 层各方法的既有语义）。
    pub fn provider_factory(&self) -> Option<Arc<dyn sm_plugin_api::host::HostProviderFactory>> {
        self.provider_factory.clone()
    }

    /// 挂上播放投递能力。**只有组合根会调** —— 理由与 `with_storage_gateway` 一致。
    pub fn with_playback_gateway(
        mut self,
        playback: Arc<dyn sm_service::playback::provider_helpers::PlaybackGateway>,
    ) -> Self {
        self.playback = Some(playback);
        self
    }

    /// provider 的**播放投递能力**（`play_media` / `play_merged_media` 要用）。
    ///
    /// `None` = 组合根没注入（等价于「没装任何插件」）→ 调用方报 503
    /// `provider_not_installed`。**别让路由自己拼** —— 拼漏了的表现是「播放一律
    /// 503」，而不是「缺哪个插件报哪个错」。
    pub fn playback_gateway(
        &self,
    ) -> Option<&Arc<dyn sm_service::playback::provider_helpers::PlaybackGateway>> {
        self.playback.as_ref()
    }

    /// 挂上下载能力。**只有组合根会调** —— 理由与上面那条完全一致。
    pub fn with_download_capabilities(
        mut self,
        downloads: Arc<dyn sm_service::transfers::download_client::DownloadCapabilityRegistry>,
    ) -> Self {
        self.downloads = Some(downloads);
        self
    }

    /// 挂上媒体库能力。**只有组合根会调** —— 只有它看得见 `sm-plugins`。
    pub fn with_media_library_registry(mut self, registry: Arc<dyn MediaLibraryRegistry>) -> Self {
        self.media_libraries = Some(registry);
        self
    }

    /// ★ 媒体库服务。**路由不要自己拼** —— 拼漏了注入的表现是：`POST` / 改配置
    /// **一律 503**、provider 目录**空表**，而不是「缺哪个插件报哪个错」。
    pub fn media_library_service(&self) -> MediaLibraryService {
        match self.media_libraries.as_ref() {
            Some(registry) => {
                MediaLibraryService::new_with_registry(self.db(), Arc::clone(registry))
            }
            None => MediaLibraryService::new(self.db()),
        }
    }

    /// ★ 下载器客户端服务。**路由不要自己拼** —— 拼漏了 `with_downloads` 的表现是
    /// 三个写方法**一律 503**，而不是「缺哪个插件就哪个报错」，排查时极难分清。
    pub fn download_client_service(
        &self,
    ) -> sm_service::transfers::download_client::DownloadClientService {
        match self.downloads.as_ref() {
            Some(_) => {
                let registry = Arc::clone(self.downloads.as_ref().expect("刚匹配到 Some"));
                DownloadClientService::new_with_downloads(self.db(), registry)
            }
            None => DownloadClientService::new(self.db()),
        }
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
        let service = match self.storage_gateway() {
            Some(gateway) => service.with_gateway(Arc::clone(gateway)),
            None => service,
        };
        // ★ 两条缝**都要接**。少接播放这条的症状极隐蔽：端点能编译、能返回
        // 200，只在真播时 503 `provider_not_installed` —— 与「没装插件」长得
        // 一模一样。这正是上面那段文档说的分叉，只是多了一条缝。
        match self.playback_gateway() {
            Some(gateway) => service.with_playback_gateway(Arc::clone(gateway)),
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
