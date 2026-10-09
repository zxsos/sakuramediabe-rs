//! 集成测试：起**真实** server（`127.0.0.1` + 内核分配的随机端口），
//! 用真实的 tonic client 跑往返。
//!
//! 每个用例各自起一个 server、各自铺一棵临时数据集，互不共享状态；
//! 随机端口也让三条并行线可以同时跑而不抢端口。

use std::path::PathBuf;

use plugin_ref_local::fixture::{
    expected_size, library_handle, media_handle, populate_tree, scratch_root, ROOT_ENTRY_NAMES,
    SCAN_RELATIVE_ORDER,
};
use plugin_ref_local::{connect, spawn, string_ref, LocalRefProvider};
use sm_plugin_api::v1::{
    generate_thumbnails_response, storage_provider_client::StorageProviderClient, BrowseRequest,
    EntryType, GenerateThumbnailsRequest, PlanPlaybackRequest, PlaybackDelivery,
    ReadImportFileRequest, ScanImportSourceRequest, ThumbnailGeneration,
};
use tokio_stream::StreamExt;

/// 铺数据集 + 起 server + 连客户端。
async fn start() -> (PathBuf, StorageProviderClient<tonic::transport::Channel>) {
    let root = scratch_root("roundtrip");
    populate_tree(&root).await.expect("铺设本地数据集");
    let (addr, _server) = spawn(LocalRefProvider::new(root.clone()))
        .await
        .expect("启动参考插件 server");
    let client = connect(addr).await.expect("连接本地 server");
    (root, client)
}

// ── Browse（一元）────────────────────────────────────────────────

#[tokio::test]
async fn browse_pages_in_order_and_emits_cursor() {
    let (_root, mut client) = start().await;

    let first = client
        .browse(BrowseRequest {
            library: Some(library_handle(7)),
            parent_ref: None,
            cursor: None,
            limit: 2,
        })
        .await
        .expect("Browse 第一页")
        .into_inner();

    let names: Vec<&str> = first.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec![ROOT_ENTRY_NAMES[0], ROOT_ENTRY_NAMES[1]]);
    assert_eq!(first.next_cursor.as_deref(), Some(ROOT_ENTRY_NAMES[1]));

    let second = client
        .browse(BrowseRequest {
            library: Some(library_handle(7)),
            parent_ref: None,
            cursor: first.next_cursor.clone(),
            limit: 2,
        })
        .await
        .expect("Browse 第二页")
        .into_inner();

    let names: Vec<&str> = second.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec![ROOT_ENTRY_NAMES[2]]);
    // 流（这里是分页）正常结束：最后一页不再给游标。
    assert_eq!(second.next_cursor, None);
}

#[tokio::test]
async fn browse_marks_directories_files_and_sizes() {
    let (_root, mut client) = start().await;

    let page = client
        .browse(BrowseRequest {
            library: Some(library_handle(7)),
            parent_ref: None,
            cursor: None,
            limit: 0, // proto 没规定默认值：<=0 走 provider 自己的兜底页大小
        })
        .await
        .expect("Browse 根")
        .into_inner();

    assert_eq!(page.entries.len(), ROOT_ENTRY_NAMES.len());
    let directory = &page.entries[0];
    assert_eq!(directory.entry_type, EntryType::Directory as i32);
    assert_eq!(directory.size_bytes, None);
    assert!(!directory.is_video);
    // 导航所需的一切都在不透明引用里，宿主只能原样回传。
    assert!(directory.source_ref.is_some());

    let readme = &page.entries[2];
    assert_eq!(readme.entry_type, EntryType::File as i32);
    assert_eq!(readme.size_bytes, Some(expected_size("z-readme.txt")));
    assert!(!readme.is_video);
}

#[tokio::test]
async fn browse_describes_nested_directory() {
    let (_root, mut client) = start().await;

    let page = client
        .browse(BrowseRequest {
            library: Some(library_handle(7)),
            parent_ref: Some(string_ref("b-movies")),
            cursor: None,
            limit: 10,
        })
        .await
        .expect("Browse 子目录")
        .into_inner();

    let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["c-nested", "movie-01.mkv"]);
    let movie = &page.entries[1];
    assert_eq!(movie.entry_type, EntryType::File as i32);
    assert_eq!(
        movie.size_bytes,
        Some(expected_size("b-movies/movie-01.mkv"))
    );
    assert!(movie.is_video);
}

