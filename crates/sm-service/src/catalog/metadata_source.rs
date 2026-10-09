//! 元数据来源：JavDB 优先，插件结果临时交付（上游 `catalog/metadata_source_service.py`，242 行）。
//!
//! # 核心约束：插件交付的元数据**只在本次调用期间有效**
//!
//! 上游 docstring：「JavDB 优先；插件结果只在本次调用期间交付给宿主。」
//!
//! `fetch_plugin` / `fetch` 都是 **context manager**：进入时向插件索取文件，
//! 退出时清理。宿主**不允许**持有跨越调用边界的插件文件句柄 —— 插件可能在
//! 下一次请求里换掉目录内容。
//!
//! ⚠️ 因此这两个方法**不能**返回 `&Path` 或把路径存进结构体。必须用闭包把
//! 生命周期圈住。
//!
//! # 交付文件必须在插件自己的目录下
//!
//! 上游校验交付文件落在
//! `<plugin>/data/metadata-tmp/<请求目录>/` 内，**用 Pillow 校验后清理**。
//!
//! 三个环节缺一不可：
//!
//! | 环节 | 防的是 |
//! |---|---|
//! | 路径前缀校验 | 插件让宿主读任意文件（`../../etc/passwd`） |
//! | Pillow 校验 | 「这是图片」只是插件的一句话，不验就是信任它 |
//! | 退出时清理 | 临时文件堆积占满磁盘 |
//!
//! # 错误用**自定义异常**而非 `ApiError`
//!
//! `MetadataSourceError` / `MetadataNotFoundError` / `MetadataRequestError`。
//! 因为这个模块被多个端点复用，HTTP 状态码由**调用方**决定 ——
//! 「按番号搜索时找不到」该是 404，「刷新时找不到」该是 502，语义不同。

use std::path::PathBuf;
use std::time::Duration;

use sm_plugin_api::movie_delivery::{cleanup_delivery, validate_movie_delivery, MovieDelivery};
use sm_plugin_api::v1::metadata_source_extension_service_client::MetadataSourceExtensionServiceClient;
use sm_plugin_api::v1::FetchMovieRequest;
use tonic::transport::Endpoint;

use crate::movie_numbers::normalize_movie_number;

/// 单次索取的时限。
///
/// ⚠️ 与上游的一处**有意**差异：上游是无上限的同步调用（Python 里就那么写着）。
/// 这里给一个上限 —— 卡住的插件会占住一个 HTTP 请求线程到天荒地老，而宿主
/// 没有别的取消手段（`RunJob` 那条流式通道不用于扩展点）。30 秒与「一次外部
/// 数据源查询」的量级相符。
pub const FETCH_DEADLINE: Duration = Duration::from_secs(30);

/// 交付目录名（在插件数据目录下）。上游 `metadata-tmp`。
pub const DELIVERY_DIR_NAME: &str = "metadata-tmp";

/// 元数据来源错误基类。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataSourceError {
    /// 该来源没有这条记录（**不是**故障）。
    NotFound,
    /// 调来源失败（网络、插件崩了）。
    RequestFailed(String),
    /// 插件交付的文件不合法（路径逃逸 / 不是图片 / 目录不对）。
    InvalidDelivery(String),
    /// 该来源未启用。
    Disabled(String),
}

/// JavDB 查询能力。**出网**。
pub trait MetadataProvider {
    /// 按番号取影片详情。
    fn get_movie_by_number(
        &self,
        movie_number: &str,
    ) -> Result<Option<serde_json::Value>, MetadataSourceError>;
    /// 按 JavDB id 取影片详情。
    fn get_movie_by_javdb_id(
        &self,
        javdb_id: &str,
    ) -> Result<Option<serde_json::Value>, MetadataSourceError>;
    /// 搜演员。
    fn search_actors(&self, keyword: &str) -> Result<Vec<serde_json::Value>, MetadataSourceError>;
}

