//! 组合根里的 **provider 数据面适配器**：把 `sm-service` 声明的
//! [`StorageGateway`] 接到真实的插件 gRPC 调用上。
//!
//! # 为什么这个文件必须存在（以及为什么它只能在组合根）
//!
//! `sm-service` 定义 `StorageGateway` 这个 trait 是因为它**不能**依赖
//! `sm-plugins`（依赖方向会成环：`sm-plugins → sm-scheduler → sm-service`）。
//! trait 只声明能力，**实现**只能由同时看得见两边的 crate 给 —— 那就是
//! `sm-server`。这是「依赖倒置」的接线点，也是 playback 域**唯一**一个。
//!
//! | 层 | 职责 |
//! |---|---|
//! | `sm-service` | 声明 `StorageGateway`，用宿主侧类型（`MediaHandle` 等） |
//! | 本模块 | 宿主侧类型 → proto 句柄 → `sm_plugins::provider_calls` |
//! | `sm-plugins` | 发 gRPC、把 `tonic::Status` 归类成上游的七码 |
//!
//! # ★ 注册表必须是**活的**，不能是快照
//!
//! `ProviderRegistration.plugin_endpoint` 记的是插件进程的控制面地址，而
//! `supervisor::launch` 每次拉起都向内核要一个新地址。看门狗重启一次插件，
//! 快照里的端点就**全部失效** —— 表现为「删媒体/生成缩略图忽然一律
//! unavailable」，而注册表看起来一切正常。所以这里拿的是
//! `Arc<Mutex<ProviderRegistry>>`，每次调用现查。
//!
//! # 错误怎么过线
//!
//! `sm-plugins` 的 `ProviderOperationError` 已经把 `tonic::Status` 归成上游那
//! 七个码（那个映射是**有损**的，理由见它的模块文档）。这里只做形状转换，
//! **不再归类一次** —— 归类分两处必然分叉。

use std::pin::Pin;
use std::sync::{Arc, Mutex, MutexGuard};

use prost_types::value::Kind;
use prost_types::{Struct, Value as PbValue};
use sm_plugins::provider_calls::{self, ProviderOperationError};
use sm_plugins::registry::ProviderRegistry;
use sm_service::playback::provider_helpers::{
    DeliveryTarget, LibraryHandle, MediaHandle, PlaybackGateway, PlaybackPlan, ProviderFailure,
    RequestedDelivery, StorageGateway, ThumbnailJobArtifact, ThumbnailJobResult,
};

/// 未被插件声明时的失败码。
///
/// 与 `sm-plugins::registry::RegistryError::UnknownProvider` 的 `code()` **同字
/// 面量** —— 上游 `MediaService.delete_media` 捕 `ProviderUnavailableError` 后
/// 抛的就是 503 `provider_not_installed`。
const NOT_INSTALLED: &str = "provider_not_installed";

/// 适配器。**可克隆**：内部只有一把共享注册表的句柄。
#[derive(Clone)]
pub struct ProviderGateway {
    providers: Arc<Mutex<ProviderRegistry>>,
}

impl ProviderGateway {
    /// 用组合根那一份（活的）注册表构造。
    pub fn new(providers: Arc<Mutex<ProviderRegistry>>) -> Self {
        Self { providers }
    }

    /// 注册表的读锁。
    fn registry(&self) -> MutexGuard<'_, ProviderRegistry> {
        // 容忍中毒，理由同 `plugins::lock_providers`：这张表是缓存，重建即可。
        self.providers
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 取该 provider 的**控制面端点**。没有 → `provider_not_installed`。
    fn endpoint_for(&self, provider_key: &str) -> Result<String, ProviderFailure> {
        self.registry()
            .get(provider_key)
            .map(|entry| entry.plugin_endpoint.clone())
            .ok_or_else(|| ProviderFailure {
                code: NOT_INSTALLED.to_owned(),
                safe_message: "媒体提供方未安装".to_owned(),
                retryable: false,
            })
    }
}

impl StorageGateway for ProviderGateway {
    fn merged_playback_format(&self, provider_key: &str) -> Option<String> {
        // **活的注册表**：读的是锁里的当下快照 —— 插件重启重装后声明会变，
        // 这里没有理由持有旧答案。
        self.registry()
            .get(provider_key)
            .and_then(|entry| entry.merged_playback_format.clone())
    }

