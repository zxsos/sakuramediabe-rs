//! `StorageProvider` 的 115 网盘实现。
//!
//! 落地的核心 rpc：
//!
//! | rpc | 说明 |
//! |---|------|
//! | `Browse` | 按 cid 列目录（一元，分页） |
//! | `ScanImportSource` | 递归枚举文件（server streaming） |
//! | `PlanPlayback` | 取直链，`redirect` 投放 |
//! | `GenerateThumbnails` | 暂不支持（115 不暴露视频解码，返回 `unimplemented`） |
//! | `GetSpaceUsage` | 115 空间用量 |
//! | `PrepareLibrary` | 校验 Cookie 有效性，解析媒体/下载目录为 cid |
//!
//! 其余 rpc 由 [`sm_plugin_api::provider::StorageProviderExt`] 的默认实现
//! 提供（返回 `Status::unimplemented`）。

use async_trait::async_trait;
use futures::stream::BoxStream;
use sm_plugin_api::provider::StorageProviderExt;
use sm_plugin_api::v1::{
    generate_thumbnails_response, playback_plan, BrowseEntry, BrowsePage, BrowseRequest, EntryType,
    GenerateThumbnailsRequest, GenerateThumbnailsResponse, GetSpaceUsageRequest,
    GetSpaceUsageResponse, ImportFile, ImportFileEntry, LibraryHandle,
    PlanPlaybackRequest, PlanPlaybackResponse, PlaybackDelivery, PlaybackPlan, PrepareLibraryRequest,
    PrepareLibraryResponse, ProgressEvent, RedirectPlan, ScanImportSourceRequest, StorageSpaceUsage,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::client::{Cloud115Client, Cloud115Entry, Cloud115Error};
use crate::config::Plugin115Config;
use crate::opaque::{dir_ref, file_ref, ref_cid, ref_pickcode, ROOT_CID};

/// `BrowseRequest.limit` 未填 / 非正数时的兜底页大小。
const DEFAULT_PAGE_SIZE: usize = 100;
/// 单页硬上限。
const MAX_PAGE_SIZE: usize = 1000;
/// 流式通道缓冲槽数。
const STREAM_BUFFER: usize = 8;

/// 115 网盘 provider。
#[derive(Debug, Clone)]
pub struct Provider115 {
    config: Plugin115Config,
    provider_key: String,
}

impl Provider115 {
    /// 用配置构造。
    pub fn new(config: Plugin115Config) -> Self {
        let provider_key = config.provider_key_or_default().to_owned();
        Self {
            config,
            provider_key,
        }
    }

    /// provider key。
    pub fn provider_key(&self) -> &str {
        &self.provider_key
    }

    fn check_library(&self, library: &LibraryHandle) -> Result<(), Status> {
        if !library.provider_key.is_empty() && library.provider_key != self.provider_key {
            return Err(Status::invalid_argument(format!(
                "provider_key 不匹配：期望 {}，收到 {}",
                self.provider_key, library.provider_key
            )));
        }
        Ok(())
    }

    /// 按当前请求的配置构造 115 客户端。
    ///
    /// 每次请求都新建 client：Cookie 可能在两次请求之间被宿主更新，
    /// 长连接复用会拿到过期的认证。
    fn client(&self, library: &LibraryHandle) -> Result<Cloud115Client, Status> {
        let config = Plugin115Config::from_provider_config(library.provider_config.as_ref());
        let merged = Plugin115Config {
            web_cookie: if config.web_cookie.is_empty() {
                self.config.web_cookie.clone()
            } else {
                config.web_cookie
            },
            device_cookie: if config.device_cookie.is_empty() {
                self.config.device_cookie.clone()
            } else {
                config.device_cookie
            },
            ..config
        };
        let cookie = merged.cookie().ok_or_else(|| {
            Status::unauthenticated("115 Cookie 未配置：请填写 web_cookie 或 device_cookie")
        })?;
        Cloud115Client::new(cookie).map_err(|e| client_error(e, "认证"))
    }
}

/// 115 错误 → gRPC Status。
fn client_error(error: Cloud115Error, operation: &str) -> Status {
    match error {
        Cloud115Error::Auth(message) => Status::unauthenticated(format!("115 {operation}认证失败: {message}")),
        Cloud115Error::NotFound(message) => Status::not_found(format!("115 {operation}: {message}")),
        Cloud115Error::Request(message) => Status::internal(format!("115 {operation}失败: {message}")),
        Cloud115Error::Transport(source) => {
            Status::unavailable(format!("115 {operation}网络错误: {source}"))
        }
    }
}

fn entry_to_browse(entry: &Cloud115Entry) -> BrowseEntry {
    let source_ref = if entry.is_dir {
        dir_ref(&entry.id)
    } else {
        file_ref(&entry.pickcode, &entry.parent_id)
    };
    BrowseEntry {
        source_ref: Some(source_ref),
        name: entry.name.clone(),
        entry_type: if entry.is_dir {
            EntryType::Directory
        } else {
            EntryType::File
        } as i32,
        size_bytes: if entry.is_dir {
            None
        } else {
            Some(entry.size_bytes as i64)
        },
        modified_at: None,
        is_video: !entry.is_dir && is_video_name(&entry.name),
    }
}

const VIDEO_EXTENSIONS: [&str; 9] = [
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "ts", "flv", "wmv",
];

fn is_video_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    VIDEO_EXTENSIONS.iter().any(|ext| lower.ends_with(&format!(".{ext}")))
}