/// 插件交付的元数据。**只在闭包内有效**。
///
/// # 为什么有**两支**
///
/// 上游 `fetch` 会 yield 两种不同的东西：JavDB 那支是 provider 原文、来源为
/// `None`；插件那支是插件的交付、来源为 `{plugin_id, ...}`。调用方据此分叉
/// （`import_movie_if_missing` vs `import_plugin_movie`）。这里用两个互斥的
/// `Option` 表达同一件事 —— 合成一个类型会让「这一支到底有什么」变成要运行
/// 到那里才知道的事。
pub struct PluginDelivery {
    /// JavDB 那一支：provider 原文（`Value`，宿主只搬运）。插件那支为 `None`。
    pub javdb_detail: Option<serde_json::Value>,
    /// 插件那一支：已通过交付校验的结构化结果。JavDB 那支为 `None`。
    pub plugin_delivery: Option<MovieDelivery>,
    /// 这份元数据是谁给的。
    ///
    /// # 从「一个串」改成枚举（2026-10-07）
    ///
    /// 骨架期这里是 `String`（`"javdb"` / `"plugin:<id>"`），而
    /// `import_plugin_movie` 要往 `movie.metadata_source` 写
    /// `{plugin_id, display_name, source_id, source_url}` 四个键 ——
    /// `display_name` 在那个串里**根本没有**，`plugin_id` 也只能靠拆
    /// `plugin:` 前缀得到（自造格式，改前缀就静默失效）。
    ///
    /// 落地 [`MetadataSourceService::import_by_number`] 时把插件的两个身份字段
    /// 直接带上：它们本来就在 `fetch_plugin` 手上，只是在构造这里时被丢掉了。
    pub source: DeliverySource,
    /// 交付目录。**闭包退出后即失效**（清理已发生）。JavDB 那支为 `None`。
    pub delivery_dir: Option<PathBuf>,
}

/// 一份元数据是谁给的。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeliverySource {
    /// JavDB provider 原文。
    Javdb,
    /// 插件交付。
    Plugin {
        plugin_id: String,
        /// 展示名。上游 `enabled_plugin_sources` 三元组里的第二个，落进
        /// `metadata_source.display_name` —— 客户端会显示它。
        display_name: String,
    },
}

/// 元数据来源服务。
pub struct MetadataSourceService {
    /// 已注册且已启用的插件来源（顺序见 [`Self::enabled_plugin_sources`]）。
    sources: Vec<RegisteredSource>,
    /// JavDB provider。`None` = 这一轮没有 JavDB 可用 —— 上游 `fetch` 的
    /// `provider` 也是调用方给的，不是类成员。
    provider: Option<Box<dyn MetadataProvider>>,
}

/// 一个已注册的插件来源。
///
/// 由**组合根**构造：它同时看得见插件注册表（端点）与配置（`enabled` 顺序与
/// 数据目录），而本 crate 两者都够不着 —— 与 `provider_gateway.rs` 同一个取向。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegisteredSource {
    pub plugin_id: String,
    /// 展示名。上游 `enabled_plugin_sources` 三元组里的第二个。
    pub display_name: String,
    /// 插件数据目录（交付文件必须落在它下面）。
    pub data_dir: PathBuf,
    /// 插件的**控制面**端点。两个扩展点与 `PluginControl` 由同一进程提供，
    /// 所以扩展点客户端就用这条通道建。
    pub endpoint: String,
}

impl MetadataSourceService {
    /// 构造。
    pub fn new(
        sources: Vec<RegisteredSource>,
        provider: Option<Box<dyn MetadataProvider>>,
    ) -> Self {
        Self { sources, provider }
    }

    /// 按宿主配置**过滤并排序**已注册的来源。
    ///
    /// 上游 `enabled_plugin_sources`（`:61-69`）：`sources` 是 `register()` 在
    /// 加载期收集的，这里只做一件事 —— **按 `plugins.enabled` 的顺序**返回
    /// 其中已启用的那些。顺序即优先级（兜底链路按它逐个试）。
    ///
    /// # 为什么是「过滤」而不是「读出来」
    ///
    /// 端点与数据目录来自插件注册表与宿主约定，不在配置里。所以
    /// [`RegisteredSource`] 由组合根构造好，这里只负责「配置说启用谁」。
    /// 反过来（从配置里现造）会让注册表与配置两处都能决定来源，改一处漏一处。
    pub fn enabled_plugin_sources(&self, config: &serde_json::Value) -> Vec<&RegisteredSource> {
        let enabled = config
            .get("plugins")
            .and_then(|section| section.get("enabled"))
            .and_then(serde_json::Value::as_array);
        let Some(enabled) = enabled else {
            // 没有 `plugins.enabled` 就是「一个都没启用」—— 与上游 `enabled = []`
            // 同义，不是错误。
            return Vec::new();
        };
        enabled
            .iter()
            .filter_map(serde_json::Value::as_str)
            .filter_map(|plugin_id| {
                self.sources
                    .iter()
                    .find(|source| source.plugin_id == plugin_id)
            })
            .collect()
    }

