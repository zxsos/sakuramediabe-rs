//! `StorageProvider` 的本地目录参考实现。
//!
//! 只落地最小集的 4 个 rpc —— 其余 **28 个不再手写 stub**，改由
//! [`sm_plugin_api::StorageProviderExt`] 的默认实现提供（返回
//! `Status::unimplemented`）。这正是 gRPC 报告 P1-4 说的那笔税：本文件因此
//! 少了约 **170 行纯签名**，而且是**编译期**少掉的 —— proto 再加 rpc 也不会
//! 让插件编译不过。
//!
//! 实现过程中遇到的 proto 缺口都就地标注了 `GAP:` 注释，并汇总到
//! `docs/parallel/grpc-plugin-report.md`。

use std::path::{Component, Path, PathBuf};
use std::time::SystemTime;

use async_trait::async_trait;
use chrono::{DateTime, SecondsFormat, Utc};
use futures::stream::BoxStream;
use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};
use prost_types::Struct;
use sm_plugin_api::provider::StorageProviderExt;
use sm_plugin_api::v1::{
    generate_thumbnails_response, playback_plan, BrowseEntry, BrowsePage, BrowseRequest, EntryType,
    GenerateThumbnailsRequest, GenerateThumbnailsResponse, ImportFile, ImportFileEntry,
    LibraryHandle, MediaHandle, PlanPlaybackRequest, PlanPlaybackResponse, PlaybackDelivery,
    PlaybackPlan, ProgressEvent, RedirectPlan, ScanImportSourceRequest, ThumbnailArtifact,
    ThumbnailGeneration,
};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};

use crate::opaque::{ref_path, string_ref};

/// `BrowseRequest.limit` 未填 / 非正数时的兜底页大小。
///
/// GAP: proto 没有规定默认值与上限，`limit` 还是 `int32`（可为负）。
/// 这些语义只能落到每个插件各自的约定里。
const DEFAULT_PAGE_SIZE: usize = 100;
/// 单页硬上限，避免一次把整个目录塞进一页超过 gRPC 默认 4MB 的消息体。
const MAX_PAGE_SIZE: usize = 1000;

/// 流式通道缓冲槽数。刻意远小于真实流的长度，让背压真的发生：
/// 消费慢时生产者会停在 `tx.send()`，而不是先把整棵树囤在内存里。
const STREAM_BUFFER: usize = 8;

/// 缩略图采样数量下限与上限（替代「按视频时长每 10 秒一张」的实作，
/// 本 crate 不真正解码视频）。
const THUMBNAIL_MIN: i64 = 3;
const THUMBNAIL_MAX: i64 = 24;
/// 采样间隔（秒）。
const THUMBNAIL_INTERVAL_SECONDS: i64 = 10;

/// 视为视频的扩展名。
const VIDEO_EXTENSIONS: [&str; 9] = [
    "mp4", "mkv", "webm", "mov", "avi", "m4v", "ts", "flv", "wmv",
];

/// file:// URL 里需要转义的字符：控制字符之外补上路径里常见的几个。
const FILE_URL_ESCAPE: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'?');

/// 本地参考插件在注册阶段会声明的 provider key。
pub const PROVIDER_KEY: &str = "local-ref";

/// 本地目录 provider。
///
/// 根目录在构造时给定。真实部署里它来自 `LibraryHandle.provider_config`
/// （同样是 `Struct`，宿主无法校验）；这里简化为构造参数，
/// 是为了让请求-响应之外不引入额外的运行时状态。
#[derive(Debug, Clone)]
pub struct LocalRefProvider {
    root: PathBuf,
    provider_key: String,
}