#[tokio::test]
async fn browse_rejects_path_escape_and_missing_library() {
    let (_root, mut client) = start().await;

    let escaped = client
        .browse(BrowseRequest {
            library: Some(library_handle(7)),
            parent_ref: Some(string_ref("../../etc")),
            cursor: None,
            limit: 10,
        })
        .await;
    assert_eq!(
        escaped.unwrap_err().code(),
        tonic::Code::InvalidArgument,
        "逃出根目录的路径必须被拒"
    );

    let no_library = client
        .browse(BrowseRequest {
            library: None,
            parent_ref: None,
            cursor: None,
            limit: 10,
        })
        .await;
    assert_eq!(no_library.unwrap_err().code(), tonic::Code::InvalidArgument);

    let gone = client
        .browse(BrowseRequest {
            library: Some(library_handle(7)),
            parent_ref: Some(string_ref("not-there")),
            cursor: None,
            limit: 10,
        })
        .await;
    assert_eq!(gone.unwrap_err().code(), tonic::Code::NotFound);
}

// ── PlanPlayback（一元）──────────────────────────────────────────

#[tokio::test]
async fn plan_playback_returns_a_local_path_plan() {
    let (_root, mut client) = start().await;

    let response = client
        .plan_playback(PlanPlaybackRequest {
            media: Some(media_handle(
                42,
                library_handle(7),
                "b-movies/movie-01.mkv",
                120,
            )),
            resource_path: String::new(),
            delivery: PlaybackDelivery::Redirect as i32,
        })
        .await
        .expect("PlanPlayback")
        .into_inner();

    let plan = response.plan.expect("PlaybackPlan 不能为空");
    assert!(!plan.unavailable);
    assert_eq!(plan.file_name, "movie-01.mkv");
    assert_eq!(
        plan.size_bytes,
        Some(expected_size("b-movies/movie-01.mkv"))
    );
    assert_eq!(plan.content_type.as_deref(), Some("video/x-matroska"));
    match plan.delivery.expect("必须给出投放方式") {
        sm_plugin_api::v1::playback_plan::Delivery::LocalPath(local) => {
            // ★ 是**路径**，不是 URL：没有 `file://` 前缀，也没有百分号转义。
            // 宿主直接拿它 `open`。拼 URL 那条老路（`file://C:\dir\a.mkv`）客户端
            // 不认，表现是「点播放没反应」而宿主侧看起来一切正常。
            assert!(
                !local.path.starts_with("file://"),
                "给的是路径不是 URL：{}",
                local.path
            );
            // 按**路径分量**比对，这样平台分隔符（`\` / `/`）都不影响。
            assert!(
                std::path::Path::new(&local.path).ends_with("b-movies/movie-01.mkv"),
                "应当指向那个文件：{}",
                local.path
            );
        }
        other => panic!("应当是 LocalPath，实际是 {other:?}"),
    }
}

#[tokio::test]
async fn plan_playback_marks_missing_media_unavailable() {
    let (_root, mut client) = start().await;

    let plan = client
        .plan_playback(PlanPlaybackRequest {
            media: Some(media_handle(
                43,
                library_handle(7),
                "b-movies/gone.mkv",
                120,
            )),
            resource_path: String::new(),
            delivery: PlaybackDelivery::Redirect as i32,
        })
        .await
        .expect("PlanPlayback 缺失媒体")
        .into_inner()
        .plan
        .expect("plan");

    // proto 只留了一个布尔位：宿主无从知道这是「文件没了」还是「被黑名单了」。
    assert!(plan.unavailable);
    assert_eq!(plan.delivery, None);
}