#[async_trait]
impl StorageProviderExt for Provider115 {
    // ── ① browse ──────────────────────────────────────────────────
    async fn browse(
        &self,
        request: Request<BrowseRequest>,
    ) -> Result<Response<BrowsePage>, Status> {
        let payload = request.into_inner();
        let library = payload
            .library
            .ok_or_else(|| Status::invalid_argument("BrowseRequest.library 缺失"))?;
        self.check_library(&library)?;

        let cid = ref_cid(payload.parent_ref.as_ref()).to_owned();
        let client = self.client(&library)?;

        let limit = if payload.limit <= 0 {
            DEFAULT_PAGE_SIZE
        } else {
            (payload.limit as usize).min(MAX_PAGE_SIZE)
        };
        // 游标是 offset 的十进制字符串。
        let offset: u64 = payload
            .cursor
            .as_deref()
            .unwrap_or("0")
            .parse()
            .map_err(|_| Status::invalid_argument("游标已失效"))?;

        let (entries, _total) = client
            .list_dir(&cid, offset, limit as u64)
            .await
            .map_err(|e| client_error(e, "浏览目录"))?;

        let next_cursor = if entries.len() == limit {
            Some((offset + entries.len() as u64).to_string())
        } else {
            None
        };
        let entries = entries.iter().map(entry_to_browse).collect();

        Ok(Response::new(BrowsePage {
            entries,
            next_cursor,
        }))
    }

    // ── ② scan_import_source（server streaming）──────────────────
    async fn scan_import_source(
        &self,
        request: Request<ScanImportSourceRequest>,
    ) -> Result<Response<BoxStream<'static, Result<ImportFileEntry, Status>>>, Status> {
        let payload = request.into_inner();
        let library = payload
            .library
            .ok_or_else(|| Status::invalid_argument("ScanImportSourceRequest.library 缺失"))?;
        self.check_library(&library)?;

        let cid = ref_cid(payload.source_ref.as_ref()).to_owned();
        let client = self.client(&library).map_err(|e| {
            // 扫描开始前就把认证错误抛出去，而不是让流里第一个 item 才报错。
            e
        })?;

        // 先探活：目录不存在时直接返回 not_found，不开流。
        client
            .list_dir(&cid, 0, 1)
            .await
            .map_err(|e| client_error(e, "扫描导入来源"))?;