impl LocalRefProvider {
    /// 以 `root` 为根目录构造 provider。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: absolutize(&root.into()),
            provider_key: PROVIDER_KEY.to_owned(),
        }
    }

    /// 覆盖 provider key（默认 [`PROVIDER_KEY`]；测试夹具 `fixture` 也用同一个
    /// 常量，所以两边不会漂移）。
    pub fn with_provider_key(mut self, provider_key: impl Into<String>) -> Self {
        self.provider_key = provider_key.into();
        self
    }

    /// provider 根目录。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 校验 library 句柄是不是指向自己。
    fn check_library(&self, library: &LibraryHandle) -> Result<(), Status> {
        if !library.provider_key.is_empty() && library.provider_key != self.provider_key {
            return Err(Status::invalid_argument(format!(
                "provider_key 不匹配：期望 {}，收到 {}",
                self.provider_key, library.provider_key
            )));
        }
        Ok(())
    }

    /// 把请求里的 `path` 解析成 root 之内的绝对路径。
    ///
    /// 路径穿透（`../..`）在此被拦下，用的是词法归一化而非 `canonicalize`：
    /// 后者要求路径已存在，而 Browse 允许列举尚不存在的子路径。
    /// 代价是符号链接跳出 root 的情况拦不住 —— 真实 provider 需要自己在
    /// 打开文件前做一次 `O_NOFOLLOW` 级别的校验。
    fn confined_path(&self, source: Option<&Struct>) -> Result<PathBuf, Status> {
        let relative = ref_path(source).unwrap_or("");
        let candidate = if relative.is_empty() {
            self.root.clone()
        } else {
            absolutize(&self.root.join(relative))
        };
        if candidate != self.root && !candidate.starts_with(&self.root) {
            return Err(Status::invalid_argument(format!(
                "路径 {} 逃出了 provider 根目录 {}",
                candidate.display(),
                self.root.display()
            )));
        }
        Ok(candidate)
    }

    /// 媒体句柄指向的绝对路径。
    fn media_path(&self, media: &MediaHandle) -> Result<PathBuf, Status> {
        if let Some(path) = ref_path(media.storage_ref.as_ref()) {
            return Ok(absolutize(&self.root.join(path)));
        }
        // GAP: `MediaHandle` 没有 `relative_path`，也没有地方声明
        // storage_ref 的 schema。缺少 path 时只能猜「同名文件在根目录」。
        Ok(absolutize(&self.root.join(&media.file_name)))
    }
}

#[async_trait]
impl StorageProviderExt for LocalRefProvider {
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

        let directory = self.confined_path(payload.parent_ref.as_ref())?;
        let all = list_dir(&directory).await?;

        let limit = page_size(payload.limit);
        let start = cursor_offset(&all, payload.cursor.as_deref())?;
        let end = all.len().min(start + limit);

        let parent_rel = ref_path(payload.parent_ref.as_ref()).unwrap_or("");
        let entries = all[start..end]
            .iter()
            .map(|item| browse_entry(parent_rel, item))
            .collect();

        // 游标 = 本页最后一个条目的名字，下一页从它之后继续。
        let next_cursor = all
            .get(end.saturating_sub(1))
            .filter(|_| end < all.len())
            .map(|item| item.name.clone());

        // GAP: BrowsePage 只有 next_cursor，没有总数，宿主无法渲染「共 N 项」。
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

        let start = self.confined_path(payload.source_ref.as_ref())?;
        match tokio::fs::metadata(&start).await {
            Ok(_) => {}
            Err(source) => return Err(Status::not_found(format!("导入来源不存在：{source}"))),
        }