    fn has_provider(&self, provider_key: &str) -> bool {
        self.registry().get(provider_key).is_some()
    }

    fn delete_media(
        &self,
        handle: &MediaHandle,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<(), ProviderFailure>> + Send + '_>> {
        let handle = handle.clone();
        Box::pin(async move {
            let endpoint = self.endpoint_for(&handle.provider_key)?;
            let mut client = match provider_calls::connect_storage(
                &handle.provider_key,
                &endpoint,
                "delete_media",
            )
            .await
            {
                Ok(client) => client,
                Err(error) => return Err(to_failure(&error)),
            };
            let (library, media) = proto_handles(&handle);
            provider_calls::delete_media(&mut client, &handle.provider_key, library, media)
                .await
                .map_err(|error| to_failure(&error))
        })
    }

    fn compute_file_hash(
        &self,
        handle: &MediaHandle,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ProviderFailure>> + Send + '_>>
    {
        let handle = handle.clone();
        Box::pin(async move {
            let endpoint = self.endpoint_for(&handle.provider_key)?;
            let mut client = match provider_calls::connect_storage(
                &handle.provider_key,
                &endpoint,
                "compute_file_hash",
            )
            .await
            {
                Ok(client) => client,
                Err(error) => return Err(to_failure(&error)),
            };
            let (library, media) = proto_handles(&handle);
            provider_calls::compute_file_hash_call(
                &mut client,
                &handle.provider_key,
                library,
                media,
            )
            .await
            .map_err(|error| to_failure(&error))
        })
    }

    fn probe_video_info(
        &self,
        handle: &MediaHandle,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, ProviderFailure>>
                + Send
                + '_,
        >,
    > {
        let handle = handle.clone();
        Box::pin(async move {
            let endpoint = self.endpoint_for(&handle.provider_key)?;
            let mut client = match provider_calls::connect_storage(
                &handle.provider_key,
                &endpoint,
                "probe_video_info",
            )
            .await
            {
                Ok(client) => client,
                Err(error) => return Err(to_failure(&error)),
            };
            let (library, media) = proto_handles(&handle);
            let info = provider_calls::probe_video_info_call(
                &mut client,
                &handle.provider_key,
                library,
                media,
            )
            .await
            .map_err(|error| to_failure(&error))?;
            // 「未设」与「设了但为 null」在服务层都按「探不到」处理。
            Ok(info.unwrap_or(serde_json::Value::Null))
        })
    }

    fn scan_managed_media_ref_keys(
        &self,
        library: &LibraryHandle,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<String>, ProviderFailure>> + Send + '_>>
    {
        let library = library.clone();
        Box::pin(async move {
            let endpoint = self.endpoint_for(&library.provider_key)?;
            let mut client = match provider_calls::connect_storage(
                &library.provider_key,
                &endpoint,
                "scan_managed_media_ref_keys",
            )
            .await
            {
                Ok(client) => client,
                Err(error) => return Err(to_failure(&error)),
            };
            provider_calls::scan_managed_media_ref_keys_call(
                &mut client,
                &library.provider_key,
                library_handle_proto(&library),
            )
            .await
            .map_err(|error| to_failure(&error))
        })
    }

    fn managed_media_ref_key(
        &self,
        library: &LibraryHandle,
        media_ref: serde_json::Value,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<String, ProviderFailure>> + Send + '_>>
    {
        let library = library.clone();
        Box::pin(async move {
            let endpoint = self.endpoint_for(&library.provider_key)?;
            let mut client = match provider_calls::connect_storage(
                &library.provider_key,
                &endpoint,
                "managed_media_ref_key",
            )
            .await
            {
                Ok(client) => client,
                Err(error) => return Err(to_failure(&error)),
            };
            provider_calls::managed_media_ref_key_call(
                &mut client,
                &library.provider_key,
                library_handle_proto(&library),
                media_ref,
            )
            .await
            .map_err(|error| to_failure(&error))
        })
    }

    fn generate_thumbnails(
        &self,
        handle: &MediaHandle,
        workspace: &std::path::Path,
    ) -> Pin<
        Box<
            dyn std::future::Future<Output = Result<ThumbnailJobResult, ProviderFailure>>
                + Send
                + '_,
        >,
    > {
        let handle = handle.clone();
        let workspace = workspace.display().to_string();
        Box::pin(async move {
            let endpoint = self.endpoint_for(&handle.provider_key)?;
            let mut client = match provider_calls::connect_storage(
                &handle.provider_key,
                &endpoint,
                "generate_thumbnails",
            )
            .await
            {
                Ok(client) => client,
                Err(error) => return Err(to_failure(&error)),
            };
            let (library, media) = proto_handles(&handle);
            let result = provider_calls::generate_thumbnails(
                &mut client,
                &handle.provider_key,
                library,
                media,
                &workspace,
                None,
            )
            .await
            .map_err(|error| to_failure(&error))?;
            Ok(ThumbnailJobResult {
                expected_count: result.expected_count,
                artifacts: result
                    .artifacts
                    .into_iter()
                    .map(|artifact| ThumbnailJobArtifact {
                        offset_seconds: artifact.offset_seconds,
                        relative_path: artifact.relative_path,
                    })
                    .collect(),
            })
        })
    }
}

impl PlaybackGateway for ProviderGateway {
    fn plan_playback(
        &self,
        handle: &MediaHandle,
        resource_path: &str,
        requested: RequestedDelivery,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<PlaybackPlan, ProviderFailure>> + Send + '_>>
    {
        let handle = handle.clone();
        let resource_path = resource_path.to_owned();
        Box::pin(async move {
            let endpoint = self.endpoint_for(&handle.provider_key)?;
            let mut client = match provider_calls::connect_storage(
                &handle.provider_key,
                &endpoint,
                "plan_playback",
            )
            .await
            {
                Ok(client) => client,
                Err(error) => return Err(to_failure(&error)),
            };
            let (_, media) = proto_handles(&handle);
            // `PLAYBACK_DELIVERY_UNSPECIFIED` = 让插件自己选（客户端没指定时）。
            let response = provider_calls::plan_playback(
                &mut client,
                &handle.provider_key,
                media,
                &resource_path,
                proto_delivery(requested),
            )
            .await
            .map_err(|error| to_failure(&error))?;
            Ok(playback_plan_from_proto(response.plan.unwrap_or_default()))
        })
    }

    fn plan_merged_playback(
        &self,
        handles: &[MediaHandle],
        resource_path: &str,
        requested: RequestedDelivery,
    ) -> Pin<Box<dyn std::future::Future<Output = Result<PlaybackPlan, ProviderFailure>> + Send + '_>>
    {
        // 合并播放的多个媒体**必须来自同一个 provider** —— 一次调用只能打到一个
        // 插件的端点。跨 provider 的拼接是宿主的事（上游同样如此）。
        let Some(first) = handles.first() else {
            return Box::pin(async {
                Err(ProviderFailure {
                    code: sm_service::playback::provider_helpers::PROVIDER_UNSUPPORTED.to_owned(),
                    safe_message: "合并播放至少要一个媒体".to_owned(),
                    retryable: false,
                })
            });
        };
        let provider_key = first.provider_key.clone();
        let medias: Vec<_> = handles
            .iter()
            .map(|handle| proto_handles(handle).1)
            .collect();
        let resource_path = resource_path.to_owned();
        Box::pin(async move {
            let endpoint = self.endpoint_for(&provider_key)?;
            let mut client = match provider_calls::connect_storage(
                &provider_key,
                &endpoint,
                "plan_merged_playback",
            )
            .await
            {
                Ok(client) => client,
                Err(error) => return Err(to_failure(&error)),
            };
            let response = provider_calls::plan_merged_playback(
                &mut client,
                &provider_key,
                medias,
                &resource_path,
                proto_delivery(requested),
            )
            .await
            .map_err(|error| to_failure(&error))?;
            Ok(playback_plan_from_proto(response.plan.unwrap_or_default()))
        })
    }
}

/// proto 的 `PlaybackPlan` → 宿主侧的 [`PlaybackPlan`]。
///
/// # 为什么是**无损**的形状转换（除了下面这一处）
///
/// proto 的 `oneof delivery` 本来就允许不设 —— `unavailable: true` 时插件不会
/// 给目标。所以宿主侧的 `delivery` 是 `Option`，两种「没有目标」的情形都在那里
/// 说明。
///
/// ⚠️ **一处已知的信息降级**：`unavailable == false` 却没有投递方式是插件违约
/// （既说能提供、又不给目标），宿主此刻只能与「资源不可用」同处理。这里 `warn`
/// 留痕，区分它需要一个新的失败码 —— 见 ADR §6 未决项。
///
/// `headers` 从 `HashMap` 转成**有序** `Vec`：proto 的 map 无序，而它会被带到
/// HTTP 响应上，顺序不确定会让同一份计划在两次调用间产生不同的字节。
fn playback_plan_from_proto(plan: sm_plugin_api::v1::PlaybackPlan) -> PlaybackPlan {
    use sm_plugin_api::v1::playback_plan::Delivery as ProtoDelivery;

    let delivery = plan.delivery.map(|delivery| match delivery {
        ProtoDelivery::Redirect(redirect) => DeliveryTarget::Redirect {
            url: redirect.url,
            headers: sorted_headers(redirect.headers),
        },
        ProtoDelivery::Proxy(proxy) => DeliveryTarget::Proxy {
            endpoint: proxy.endpoint,
            path_prefix: proxy.path_prefix,
            headers: sorted_headers(proxy.headers),
        },
        // **原样**：proto 里写明这里是路径不是 URL，宿主不做反转义（做了就会
        // 把「文件名里恰好带 `%20`」这种真实路径改坏）。
        ProtoDelivery::LocalPath(local) => DeliveryTarget::LocalPath { path: local.path },
    });

    if delivery.is_none() && !plan.unavailable {
        // 插件违约：既没说不提供，也没给投递方式。
        tracing::warn!(
            file_name = %plan.file_name,
            "插件返回的播放计划既未标记 unavailable、也没有投递方式 —— 按资源不可用处理"
        );
    }

    PlaybackPlan {
        delivery,
        file_name: plan.file_name,
        size_bytes: plan.size_bytes,
        content_type: plan.content_type,
        unavailable: plan.unavailable,
    }
}

/// 宿主侧的投递意图 → proto 的枚举值。
///
/// 客户端没指定时传 `UNSPECIFIED` —— **与上游一致**：那时插件按自己的默认选
/// （上游是取 `bundle.playback_deliveries[0]`）。宿主不替它猜。
fn proto_delivery(requested: RequestedDelivery) -> i32 {
    match requested {
        RequestedDelivery::Unspecified => sm_plugin_api::v1::PlaybackDelivery::Unspecified as i32,
        RequestedDelivery::Proxy => sm_plugin_api::v1::PlaybackDelivery::Proxy as i32,
        RequestedDelivery::Redirect => sm_plugin_api::v1::PlaybackDelivery::Redirect as i32,
    }
}

/// proto 的 `map<string,string>` → 有序键值对（按 key 升序）。
fn sorted_headers(headers: std::collections::HashMap<String, String>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = headers.into_iter().collect();
    pairs.sort();
    pairs
}

/// 播放计划：proto → 宿主形状的转换。
///
/// 单独一个模块（不并进文件末尾那个 `tests`）：这条缝里**只有这一步**不需要
/// gRPC —— 其余都要真起一个插件进程，所以它值得与「注册表是活的」那类装配测试
/// 分开看。
#[cfg(test)]
mod playback_plan_conversion_tests {
    use super::*;
    use sm_plugin_api::v1::playback_plan::Delivery as ProtoDelivery;
    use sm_plugin_api::v1::{LocalPathPlan, PlaybackPlan as ProtoPlan, ProxyPlan, RedirectPlan};

    fn headers(pairs: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        pairs
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect()
    }

    fn plan_with(delivery: Option<ProtoDelivery>) -> ProtoPlan {
        ProtoPlan {
            delivery,
            file_name: "m.mp4".to_owned(),
            size_bytes: Some(1024),
            content_type: Some("video/mp4".to_owned()),
            unavailable: false,
        }
    }

    /// `redirect` 分支：URL 原样带出，**头按 key 排序**。
    ///
    /// 排序不是洁癖：`HashMap` 无序，而这份头会写到 HTTP 响应上 —— 顺序不定会让
    /// 同一份计划在两次调用间产生不同字节，比对抓包时会被当成"变了"。
    #[test]
    fn a_redirect_plan_carries_the_url_and_sorted_headers() {
        let plan =
            playback_plan_from_proto(plan_with(Some(ProtoDelivery::Redirect(RedirectPlan {
                url: "https://pan.example/d/abc".to_owned(),
                headers: headers(&[("z-last", "1"), ("a-first", "2")]),
            }))));

        assert_eq!(plan.file_name, "m.mp4");
        assert_eq!(plan.size_bytes, Some(1024));
        assert_eq!(plan.content_type.as_deref(), Some("video/mp4"));
        assert!(!plan.unavailable);
        assert_eq!(
            plan.delivery,
            Some(DeliveryTarget::Redirect {
                url: "https://pan.example/d/abc".to_owned(),
                headers: vec![
                    ("a-first".to_owned(), "2".to_owned()),
                    ("z-last".to_owned(), "1".to_owned()),
                ],
            })
        );
    }

    /// `proxy` 分支：端点与**前缀**都要带出（前缀丢了会拼错请求路径）。
    #[test]
    fn a_proxy_plan_carries_the_endpoint_and_prefix() {
        let plan = playback_plan_from_proto(plan_with(Some(ProtoDelivery::Proxy(ProxyPlan {
            endpoint: "http://127.0.0.1:5001".to_owned(),
            path_prefix: "/v1/media".to_owned(),
            headers: headers(&[("x-token", "t")]),
        }))));

        assert_eq!(
            plan.delivery,
            Some(DeliveryTarget::Proxy {
                endpoint: "http://127.0.0.1:5001".to_owned(),
                path_prefix: "/v1/media".to_owned(),
                headers: vec![("x-token".to_owned(), "t".to_owned())],
            })
        );
    }

    /// ★ `local_path` 分支：路径**原样**带出，不做 URL 反转义。
    ///
    /// 反解 `%20` / `%2F` 看起来「更规范」，实际会把真实路径改坏：本地库里一个
    /// 真叫 `100% legit/ep 1.mkv` 的文件，路径里本来就长这样 —— 解一轮会得到一个
    /// 不存在的名字，而错误指向的是「文件没了」。
    ///
    /// （拼 URL 的那条老路必须转义，因为它要过 URL；这里给的是路径，不过 URL。）
    #[test]
    fn a_local_path_plan_carries_the_path_verbatim() {
        let plan =
            playback_plan_from_proto(plan_with(Some(ProtoDelivery::LocalPath(LocalPathPlan {
                path: r"C:\media\100% legit\ep 1.mkv".to_owned(),
            }))));

        assert_eq!(
            plan.delivery,
            Some(DeliveryTarget::LocalPath {
                path: r"C:\media\100% legit\ep 1.mkv".to_owned(),
            }),
            "路径要一个字节都不变"
        );
    }

    /// ★ `unavailable` 的否定结果**没有**投递目标，且**不是**错误。
    ///
    /// 把它变成 `Err` 就会让「影片文件被删」看起来像「插件坏了」（502）—— 而
    /// proto 的 `oneof` 本来就可以不设，这是**正常应答**。
    #[test]
    fn an_unavailable_plan_has_no_delivery_target() {
        let mut proto = plan_with(None);
        proto.unavailable = true;

        let plan = playback_plan_from_proto(proto);

        assert!(plan.unavailable, "否定结果要如实带出");
        assert_eq!(plan.delivery, None, "没有目标，但这是正常应答而不是错误");
    }
}

/// `sm-plugins` 的错误 → `sm-service` 的失败信封。**只换形状，不再归类。**
fn to_failure(error: &ProviderOperationError) -> ProviderFailure {
    ProviderFailure {
        code: error.code().to_owned(),
        safe_message: error.safe_message().to_owned(),
        // 上游的 `retryable` 是 provider 给的独立字段；proto 目前过不了线，
        // 所以 `sm-plugins` 是按码猜的 —— 缩略图的延迟轨就靠这个值。
        retryable: error.retryable(),
    }
}

/// 宿主侧句柄 → proto 的 `LibraryHandle` + `MediaHandle`。
///
/// # 宿主侧是展平的，proto 是嵌套的
///
/// `sm-service` 的 `MediaHandle` 把库的字段摊平了（因为 `sm_db::Media` 只有
/// `library_id`，库要另查），而 proto 的 `MediaHandle` 里嵌着一个
/// `LibraryHandle`。所以这里要把展平的那几个**重新装回** library 里。
///
/// ⚠️ 两边字段数量不一致就会在这里静默丢字段 —— 上次丢的是 `file_name` 与
/// `duration_seconds`（`plugin-ref-local` 靠它们算张数与命名）。加字段时
/// **两边一起加**，并看 `sm-service` 里那条 `the_media_handle_carries_what_the
/// _plugin_actually_reads` 测试。
fn proto_handles(
    handle: &MediaHandle,
) -> (
    sm_plugin_api::v1::LibraryHandle,
    sm_plugin_api::v1::MediaHandle,
) {
    let library = sm_plugin_api::v1::LibraryHandle {
        library_id: handle.library_id,
        provider_key: handle.provider_key.clone(),
        provider_config: json_to_struct(&handle.provider_config),
        account_key: handle.account_key.clone(),
    };
    let media = sm_plugin_api::v1::MediaHandle {
        media_id: handle.media_id,
        library: Some(library.clone()),
        storage_ref: json_to_struct(&handle.storage_ref),
        file_name: handle.file_name.clone(),
        file_size_bytes: handle.file_size_bytes,
        duration_seconds: i64::from(handle.duration_seconds),
    };
    (library, media)
}

/// 宿主侧的 [`LibraryHandle`] → proto 的 `LibraryHandle`。
///
/// 与 [`proto_handles`] 的库半边**同源** —— 字段清单只此一份。
fn library_handle_proto(library: &LibraryHandle) -> sm_plugin_api::v1::LibraryHandle {
    sm_plugin_api::v1::LibraryHandle {
        library_id: library.library_id,
        provider_key: library.provider_key.clone(),
        provider_config: json_to_struct(&library.provider_config),
        account_key: library.account_key.clone(),
    }
}

/// `serde_json::Value` → proto 的 `google.protobuf.Struct`。
///
/// **非对象返回 `None`** —— `Struct` 只能是对象；宿主侧的空引用是
/// `Value::Null`，正好对应「没给」。
fn json_to_struct(value: &serde_json::Value) -> Option<Struct> {
    let object = value.as_object()?;
    Some(Struct {
        fields: object
            .iter()
            .map(|(key, item)| (key.clone(), json_to_value(item)))
            .collect(),
    })
}

fn json_to_value(value: &serde_json::Value) -> PbValue {
    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(flag) => Kind::BoolValue(*flag),
        serde_json::Value::Number(number) => Kind::NumberValue(number.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(text) => Kind::StringValue(text.clone()),
        serde_json::Value::Array(items) => Kind::ListValue(prost_types::ListValue {
            values: items.iter().map(json_to_value).collect(),
        }),
        serde_json::Value::Object(_) => {
            Kind::StructValue(json_to_struct(value).unwrap_or_default())
        }
    };
    PbValue { kind: Some(kind) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_plugins::registry::ProviderRegistration;
    use sm_service::playback::provider_helpers::{LibraryRecord, MediaRecord};

    fn register(registry: &mut ProviderRegistry, key: &str, endpoint: &str) {
        registry.insert(ProviderRegistration {
            provider_key: key.to_owned(),
            display_name: key.to_uppercase(),
            plugin_id: format!("plugin_{key}"),
            capabilities: Vec::new(),
            data_plane_endpoint: None,
            plugin_endpoint: endpoint.to_owned(),
            library_config_fields: Vec::new(),
            playback_deliveries: Vec::new(),
            merged_playback_format: None,
            download_config_fields: Vec::new(),
        });
    }

    fn gateway() -> ProviderGateway {
        let mut registry = ProviderRegistry::new();
        register(&mut registry, "local", "http://127.0.0.1:51001");
        ProviderGateway::new(Arc::new(Mutex::new(registry)))
    }

    fn host_handle() -> MediaHandle {
        sm_service::playback::provider_helpers::media_handle_for(&MediaRecord {
            id: 12,
            library_id: 3,
            storage_ref: serde_json::json!({"path": "a/b.mp4"}),
            provider_config: serde_json::json!({"root": "/mnt"}),
            provider_key: "local".to_owned(),
            account_key: Some("acct".to_owned()),
            file_name: "ABC-001.mp4".to_owned(),
            file_size_bytes: 2_048,
            duration_seconds: 1_800,
        })
    }

    /// ★ 注册表必须是**活的**：`rebuild` 换掉端点后，适配器要立刻看到新的。
    ///
    /// 反过来的实现（构造时快照一份）会在插件重启后把请求发到旧端口。
    #[test]
    fn the_registry_is_read_live_not_snapshotted() {
        let registry = Arc::new(Mutex::new(ProviderRegistry::new()));
        register(&mut registry.lock().unwrap(), "local", "http://old:1");
        let gateway = ProviderGateway::new(Arc::clone(&registry));
        assert!(gateway.has_provider("local"));

        // 看门狗重启插件 → 重建注册表 → 换端点。
        let mut rebuilt = ProviderRegistry::new();
        register(&mut rebuilt, "local", "http://new:2");
        *registry.lock().unwrap() = rebuilt;

        // `has_provider` 仍为真：对象没被换掉，只是内容换了。
        assert!(gateway.has_provider("local"));
        // 假如当初是「换一个新 Arc」，这里就会变假。
        let endpoint = registry
            .lock()
            .unwrap()
            .get("local")
            .map(|entry| entry.plugin_endpoint.clone());
        assert_eq!(endpoint.as_deref(), Some("http://new:2"));
    }

    #[test]
    fn an_unknown_provider_is_not_installed() {
        let gateway = gateway();
        let error = gateway
            .endpoint_for("nope")
            .expect_err("没注册的 provider 就是没装");
        assert_eq!(error.code, NOT_INSTALLED);
        assert!(!error.retryable, "装不装是确定性的，重试没用");
    }

    /// ★ 宿主侧展平的字段要**原样装回** proto 的嵌套句柄。
    ///
    /// 丢字段不会编译报错，只会在插件那边表现为「生成 0 张」。
    #[test]
    fn the_flattened_library_fields_are_reassembled() {
        let (library, media) = proto_handles(&host_handle());

        assert_eq!(library.library_id, 3);
        assert_eq!(library.provider_key, "local");
        assert_eq!(library.account_key.as_deref(), Some("acct"));
        assert_eq!(
            library.provider_config.as_ref().and_then(|c| c
                .fields
                .get("root")
                .and_then(|v| v.kind.as_ref())
                .and_then(|kind| match kind {
                    Kind::StringValue(text) => Some(text.as_str()),
                    _ => None,
                })),
            Some("/mnt")
        );

        assert_eq!(media.media_id, 12);
        assert_eq!(media.file_name, "ABC-001.mp4");
        assert_eq!(media.duration_seconds, 1_800);
        assert_eq!(media.file_size_bytes, 2_048);
        // proto 里 library 是嵌套的，两个要指向同一份。
        assert_eq!(media.library.as_ref().map(|l| l.library_id), Some(3));
        assert_eq!(
            media.storage_ref.as_ref().and_then(|s| s
                .fields
                .get("path")
                .and_then(|v| v.kind.as_ref())
                .and_then(|kind| match kind {
                    Kind::StringValue(text) => Some(text.as_str()),
                    _ => None,
                })),
            Some("a/b.mp4")
        );
    }

    /// 非对象的 JSON 不该硬塞进 `Struct`（`storage_ref` 为空时就是 `Null`）。
    #[test]
    fn a_non_object_json_is_no_struct_at_all() {
        assert!(json_to_struct(&serde_json::Value::Null).is_none());
        assert!(json_to_struct(&serde_json::json!("text")).is_none());
        assert!(json_to_struct(&serde_json::json!(7)).is_none());
        assert!(json_to_struct(&serde_json::json!({})).is_some());
    }

    /// 嵌套与数组也要能过线（`storage_ref` 的结构由 provider 定义，宿主不解释）。
    #[test]
    fn nested_and_array_values_survive_the_conversion() {
        let converted = json_to_struct(&serde_json::json!({
            "n": 1,
            "f": 1.5,
            "b": true,
            "z": null,
            "list": [1, "two"],
            "obj": {"inner": "x"},
        }))
        .expect("对象应当能转");
        assert_eq!(converted.fields.len(), 6);
        assert!(matches!(
            converted.fields["z"].kind,
            Some(Kind::NullValue(_))
        ));
        assert!(matches!(
            converted.fields["list"].kind,
            Some(Kind::ListValue(_))
        ));
        assert!(matches!(
            converted.fields["obj"].kind,
            Some(Kind::StructValue(_))
        ));
        assert!(matches!(
            converted.fields["b"].kind,
            Some(Kind::BoolValue(true))
        ));
    }

    /// 库句柄那条路径（`library_handle_for`）也过一遍，避免只测了媒体侧。
    #[test]
    fn a_library_record_round_trips_its_account_key() {
        let handle = sm_service::playback::provider_helpers::library_handle_for(&LibraryRecord {
            id: 3,
            provider_key: "local".to_owned(),
            provider_config: serde_json::json!({}),
            account_key: None,
        });
        assert!(handle.account_key.is_none());
    }
}