        let (sender, receiver) = mpsc::channel(STREAM_BUFFER);
        tokio::spawn(async move {
            if let Err(status) = walk_115(&client, &cid, &sender).await {
                let _ = sender.send(Err(status)).await;
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    // ── ⑩ plan_playback ──────────────────────────────────────────
    async fn plan_playback(
        &self,
        request: Request<PlanPlaybackRequest>,
    ) -> Result<Response<PlanPlaybackResponse>, Status> {
        let payload = request.into_inner();
        let media = payload
            .media
            .ok_or_else(|| Status::invalid_argument("PlanPlaybackRequest.media 缺失"))?;
        if let Some(library) = media.library.as_ref() {
            self.check_library(library)?;
        }

        let pickcode = ref_pickcode(media.storage_ref.as_ref()).ok_or_else(|| {
            Status::invalid_argument("115 媒体句柄缺少 pickcode")
        })?;
        let client = self.client(
            &media
                .library
                .clone()
                .unwrap_or_else(|| LibraryHandle {
                    provider_key: self.provider_key.clone(),
                    ..Default::default()
                }),
        )?;

        let url = client
            .get_download_url(pickcode, "Mozilla/5.0")
            .await
            .map_err(|e| client_error(e, "取直链"))?;

        let delivery =
            PlaybackDelivery::try_from(payload.delivery).unwrap_or(PlaybackDelivery::Unspecified);
        // 115 直链天然是 redirect；proxy 模式暂不支持。
        if matches!(delivery, PlaybackDelivery::Proxy) {
            return Err(Status::unimplemented(
                "115 provider 暂不支持 proxy 投放：请用 redirect",
            ));
        }

        let plan = PlaybackPlan {
            delivery: Some(playback_plan::Delivery::Redirect(RedirectPlan {
                url,
                headers: Default::default(),
            })),
            file_name: media.file_name.clone(),
            size_bytes: None,
            content_type: content_type_of(&media.file_name),
            unavailable: false,
        };
        Ok(Response::new(PlanPlaybackResponse { plan: Some(plan) }))
    }

    // ── ⑪ generate_thumbnails ────────────────────────────────────
    async fn generate_thumbnails(
        &self,
        request: Request<GenerateThumbnailsRequest>,
    ) -> Result<Response<BoxStream<'static, Result<GenerateThumbnailsResponse, Status>>>, Status>
    {
        let payload = request.into_inner();
        let library = payload
            .library
            .ok_or_else(|| Status::invalid_argument("GenerateThumbnailsRequest.library 缺失"))?;
        self.check_library(&library)?;

        // 115 不暴露视频解码能力，缩略图需要把 HLS 分片拉回本地解码。
        // 当前版本返回 unimplemented，宿主会回退到其他缩略图后端。
        let (sender, receiver) = mpsc::channel(1);
        tokio::spawn(async move {
            let _ = sender
                .send(Err(Status::unimplemented(
                    "115 provider 暂不支持服务端缩略图生成",
                )))
                .await;
        });
        let _ = generate_thumbnails_response::Payload::Progress(ProgressEvent::default());
        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }

    // ── GetSpaceUsage ─────────────────────────────────────────────
    async fn get_space_usage(
        &self,
        request: Request<GetSpaceUsageRequest>,
    ) -> Result<Response<GetSpaceUsageResponse>, Status> {
        let payload = request.into_inner();
        let library = payload
            .library
            .ok_or_else(|| Status::invalid_argument("GetSpaceUsageRequest.library 缺失"))?;
        self.check_library(&library)?;

        let client = self.client(&library)?;
        let usage = client
            .space_usage()
            .await
            .map_err(|e| client_error(e, "空间用量"))?;

        Ok(Response::new(GetSpaceUsageResponse {
            usage: Some(StorageSpaceUsage {
                total_bytes: Some(usage.total_bytes as i64),
                used_bytes: Some(usage.used_bytes as i64),
                free_bytes: Some(
                    usage.total_bytes.saturating_sub(usage.used_bytes) as i64
                ),
            }),
        }))
    }

    // ── PrepareLibrary ────────────────────────────────────────────
    async fn prepare_library(
        &self,
        request: Request<PrepareLibraryRequest>,
    ) -> Result<Response<PrepareLibraryResponse>, Status> {
        let payload = request.into_inner();

        let config = Plugin115Config::from_provider_config(payload.submitted_config.as_ref());
        // 没有提交配置时用启动时的配置。
        let config = if config.cookie().is_none() {
            self.config.clone()
        } else {
            config
        };
        let cookie = config.cookie().ok_or_else(|| {
            Status::invalid_argument("115 Cookie 未配置：请填写 web_cookie 或 device_cookie")
        })?;
        let client =
            Cloud115Client::new(cookie).map_err(|e| client_error(e, "校验配置"))?;
        let alive = client.check_alive().await.map_err(|e| client_error(e, "校验登录"))?;
        if !alive {
            return Err(Status::unauthenticated("115 登录已失效，请更新 Cookie"));
        }

        // 解析媒体 / 下载目录为 cid，提前暴露配置错误。
        if !config.media_root_path.is_empty() {
            client
                .resolve_path(&config.media_root_path)
                .await
                .map_err(|e| client_error(e, "解析媒体目录"))?;
        }

        // 回显 provider_config（Struct），让宿主存下来。
        let mut fields = std::collections::BTreeMap::new();
        let put = |fields: &mut std::collections::BTreeMap<String, prost_types::Value>, k: &str, v: &str| {
            fields.insert(
                k.to_owned(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(v.to_owned())),
                },
            );
        };
        put(&mut fields, "provider_key", config.provider_key_or_default());
        put(&mut fields, "web_cookie", &config.web_cookie);
        put(&mut fields, "device_cookie", &config.device_cookie);
        put(&mut fields, "media_root_path", &config.media_root_path);
        put(&mut fields, "downloads_root_path", &config.downloads_root_path);

        Ok(Response::new(PrepareLibraryResponse {
            provider_config: Some(prost_types::Struct { fields }),
            account_key: None,
        }))
    }
}

/// 递归枚举 115 目录下的所有文件，顺着通道发出。
async fn walk_115(
    client: &Cloud115Client,
    root_cid: &str,
    sender: &mpsc::Sender<Result<ImportFileEntry, Status>>,
) -> Result<(), Status> {
    let mut stack = vec![(root_cid.to_owned(), String::new())];
    while let Some((cid, prefix)) = stack.pop() {
        let entries = client
            .list_directory(&cid)
            .await
            .map_err(|e| client_error(e, "枚举文件"))?;
        let mut subdirs = Vec::new();
        for entry in &entries {
            if entry.is_dir {
                let child_prefix = if prefix.is_empty() {
                    entry.name.clone()
                } else {
                    format!("{}/{}", prefix, entry.name)
                };
                subdirs.push((entry.id.clone(), child_prefix));
                continue;
            }
            let relative_path = if prefix.is_empty() {
                entry.name.clone()
            } else {
                format!("{}/{}", prefix, entry.name)
            };
            let file = ImportFile {
                source_ref: Some(file_ref(&entry.pickcode, &cid)),
                is_video: is_video_name(&entry.name),
                name: entry.name.clone(),
                relative_path,
                size_bytes: entry.size_bytes as i64,
            };
            if sender
                .send(Ok(ImportFileEntry { file: Some(file) }))
                .await
                .is_err()
            {
                return Ok(());
            }
        }
        for subdir in subdirs.into_iter().rev() {
            stack.push(subdir);
        }
    }
    Ok(())
}

fn content_type_of(file_name: &str) -> Option<String> {
    let lower = file_name.to_ascii_lowercase();
    let mime = if lower.ends_with(".mp4") || lower.ends_with(".m4v") {
        "video/mp4"
    } else if lower.ends_with(".mkv") {
        "video/x-matroska"
    } else if lower.ends_with(".webm") {
        "video/webm"
    } else if lower.ends_with(".mov") {
        "video/quicktime"
    } else if lower.ends_with(".avi") {
        "video/x-msvideo"
    } else if lower.ends_with(".ts") {
        "video/mp2t"
    } else {
        return None;
    };
    Some(mime.to_owned())
}

#[allow(dead_code)]
fn _root_cid() -> &'static str {
    ROOT_CID
}