        let (sender, receiver) = mpsc::channel(STREAM_BUFFER);
        let base = self.root.clone();
        tokio::spawn(async move {
            walk_and_emit(base, start, sender).await;
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

        // GAP: 本地 provider 最自然的答案是「给你一个本地路径，你自己读」，
        // 但 PlaybackPlan 的 delivery 只有 redirect(url) / proxy(endpoint) 两种，
        // 没有 local_path —— 而 OpenCoverSourceResponse 恰恰有 local_path。
        // 这里只能退化成 file:// 的伪 redirect，宿主必须为此开一个特殊分支。
        let delivery =
            PlaybackDelivery::try_from(payload.delivery).unwrap_or(PlaybackDelivery::Unspecified);
        if matches!(delivery, PlaybackDelivery::Proxy) {
            return Err(Status::unimplemented(
                "本地 provider 不支持 proxy 投放：PlaybackPlan 缺少『宿主直接读本地文件』这一 delivery",
            ));
        }
        if !payload.resource_path.is_empty() {
            return Err(Status::invalid_argument("本地 provider 不支持子资源播放"));
        }

        let path = self.media_path(&media)?;
        let metadata = tokio::fs::metadata(&path).await;

        let plan = match metadata {
            Ok(info) if info.is_file() => PlaybackPlan {
                delivery: Some(playback_plan::Delivery::Redirect(RedirectPlan {
                    url: file_url(&path),
                    headers: Default::default(),
                })),
                file_name: media.file_name.clone(),
                size_bytes: Some(clamp_i64(info.len())),
                content_type: content_type_of(&media.file_name),
                unavailable: false,
            },
            // GAP: 失败只有一个布尔位 `unavailable`，区分不了
            // 「文件不存在」「无权限」「已被黑名单」，也带不了
            // ProviderErrorCode / retryable —— common.proto 里定义了
            // ProviderError，但没有任何 rpc 用它来做错误通道。
            _ => PlaybackPlan {
                delivery: None,
                file_name: media.file_name.clone(),
                size_bytes: None,
                content_type: None,
                unavailable: true,
            },
        };

        Ok(Response::new(PlanPlaybackResponse { plan: Some(plan) }))
    }

    // ── ⑪ generate_thumbnails（server streaming）─────────────────
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
        let media = payload
            .media
            .ok_or_else(|| Status::invalid_argument("GenerateThumbnailsRequest.media 缺失"))?;

        // 按 proto 注释，workspace 是宿主给的工作目录、路径共享同一挂载，
        // 因此不套用 root 约束。
        let workspace = PathBuf::from(payload.workspace.clone());
        let total = thumbnail_count(media.duration_seconds);
        let stem = file_stem_of(&media.file_name);

        let (sender, receiver) = mpsc::channel(STREAM_BUFFER);
        tokio::spawn(async move {
            if let Err(source) = tokio::fs::create_dir_all(&workspace).await {
                let _ = sender
                    .send(Err(Status::failed_precondition(format!(
                        "宿主提供的工作目录不可用：{source}"
                    ))))
                    .await;
                return;
            }

            let mut artifacts: Vec<ThumbnailArtifact> = Vec::with_capacity(total as usize);
            for index in 1..=total {
                // 本 crate 不解码视频，落一个占位文件代表「第 index 张真图」。
                let artifact = format!("{stem}-{index:04}.jpg");
                if let Err(source) =
                    tokio::fs::write(workspace.join(&artifact), b"placeholder").await
                {
                    let _ = sender
                        .send(Err(Status::internal(format!(
                            "写入缩略图产物 {artifact} 失败：{source}"
                        ))))
                        .await;
                    return;
                }
                // 均分到整条时长上：第 i 张落在 i/(total+1) 处，不落在首尾帧
                // （首帧常是黑场、末帧常是片尾）。
                let divisor = i64::from(total) + 1;
                let offset_seconds = (media.duration_seconds.max(0) * i64::from(index)) / divisor;
                artifacts.push(ThumbnailArtifact {
                    offset_seconds: offset_seconds as i32,
                    relative_path: artifact.clone(),
                });

                let progress = ProgressEvent {
                    text: format!("已生成第 {index}/{total} 张：{artifact}"),
                    current: index,
                    total,
                };
                if sender
                    .send(Ok(GenerateThumbnailsResponse {
                        payload: Some(generate_thumbnails_response::Payload::Progress(progress)),
                    }))
                    .await
                    .is_err()
                {
                    // 客户端断开：直接收尾，不要往已关闭的流里塞。
                    return;
                }
            }

            // ★ 以 `done` 收尾 —— 宿主**凭这一条**落库。
            // （P1-1 修订前流就这样结束了，宿主拿不到产物清单。）
            let _ = sender
                .send(Ok(GenerateThumbnailsResponse {
                    payload: Some(generate_thumbnails_response::Payload::Done(
                        ThumbnailGeneration {
                            expected_count: total,
                            artifacts,
                        },
                    )),
                }))
                .await;
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(receiver))))
    }
}

/// 目录里的一条目，已经排好序。
struct DirectoryItem {
    name: String,
    is_directory: bool,
    size_bytes: Option<i64>,
    modified_at: Option<String>,
    is_video: bool,
}

/// 读取并排序一层目录。文件与目录混排，按文件名字典序。
async fn list_dir(directory: &Path) -> Result<Vec<DirectoryItem>, Status> {
    let mut reader = tokio::fs::read_dir(directory).await.map_err(|source| {
        Status::not_found(format!("无法读取目录 {}：{source}", directory.display()))
    })?;

    let mut items = Vec::new();
    while let Some(entry) = reader
        .next_entry()
        .await
        .map_err(|source| Status::internal(format!("读取目录项失败：{source}")))?
    {
        let name = entry.file_name().to_string_lossy().into_owned();
        let file_type = entry
            .file_type()
            .await
            .map_err(|source| Status::internal(format!("读取文件类型失败：{source}")))?;

        if file_type.is_symlink() {
            // GAP: EntryType 只有 FILE / DIRECTORY，符号链接没有归属，
            // 这里直接跳过以免把「穿越借口」暴露给上层——更好的做法需要 proto 表态。
            continue;
        }
        if file_type.is_dir() {
            let modified = tokio::fs::metadata(entry.path())
                .await
                .ok()
                .and_then(|info| rfc3339(info.modified().ok()?));
            items.push(DirectoryItem {
                name,
                is_directory: true,
                size_bytes: None,
                modified_at: modified,
                is_video: false,
            });
            continue;
        }
        let info = tokio::fs::metadata(entry.path()).await.map_err(|source| {
            Status::internal(format!(
                "读取 {} 元数据失败：{source}",
                entry.path().display()
            ))
        })?;
        items.push(DirectoryItem {
            is_video: is_video_extension(&name),
            name,
            is_directory: false,
            size_bytes: Some(clamp_i64(info.len())),
            modified_at: info.modified().ok().and_then(rfc3339),
        });
    }

    // GAP: proto 没有给 Browse 提供排序方式 / 排序字段，宿主也无法要求
    // 「目录在前」或「按修改时间倒序」。本地插件只能自定一套。
    items.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(items)
}