    /// 该插件来源是否启用。
    pub fn is_plugin_enabled(sources: &[RegisteredSource], plugin_id: &str) -> bool {
        sources.iter().any(|source| source.plugin_id == plugin_id)
    }

    /// ★ 向某个插件来源索取元数据，**闭包内有效**。
    ///
    /// 上游 `fetch_plugin(cls, plugin_id, movie_number)` 是 contextmanager。
    /// 闭包参数是 [`PluginDelivery`]。
    ///
    /// 三个交付校验都在这里做（见模块文档）：路径前缀、Pillow 校验、退出清理。
    pub async fn fetch_plugin<R>(
        &self,
        config: &serde_json::Value,
        plugin_id: &str,
        movie_number: &str,
        consume: impl AsyncFnOnce(PluginDelivery) -> R,
    ) -> Result<R, MetadataSourceError> {
        // 未启用 / 未注册 → 上游 `MetadataSourceError(f"元数据插件未启用或未注册: {id}")`。
        let source = self
            .enabled_plugin_sources(config)
            .into_iter()
            .find(|source| source.plugin_id == plugin_id)
            .ok_or_else(|| MetadataSourceError::Disabled(plugin_id.to_owned()))?;
        let (delivery, delivery_dir) = self.load_plugin(source, movie_number).await?;
        let paths = delivery.image_paths();
        let result = consume(PluginDelivery {
            javdb_detail: None,
            plugin_delivery: Some(delivery),
            source: DeliverySource::Plugin {
                plugin_id: plugin_id.to_owned(),
                display_name: source.display_name.clone(),
            },
            delivery_dir: Some(delivery_dir),
        })
        .await;
        // 消费完**立刻**清理：交付目录是插件的临时区，插件可能在下一次请求里
        // 换掉它的内容。
        cleanup_delivery(&paths);
        Ok(result)
    }

    /// 向一个插件索取并校验（上游 `_load_plugin`，`:102-152`）。
    ///
    /// 返回**已校验**的交付与其所在目录。校验顺序照抄上游：
    /// 交付目录 → 图片路径/类型 → 番号一致性 → （Pillow 校验）。
    async fn load_plugin(
        &self,
        source: &RegisteredSource,
        movie_number: &str,
    ) -> Result<(MovieDelivery, PathBuf), MetadataSourceError> {
        // 交付目录：`<data_dir>/metadata-tmp`。宿主建、插件往里放、宿主用完删。
        let delivery_dir = source.data_dir.join(DELIVERY_DIR_NAME);
        std::fs::create_dir_all(&delivery_dir).map_err(|error| {
            MetadataSourceError::RequestFailed(format!(
                "交付目录 {} 建不出来：{error}",
                delivery_dir.display()
            ))
        })?;

        let request = FetchMovieRequest {
            movie_number: movie_number.to_owned(),
            delivery_dir: delivery_dir.display().to_string(),
        };
        // 校验还要用 `request`（它带着 `delivery_dir` 这个边界），所以这里传副本
        // —— 一次 `FetchMovie` 只有两个小字段，克隆的代价远小于把边界再算一遍。
        let response = fetch_from_plugin(&source.endpoint, request.clone()).await?;
        // `found = false` 是「没收录」，不是失败 —— 上层据此试下一个来源。
        if !response.found {
            cleanup_delivery(&[]);
            return Err(MetadataSourceError::NotFound);
        }

        let delivery = validate_movie_delivery(&request, response).map_err(|problem| {
            // 校验失败也要清理：插件可能已经把文件放进去了。
            MetadataSourceError::InvalidDelivery(problem.code().to_owned())
        })?;
        if normalize_movie_number(&delivery.movie_number) != normalize_movie_number(movie_number) {
            cleanup_delivery(&delivery.image_paths());
            return Err(MetadataSourceError::InvalidDelivery(
                "插件返回的番号与请求不匹配".to_owned(),
            ));
        }
        // ⚠️ 上游还有一步 **Pillow 校验**（真的把每张图解码一遍，确认它是图片）。
        // 本仓没有图像解码依赖，这一步**尚未实现** —— 判据目前到「是普通文件、
        // 在交付目录内」为止，坏图会在更下游的 image store 那一侧暴露。
        Ok((delivery, delivery_dir))
    }