/// ★ 请求里的 `delivery` **不影响答案** —— 本地文件的投递方式只有一种。
///
/// 上游 `local_provider.handle_playback` 就是这个语义：`context.delivery` 只影响
/// 它拼出来的 URL（starlette 的事），最终一律自己读字节。
///
/// 反过来写（把 `proxy` 拒成 `unsupported`）会让客户端「换个投递方式重试」拿到
/// 422 `provider_playback_delivery_unsupported` —— 而它其实没有第二种选择。
#[tokio::test]
async fn plan_playback_answers_local_path_for_any_requested_delivery() {
    let (_root, mut client) = start().await;

    for (index, requested) in [
        PlaybackDelivery::Unspecified,
        PlaybackDelivery::Proxy,
        PlaybackDelivery::Redirect,
    ]
    .into_iter()
    .enumerate()
    {
        let media_id = 50 + i64::try_from(index).expect("小数字");
        let plan = client
            .plan_playback(PlanPlaybackRequest {
                media: Some(media_handle(
                    media_id,
                    library_handle(7),
                    "b-movies/movie-01.mkv",
                    120,
                )),
                resource_path: String::new(),
                delivery: requested as i32,
            })
            .await
            .unwrap_or_else(|error| panic!("delivery={requested:?} 不该报错：{error}"))
            .into_inner()
            .plan
            .expect("plan");

        assert!(
            matches!(
                plan.delivery,
                Some(sm_plugin_api::v1::playback_plan::Delivery::LocalPath(_))
            ),
            "delivery={requested:?} 也应当是 LocalPath，实际是 {:?}",
            plan.delivery
        );
    }
}

// ── GenerateThumbnails（server streaming）────────────────────────

#[tokio::test]
async fn generate_thumbnails_streams_progress_in_order() {
    let (_root, mut client) = start().await;
    let workspace = scratch_root("thumbnails");

    let duration_seconds = 100;
    let expected_total: i32 = 10; // 100s / 10s
    let mut stream = client
        .generate_thumbnails(GenerateThumbnailsRequest {
            library: Some(library_handle(7)),
            media: Some(media_handle(
                1,
                library_handle(7),
                "b-movies/movie-01.mkv",
                duration_seconds,
            )),
            workspace: workspace.to_string_lossy().into_owned(),
        })
        .await
        .expect("GenerateThumbnails")
        .into_inner();

    // 分成两类收：进度事件与**终态产物清单**（P1-1 修订后才有的后者）。
    let mut progress = Vec::new();
    let mut done: Option<ThumbnailGeneration> = None;
    while let Some(frame) = stream.next().await {
        let frame = frame.expect("流不应出错");
        match frame.payload {
            Some(generate_thumbnails_response::Payload::Progress(event)) => progress.push(event),
            Some(generate_thumbnails_response::Payload::Done(generation)) => {
                assert!(done.is_none(), "`done` 只能出现一次");
                done = Some(generation);
            }
            None => panic!("`GenerateThumbnailsResponse.payload` 必须有值"),
        }
    }
    // 不靠超时判定结束：上面这行在流正常关闭时返回 None。

    assert_eq!(
        progress.len(),
        usize::try_from(expected_total).expect("数量为小正整数"),
        "进度事件数量必须完整"
    );
    for (index, event) in progress.iter().enumerate() {
        let step = i32::try_from(index + 1).expect("步进");
        assert_eq!(event.current, step, "进度顺序不能乱");
        assert_eq!(event.total, expected_total);
        assert!(event.text.contains(&format!("{step}/{expected_total}")));
    }

    // ★ 流必须以 `done` 收尾 —— 宿主**凭这一条**落库。
    // 修订前宿主只能扫磁盘猜产物名字（`ThumbnailGeneration` 没有通道）。
    let generation = done.expect("流必须以 `done` 收尾");
    assert_eq!(generation.expected_count, expected_total);
    assert_eq!(
        generation.artifacts.len(),
        usize::try_from(expected_total).expect("数量"),
        "产物清单必须与生成的数量一致"
    );
    // 偏移要单调递增且落在区间内（第 i 张落在 i/(total+1) 处）。
    let mut previous = -1;
    for artifact in &generation.artifacts {
        assert!(
            artifact.offset_seconds > previous,
            "偏移必须严格递增：{previous} -> {}",
            artifact.offset_seconds
        );
        assert!(
            artifact.offset_seconds > 0 && i64::from(artifact.offset_seconds) < duration_seconds,
            "偏移要落在 (0, duration) 内，不取首尾帧：{}",
            artifact.offset_seconds
        );
        previous = artifact.offset_seconds;
        // `relative_path` 必须能直接拼到 workspace 上找到文件。
        assert!(
            workspace.join(&artifact.relative_path).is_file(),
            "产物 {} 应当真的在 workspace 里",
            artifact.relative_path
        );
    }
}