/// 目录项 → BrowseEntry。
fn browse_entry(parent_rel: &str, item: &DirectoryItem) -> BrowseEntry {
    BrowseEntry {
        source_ref: Some(string_ref(&join_ref(parent_rel, &item.name))),
        name: item.name.clone(),
        entry_type: if item.is_directory {
            EntryType::Directory
        } else {
            EntryType::File
        } as i32,
        size_bytes: item.size_bytes,
        modified_at: item.modified_at.clone(),
        is_video: item.is_video,
    }
}

/// 把 parent_ref 的路径与本级名字拼成下一级引用的 `path`。
fn join_ref(parent_rel: &str, name: &str) -> String {
    if parent_rel.is_empty() {
        name.to_owned()
    } else {
        format!("{parent_rel}/{name}")
    }
}

/// limit → 实际页大小。
fn page_size(limit: i32) -> usize {
    if limit <= 0 {
        return DEFAULT_PAGE_SIZE;
    }
    (limit as usize).min(MAX_PAGE_SIZE)
}

/// 游标（上一页最后一个条目名）→ 本页起始下标。
fn cursor_offset(items: &[DirectoryItem], cursor: Option<&str>) -> Result<usize, Status> {
    let Some(cursor) = cursor else { return Ok(0) };
    let index = items
        .iter()
        .position(|item| item.name == cursor)
        .ok_or_else(|| Status::invalid_argument(format!("游标 {cursor} 已失效")))?;
    Ok(index + 1)
}