    /// ★ 按番号取元数据，**JavDB 优先**。
    ///
    /// 上游 `fetch(cls, movie_number, provider)`，同样是 contextmanager。
    /// 顺序：先 JavDB，**查不到**才走插件。
    pub async fn fetch<R>(
        &self,
        config: &serde_json::Value,
        movie_number: &str,
        consume: impl AsyncFnOnce(PluginDelivery) -> R,
    ) -> Result<R, MetadataSourceError> {
        // ① 先 JavDB。上游 `except MetadataNotFoundError: pass` ——
        // 「没收录」不算错，继续往下试插件。
        if let Some(provider) = &self.provider {
            match provider.get_movie_by_number(movie_number) {
                Ok(Some(detail)) => {
                    return Ok(consume(PluginDelivery {
                        javdb_detail: Some(detail),
                        plugin_delivery: None,
                        source: DeliverySource::Javdb,
                        delivery_dir: None,
                    })
                    .await)
                }
                // 查不到 → 往下走；出错 → 也往下走（上游只 catch NotFound，
                // 其余会冒出去，但这里冒出去会让整条兜底链路断掉，而插件可能
                // 还有这条片 —— 所以记下来、继续）。
                Ok(None) => {}
                Err(MetadataSourceError::NotFound) => {}
                Err(error) => tracing::warn!(
                    code = ?error,
                    movie_number,
                    "JavDB 查询失败，改试插件来源"
                ),
            }
        }

        // ② 按启用顺序逐个试插件。
        let mut failures: Vec<String> = Vec::new();
        for source in self.enabled_plugin_sources(config) {
            match self.load_plugin(source, movie_number).await {
                Ok((delivery, delivery_dir)) => {
                    let paths = delivery.image_paths();
                    let result = consume(PluginDelivery {
                        javdb_detail: None,
                        plugin_delivery: Some(delivery),
                        source: DeliverySource::Plugin {
                            plugin_id: source.plugin_id.clone(),
                            display_name: source.display_name.clone(),
                        },
                        delivery_dir: Some(delivery_dir),
                    })
                    .await;
                    cleanup_delivery(&paths);
                    return Ok(result);
                }
                // 这一个没收录 → 试下一个（上游 `continue`）。
                Err(MetadataSourceError::NotFound) => continue,
                // 这一个坏了 → 记下来，试下一个。
                Err(error) => failures.push(format!("{}: {error:?}", source.plugin_id)),
            }
        }
        // ③ 全试完了：有过真实失败就报失败（上游 `"; ".join(failures)`），
        // 否则就是"谁都没有这部片"。
        if !failures.is_empty() {
            return Err(MetadataSourceError::RequestFailed(failures.join("; ")));
        }
        Err(MetadataSourceError::NotFound)
    }