#[tokio::test]
async fn cancelling_a_stream_does_not_break_the_server() {
    let (_root, mut client) = start().await;
    let workspace = scratch_root("cancelled");

    let mut stream = client
        .generate_thumbnails(GenerateThumbnailsRequest {
            library: Some(library_handle(7)),
            media: Some(media_handle(
                1,
                library_handle(7),
                "a-clips/clip-01.mp4",
                240,
            )),
            workspace: workspace.to_string_lossy().into_owned(),
        })
        .await
        .expect("GenerateThumbnails")
        .into_inner();

    let first = stream.next().await.expect("首帧").expect("首帧不应出错");
    let Some(generate_thumbnails_response::Payload::Progress(event)) = first.payload else {
        panic!("首帧应当是进度事件：{first:?}");
    };
    assert_eq!(event.current, 1);
    drop(stream); // 客户端提前断开

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    // 同一个连接上的后续调用必须照常可用。
    let page = client
        .browse(BrowseRequest {
            library: Some(library_handle(7)),
            parent_ref: None,
            cursor: None,
            limit: 1,
        })
        .await
        .expect("断流后 Browse 仍应可用")
        .into_inner();
    assert_eq!(page.entries.len(), 1);
}

// ── ScanImportSource（server streaming）──────────────────────────

#[tokio::test]
async fn scan_import_source_streams_files_in_order() {
    let (_root, mut client) = start().await;

    let mut stream = client
        .scan_import_source(ScanImportSourceRequest {
            library: Some(library_handle(7)),
            source_ref: Some(string_ref("")),
        })
        .await
        .expect("ScanImportSource")
        .into_inner();

    let mut seen = 0;
    while let Some(entry) = stream.next().await {
        let entry = entry.expect("扫描条目不应出错").file.expect("ImportFile");
        assert_eq!(
            entry.relative_path, SCAN_RELATIVE_ORDER[seen],
            "第 {seen} 个条目的顺序不对"
        );
        assert_eq!(
            entry.size_bytes,
            expected_size(&entry.relative_path),
            "{} 的大小不对",
            entry.relative_path
        );
        let should_be_video = SCAN_RELATIVE_ORDER[seen].ends_with(".mp4")
            || SCAN_RELATIVE_ORDER[seen].ends_with(".mkv")
            || SCAN_RELATIVE_ORDER[seen].ends_with(".mov");
        assert_eq!(entry.is_video, should_be_video);
        seen += 1;
    }

    assert_eq!(seen, SCAN_RELATIVE_ORDER.len(), "必须扫全，且流正常结束");
}

#[tokio::test]
async fn scan_import_source_accepts_file_root_and_rejects_missing() {
    let (_root, mut client) = start().await;

    let missing = client
        .scan_import_source(ScanImportSourceRequest {
            library: Some(library_handle(7)),
            source_ref: Some(string_ref("no-such-dir")),
        })
        .await;
    assert_eq!(missing.unwrap_err().code(), tonic::Code::NotFound);

    let mut single = client
        .scan_import_source(ScanImportSourceRequest {
            library: Some(library_handle(7)),
            source_ref: Some(string_ref("a-clips")),
        })
        .await
        .expect("扫描单个子目录")
        .into_inner();

    let mut entries = 0;
    while let Some(entry) = single.next().await {
        entry.expect("子目录扫描条目");
        entries += 1;
    }
    assert_eq!(entries, 2);
}

// ── 未实现的方法 ─────────────────────────────────────────────────

#[tokio::test]
async fn unimplemented_rpc_reports_unimplemented() {
    let (_root, mut client) = start().await;

    // tonic 0.14 生成的 trait 没有默认方法体：32 个方法必须逐个写，
    // 写了也多半只是把自己登记为 unimplemented（报告 §4.1）。
    let error = client
        .read_import_file(ReadImportFileRequest {
            library: Some(library_handle(7)),
            source: None,
        })
        .await
        .expect_err("未实现的方法必须报错");
    assert_eq!(error.code(), tonic::Code::Unimplemented);
}