/// 递归遍历并把每个文件作为 [`ImportFileEntry`] 送进通道。
///
/// 顺序是确定的：**同一层按文件名字典序，本层文件先出，子目录后下沉**。
/// 这让「事件顺序」可以被断言 —— 流式 ABI 能否信赖，取决于这件事。
async fn walk_and_emit(
    base: PathBuf,
    start: PathBuf,
    sender: mpsc::Sender<Result<ImportFileEntry, Status>>,
) {
    let mut stack = vec![start];

    while let Some(directory) = stack.pop() {
        let mut reader = match tokio::fs::read_dir(&directory).await {
            Ok(reader) => reader,
            Err(source) => {
                let _ = sender
                    .send(Err(Status::internal(format!(
                        "读取 {} 失败：{source}",
                        directory.display()
                    ))))
                    .await;
                return;
            }
        };

        let mut level: Vec<(String, PathBuf, bool)> = Vec::new();
        while let Some(entry) = match reader.next_entry().await {
            Ok(entry) => entry,
            Err(source) => {
                let _ = sender
                    .send(Err(Status::internal(format!("读取目录项失败：{source}"))))
                    .await;
                return;
            }
        } {
            let path = entry.path();
            let Ok(file_type) = entry.file_type().await else {
                continue;
            };
            if file_type.is_symlink() {
                continue;
            }
            level.push((
                entry.file_name().to_string_lossy().into_owned(),
                path,
                file_type.is_dir(),
            ));
        }
        level.sort_by(|left, right| left.0.cmp(&right.0));

        let mut subdirectories = Vec::new();
        for (name, path, is_directory) in level {
            if is_directory {
                subdirectories.push(path);
                continue;
            }
            let Ok(metadata) = tokio::fs::metadata(&path).await else {
                continue;
            };
            // `Path` 的 `Display` 用**平台分隔符** —— Windows 上会产出
            // `b-movies\c-nested\deep-01.mov`。而 `source_ref` 与
            // `relative_path` 是协议字段，契约是 POSIX 的 `/`（`fixture.rs`
            // 取文件名也用 `rsplit('/')`）。所以不能直接 `to_string_lossy()`，
            // 要按组件拼。
            //
            // 这不只是测试洁癖：宿主拿到的 ref 要当 storage key 用，
            // Windows 上分隔符不一致会让同一文件在两个平台上产生两条记录。
            let relative_path_ref = path.strip_prefix(&base).unwrap_or(&path);
            let relative = relative_path_ref
                .components()
                .map(|component| component.as_os_str().to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            let entry = ImportFileEntry {
                file: Some(ImportFile {
                    source_ref: Some(string_ref(&relative)),
                    is_video: is_video_extension(&name),
                    name,
                    relative_path: relative,
                    size_bytes: clamp_i64(metadata.len()),
                }),
            };
            if sender.send(Ok(entry)).await.is_err() {
                return;
            }
        }
        // 栈是后进先出：倒着压才能让字典序的第一个子目录先被访问。
        for subdirectory in subdirectories.into_iter().rev() {
            stack.push(subdirectory);
        }
    }
}

/// 采样张数。真实实现按视频时长每 `THUMBNAIL_INTERVAL_SECONDS` 秒一张。
fn thumbnail_count(duration_seconds: i64) -> i32 {
    if duration_seconds <= 0 {
        return THUMBNAIL_MIN as i32;
    }
    let wanted = duration_seconds / THUMBNAIL_INTERVAL_SECONDS;
    let counted = wanted.clamp(THUMBNAIL_MIN, THUMBNAIL_MAX);
    i32::try_from(counted).unwrap_or(i32::MAX)
}

/// `file://` URL。路径里的空格等字符必须转义，否则宿主的 HTTP 客户端会解析出错。
///
/// # 分隔符必须转成 `/`
///
/// 和上面 `walk_and_emit` 里的 `relative_path` 是**同一个 bug 的第二处**：
/// `Path` 的 `Display` 用平台分隔符，Windows 上是 `\`，而 URL 的路径部分是
/// 由 `/` 分隔的。`file://C:\dir\a.mkv` 不是一个合法 URL —— 宿主拿去解析会
/// 拿到错误的 host 或空的 path。
///
/// Windows 上还要多一个斜杠：`C:\dir\a.mkv` 的正确形态是
/// `file:///C:/dir/a.mkv`（`file://` + 空 host + `/C:/...`）。直接用
/// `path.to_string_lossy()` 得到的是 `C:\...`，拼出来是 `file://C:\...`
/// —— 少一个斜杠，host 段会被解析成 `C:`。
fn file_url(path: &Path) -> String {
    let raw = path.to_string_lossy().replace('\\', "/");
    // 盘符绝对路径（`C:/...`）前面要补 `/` 才是合法的 file URL。
    let needs_leading_slash = !raw.starts_with('/');
    let body = if needs_leading_slash {
        format!("/{raw}")
    } else {
        raw
    };
    format!("file://{}", utf8_percent_encode(&body, FILE_URL_ESCAPE))
}

/// 按扩展名猜 Content-Type。
fn content_type_of(file_name: &str) -> Option<String> {
    let extension = extension_of(file_name)?;
    let mime = match extension.as_str() {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "avi" => "video/x-msvideo",
        "ts" => "video/mp2t",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        _ => return None,
    };
    Some(mime.to_owned())
}

/// 按扩展名判断是不是视频。
fn is_video_extension(file_name: &str) -> bool {
    extension_of(file_name)
        .map(|extension| VIDEO_EXTENSIONS.contains(&extension.as_str()))
        .unwrap_or(false)
}

/// 取小写扩展名。
fn extension_of(file_name: &str) -> Option<String> {
    Some(
        Path::new(file_name)
            .extension()?
            .to_string_lossy()
            .to_ascii_lowercase(),
    )
}

/// 去掉扩展名的文件名主干。
fn file_stem_of(file_name: &str) -> String {
    Path::new(file_name)
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_name.to_owned())
}

/// SystemTime → RFC 3339（`BrowseEntry.modified_at` 要求的格式）。
fn rfc3339(instant: SystemTime) -> Option<String> {
    let moment = DateTime::<Utc>::from(instant).to_rfc3339_opts(SecondsFormat::Secs, true);
    Some(moment)
}

/// u64 → i64，溢出时给 i64::MAX 而不是回绕。
fn clamp_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

/// 词法规范化：去掉 `.` 与 `..`，相对路径补上工作目录。
fn absolutize(path: &Path) -> PathBuf {
    let candidate = match path.is_absolute() {
        true => path.to_path_buf(),
        false => std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf()),
    };
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other.as_os_str()),
        }
    }
    normalized
}