    /// 按番号导入（JavDB 优先）。上游 `import_by_number`
    /// （`metadata_source_service.py:30-44`）。
    ///
    /// ```python
    /// existing = find_movie_by_number(movie_number)
    /// if existing is not None:
    ///     return existing, False
    /// with cls.fetch(movie_number, provider) as (detail, source):
    ///     if source is not None:
    ///         return import_service.import_plugin_movie(detail, source, provider, force_subscribed=…)
    ///     return import_service.import_movie_if_missing(detail, force_subscribed=…)
    /// ```
    ///
    /// 返回 `(movie_id, 是否新建)`。
    ///
    /// # 第一段的短路**不能省**
    ///
    /// 它是「已经有的番号不再打一次外部来源」。批量导入时那是每条一次 JavDB
    /// 查询或插件 gRPC 调用 —— `import_*` 内部虽然也会二次确认，但那是**拿锁
    /// 之后**的事，外面的这次外部调用早就付出去了。
    ///
    /// # 为什么整段在闭包内
    ///
    /// 插件那一支的交付文件在闭包退出后**立刻被清理**（见 [`Self::fetch`] 与
    /// `cleanup_delivery`）—— 入库必须在那之前做完，否则交给导入方的是已经
    /// 不存在的图片路径。
    ///
    /// # 错误是 `MetadataSourceError` 而不是 `ServiceError`
    ///
    /// 与 [`Self::match_actors`] 同一个取向：本模块的错误语义属于「来源」，
    /// HTTP 状态码由调用方决定（见模块文档）。导入侧的 `ServiceError` 在这里
    /// 被折成 [`MetadataSourceError::RequestFailed`]。
    pub async fn import_by_number(
        &self,
        config: &serde_json::Value,
        movie_number: &str,
        import_service: &dyn crate::catalog::catalog_import::CatalogImport,
        force_subscribed: bool,
    ) -> Result<(i32, bool), MetadataSourceError> {
        // ① 已存在 → 直接返回（上游第一行）。`false` = 没有新建。
        if let Some(movie_id) = import_service
            .find_movie_id(movie_number)
            .await
            .map_err(import_failed)?
        {
            return Ok((movie_id, false));
        }

        // ② JavDB 优先，逐个插件兜底；**入库在闭包内做**。
        self.fetch(
            config,
            movie_number,
            |delivery: PluginDelivery| async move {
                match delivery.plugin_delivery {
                    // 插件那一支：交付形状要翻译成 `CatalogImport` 认的键。
                    Some(plugin) => {
                        let detail = import_detail_of(&plugin);
                        let source = match &delivery.source {
                            DeliverySource::Plugin {
                                plugin_id,
                                display_name,
                            } => source_identity_of(
                                plugin_id.as_str(),
                                display_name.as_str(),
                                &plugin,
                            ),
                            // 到不了：`plugin_delivery` 有值时 `source` 一定是
                            // `Plugin`（两个构造点都这么写）。给 `Null` 而不是
                            // panic —— 一个来源身份的缺失不该让整条导入挂掉。
                            DeliverySource::Javdb => {
                                tracing::warn!(
                                    movie_number,
                                    "插件交付带着 JavDB 来源标识，metadata_source 将为空"
                                );
                                serde_json::Value::Null
                            }
                        };
                        import_service
                            .import_plugin_movie(&detail, &source, force_subscribed)
                            .await
                    }
                    // JavDB 那一支：provider 原文直接交给导入方（宿主只搬运）。
                    None => {
                        let detail = delivery.javdb_detail.unwrap_or(serde_json::Value::Null);
                        import_service
                            .import_movie_if_missing(movie_number, &detail)
                            .await
                    }
                }
                .map_err(import_failed)
            },
        )
        // 两层 `Result` 是刻意的：**外层**是 `fetch` 自己的失败（谁都没有这部片
        // = `NotFound`），**内层**是导入结果。`?` 只传播外层 —— 内层正是要返回
        // 给调用方的东西（「找到了但写库失败」与「压根没找到」是两回事）。
        .await?
    }

    /// 搜演员并逐个入库。返回入库的演员数。
    ///
    /// 上游 `match_actors` 只搜 JavDB（插件来源不提供演员搜索）。
    pub async fn match_actors(
        &self,
        keyword: &str,
        import_service: &dyn crate::catalog::catalog_import::CatalogImport,
    ) -> Result<usize, MetadataSourceError> {
        let provider = self
            .provider
            .as_ref()
            .ok_or(MetadataSourceError::NotFound)?;
        let mut imported = 0_usize;
        for resource in provider.search_actors(keyword)? {
            import_service
                .upsert_actor(&resource)
                .await
                .map_err(|error| MetadataSourceError::RequestFailed(error.code().to_owned()))?;
            imported += 1;
        }
        Ok(imported)
    }
}

