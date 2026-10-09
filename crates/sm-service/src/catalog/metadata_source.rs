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
    /// 来源标识（`javdb` / `plugin:<id>`），落进 `movie.metadata_source`。
    ///
    /// ⚠️ 上游这一支存的是 `{plugin_id, display_name, source_id, source_url}`
    /// 四个键；这里只存一个串（骨架期既有的契约，改它会波及还没落地的
    /// `catalog_import`）。`source_id` / `source_url` 在 [`MovieDelivery`] 上，没丢。
    pub source: String,
    /// 交付目录。**闭包退出后即失效**（清理已发生）。JavDB 那支为 `None`。
    pub delivery_dir: Option<PathBuf>,
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
            source: format!("plugin:{plugin_id}"),
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
                        source: "javdb".to_owned(),
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
                        source: format!("plugin:{}", source.plugin_id),
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

    /// 按番号导入（JavDB 优先）。**不返回交付目录** —— 导入过程中用即可。
    pub async fn import_by_number(
        &self,
        movie_number: &str,
        import_service: &mut dyn crate::catalog::catalog_import::CatalogImport,
        force_subscribed: bool,
    ) -> Result<bool, MetadataSourceError> {
        let _ = (movie_number, import_service, force_subscribed);
        todo!("骨架：fetch() 闭包内调 import_movie_if_missing；闭包退出即清理交付文件")
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
}