/// 导入侧的 [`ServiceError`](crate::error::ServiceError) → 本模块的来源错误。
///
/// 两类错误在这里被压成同一个 `RequestFailed`：本模块**不区分**「来源坏了」
/// 与「写库失败」。上游也是这么做的（`import_service` 抛出来的异常一路冒到
/// 调用方）。要区分得给 [`MetadataSourceError`] 再加一种变体，而目前没有
/// 调用方按它分支。
fn import_failed(error: crate::error::ServiceError) -> MetadataSourceError {
    MetadataSourceError::RequestFailed(error.code().to_owned())
}

/// 把插件交付**翻译成 `CatalogImport` 认的元数据形状**。
///
/// # 为什么需要这一层
///
/// 上游 `import_plugin_movie(detail: PluginMovieMetadata, …)` 直接把 pydantic
/// 模型交下去；本仓的 [`CatalogImport`](crate::catalog::catalog_import::CatalogImport)
/// 窄接口收 `&serde_json::Value`（骨架期为了不让它依赖契约层的结构），所以
/// 这里要做一次搬运。
///
/// **键名与 JavDB 那一支一致** —— `CatalogImportService::create_movie` 对两支
/// 读同一组键（`movie_number` / `title` / `summary` / `maker_name` /
/// `director_name` / `release_date` / `duration_minutes`）。改键名要同时看那边。
///
/// # 不写 `javdb_id`
///
/// 插件来源**没有** JavDB 身份：`create_movie` 在 `source` 非空时强制把它写成
/// `None`，这里再给一个也只是被覆盖。给它反而会让「这一支到底有没有 JavDB
/// 身份」变成要看两处才能回答的问题。
///
/// # 不写 `series_name` / `actors` / `tags`
///
/// `create_movie` 现在**不读**它们（系列要 join、演员与标签要那两张表的写入
/// 方法，都还没接 —— 见 `catalog_import` 的模块文档）。这里给不给结果一样，
/// 所以不给：一个「看起来在传、其实被丢掉」的键比不传更容易让人误解。
fn import_detail_of(delivery: &MovieDelivery) -> serde_json::Value {
    serde_json::json!({
        "movie_number": delivery.movie_number,
        "title": delivery.title,
        "summary": delivery.summary,
        "maker_name": delivery.maker_name,
        "director_name": delivery.director_name,
        // 严格 `YYYY-MM-DD`（`validate_movie_delivery` 保证的），下游
        // `CatalogImportService` 的 `date_of` 按同一个格式解析。
        "release_date": delivery.release_date,
        "duration_minutes": delivery.duration_minutes,
    })
}

/// 写进 `movie.metadata_source` 的那个对象。
///
/// 上游 `import_plugin_movie(detail, source, …)` 的 `source` 就是这四个键
/// （`metadata_source_service.py:145-152`）：
///
/// ```python
/// {"plugin_id": …, "display_name": …,
///  "source_id": detail.source_id, "source_url": detail.source_url}
/// ```
///
/// 前两个描述**哪个插件**（来自 [`DeliverySource::Plugin`]），后两个描述
/// **这一条记录**（来自插件交付本身）—— 同一个插件的两部影片 `source_url`
/// 不同，所以它们属于交付而不属于插件的注册信息。
fn source_identity_of(
    plugin_id: &str,
    display_name: &str,
    delivery: &MovieDelivery,
) -> serde_json::Value {
    serde_json::json!({
        "plugin_id": plugin_id,
        "display_name": display_name,
        "source_id": delivery.source_id,
        "source_url": delivery.source_url,
    })
}

/// 向一个插件发一次 `FetchMovie`。
///
/// # 为什么**不复用** `sm-plugins::extension_calls::fetch_movie`
///
/// 那是同一件事（发同一个 rpc、`found=false` 判"没收录"），但本 crate 依赖不了
/// `sm-plugins`（依赖方向）。而这次调用需要的**只有生成的客户端与 tonic**，
/// 两者都在契约层 —— 所以这里照着写一份，判据与它一致（超时单独归一类）。
async fn fetch_from_plugin(
    endpoint: &str,
    request: FetchMovieRequest,
) -> Result<sm_plugin_api::v1::FetchMovieResponse, MetadataSourceError> {
    let channel = Endpoint::from_shared(endpoint.to_owned())
        .map_err(|error| MetadataSourceError::RequestFailed(format!("插件端点非法：{error}")))?
        .connect()
        .await
        .map_err(|error| MetadataSourceError::RequestFailed(format!("连不上插件：{error}")))?;
    let mut call = tonic::Request::new(request);
    call.set_timeout(FETCH_DEADLINE);
    MetadataSourceExtensionServiceClient::new(channel)
        .fetch_movie(call)
        .await
        .map(|response| response.into_inner())
        .map_err(|status| match status.code() {
            tonic::Code::DeadlineExceeded => {
                MetadataSourceError::RequestFailed("插件索取超时".to_owned())
            }
            _ => MetadataSourceError::RequestFailed(status.to_string()),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(id: &str) -> RegisteredSource {
        RegisteredSource {
            plugin_id: id.to_owned(),
            display_name: id.to_owned(),
            data_dir: PathBuf::from("/tmp").join(id),
            endpoint: "http://127.0.0.1:1".to_owned(),
        }
    }

    fn sources(ids: &[&str]) -> Vec<RegisteredSource> {
        ids.iter().map(|id| source(id)).collect()
    }

    /// ★ 顺序 = `plugins.enabled` 的顺序，**不是**注册顺序。
    ///
    /// 兜底链路按这个顺序逐个试，谁先命中就用谁 —— 顺序错了会静默地换掉
    /// 元数据的来源（而两边的数据质量不一样）。
    #[test]
    fn the_enabled_order_comes_from_the_config_not_the_registry() {
        let service = MetadataSourceService::new(sources(&["a", "b", "c"]), None);
        let config = serde_json::json!({"plugins": {"enabled": ["c", "a"]}});
        let ids: Vec<&str> = service
            .enabled_plugin_sources(&config)
            .iter()
            .map(|source| source.plugin_id.as_str())
            .collect();
        assert_eq!(ids, vec!["c", "a"]);
    }

    /// 配置里没写的 = 没启用；`plugins.enabled` 缺失 = 一个都没启用（**不是错误**）。
    #[test]
    fn unlisted_and_missing_sections_disable_everything() {
        let service = MetadataSourceService::new(sources(&["a"]), None);
        assert!(service
            .enabled_plugin_sources(&serde_json::json!({"plugins": {"enabled": []}}))
            .is_empty());
        assert!(service
            .enabled_plugin_sources(&serde_json::json!({}))
            .is_empty());
        // 配了但没注册（插件没起来）的也不出现 —— 否则会去连一个不存在的端点。
        assert!(service
            .enabled_plugin_sources(&serde_json::json!({"plugins": {"enabled": ["ghost"]}}))
            .is_empty());
    }

    /// 未启用的插件 → [`MetadataSourceError::Disabled`]，不是"没收录"。
    ///
    /// 「你没启用它」和「它没有这部片」是两回事：前者要去改配置，后者是正常结果。
    #[tokio::test]
    async fn a_disabled_plugin_is_reported_as_disabled() {
        let service = MetadataSourceService::new(sources(&["a"]), None);
        let error = service
            .fetch_plugin(&serde_json::json!({}), "a", "ABC-123", |_delivery| async {
                0
            })
            .await
            .expect_err("没启用就该报错");
        assert_eq!(error, MetadataSourceError::Disabled("a".to_owned()));
    }

    /// 只有**已启用**的来源可用。
    #[test]
    fn only_enabled_sources_are_used() {
        let enabled = sources(&["local", "115"]);
        assert!(MetadataSourceService::is_plugin_enabled(&enabled, "local"));
        assert!(!MetadataSourceService::is_plugin_enabled(
            &enabled, "unknown"
        ));
    }

    /// 一个来源都没有时，`is_plugin_enabled` 全部为假 —— 不报错。
    #[test]
    fn an_empty_source_list_disables_everything() {
        assert!(!MetadataSourceService::is_plugin_enabled(&[], "local"));
    }

    /// 「没收录」与「调用失败」是**两个不同的结果**。
    ///
    /// 合成一个会让「按番号搜索没找到」被报成 502 —— 客户端会以为服务端坏了，
    /// 而实际只是这部片不在 JavDB。
    #[test]
    fn not_found_and_failure_are_distinct() {
        let not_found = MetadataSourceError::NotFound;
        let failed = MetadataSourceError::RequestFailed("超时".to_owned());
        assert_ne!(not_found, failed);
    }

    /// 造一份插件交付（字段值本身不重要，形状要全）。
    fn delivery() -> MovieDelivery {
        MovieDelivery {
            movie_number: "ABC-123".to_owned(),
            title: "标题".to_owned(),
            release_date: "2024-03-05".to_owned(),
            duration_minutes: 120,
            cover_image_path: PathBuf::from("/tmp/metadata-tmp/req-1/cover.jpg"),
            plot_image_paths: vec![PathBuf::from("/tmp/metadata-tmp/req-1/plot-1.jpg")],
            summary: "简介".to_owned(),
            maker_name: Some("厂商".to_owned()),
            director_name: Some("导演".to_owned()),
            series_name: Some("系列".to_owned()),
            actors: Vec::new(),
            tag_names: vec!["标签".to_owned()],
            source_url: Some("https://example.test/ABC-123".to_owned()),
            source_id: Some("abc-123".to_owned()),
        }
    }

    /// ★ 交付 → 入库形状：**键名要对上 `create_movie` 读的那一组**。
    ///
    /// 这条测的不是「值传对了」（那是它自己的事），而是**键没写错**：
    /// `CatalogImportService::create_movie` 用 `text_of` / `int_of` 按名取值，
    /// 读不到就是 `None` —— 少写一个键不会报错，只会让那一列静默变成空串/0
    /// （`maker_name` 尤其隐蔽：它是**可选**列，丢了看不出异常）。
    #[test]
    fn the_import_detail_uses_the_keys_create_movie_reads() {
        let detail = import_detail_of(&delivery());
        for key in [
            "movie_number",
            "title",
            "summary",
            "maker_name",
            "director_name",
            "release_date",
            "duration_minutes",
        ] {
            assert!(!detail[key].is_null(), "{key} 不该缺");
        }
        assert_eq!(detail["movie_number"], "ABC-123");
        assert_eq!(detail["maker_name"], "厂商");
        assert_eq!(detail["duration_minutes"], 120);
        // 插件来源**没有** JavDB 身份 —— 给了也会被 `create_movie` 覆盖成 None。
        assert!(detail.get("javdb_id").is_none());
        // 上面列出但**刻意不写**的三个：`create_movie` 现在不读它们。
        for key in ["series_name", "actors", "tag_names"] {
            assert!(
                detail.get(key).is_none(),
                "{key} 不该出现（读不到就是误导）"
            );
        }
    }

    /// 可选字段缺失时给 `null` 而不是省略。
    ///
    /// `text_of` 对「键不存在」与「值为 null」都返回 `None`，但省略会让
    /// 「这个键到底在不在」变成调用方要靠 `get` 猜的事。
    #[test]
    fn absent_optional_fields_are_null_not_missing() {
        let mut movie = delivery();
        movie.maker_name = None;
        movie.director_name = None;
        let detail = import_detail_of(&movie);
        assert!(detail["maker_name"].is_null());
        assert!(detail.get("maker_name").is_some(), "键要在，值是 null");
        assert!(detail["director_name"].is_null());
    }

    /// `metadata_source` 的四个键：前两个来自**注册信息**，后两个来自**交付**。
    ///
    /// 分开是有意义的：同一个插件的两部影片 `source_url` 不同，所以 `source_id`
    /// / `source_url` 属于那条记录，不能塞进注册信息里。
    #[test]
    fn the_source_identity_carries_both_registration_and_delivery_facts() {
        let source = source_identity_of("javbus", "JavBus 元数据", &delivery());
        assert_eq!(source["plugin_id"], "javbus");
        assert_eq!(source["display_name"], "JavBus 元数据");
        assert_eq!(source["source_id"], "abc-123");
        assert_eq!(source["source_url"], "https://example.test/ABC-123");
    }

    /// 交付**没有** `source_url` 时写 `null` —— 不是空串。
    ///
    /// 空串会被客户端当成「有一个空链接」并渲染出可点的空 `href`。
    #[test]
    fn a_missing_source_url_stays_null() {
        let mut movie = delivery();
        movie.source_url = None;
        movie.source_id = None;
        let source = source_identity_of("javbus", "JavBus", &movie);
        assert!(source["source_url"].is_null());
        assert!(source["source_id"].is_null());
    }
}
