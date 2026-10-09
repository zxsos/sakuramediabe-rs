//! 粗测：一元 RPC 与流式首帧的往返延迟。
//!
//! 目的不是出 benchmark，而是回答「插件拆进程后，每次跨进程调用要付多少钱」
//! 这道门槛题。数据是同一台机器上的回环调用，**只求量级**，
//! 结论写进 `docs/parallel/grpc-plugin-report.md` §3。
//!
//! 跑法：`cargo test -p plugin-ref-local --test latency -- --nocapture`

use std::path::Path;
use std::time::Instant;

use plugin_ref_local::fixture::{library_handle, media_handle, populate_tree, scratch_root};
use plugin_ref_local::latency::{summarize, Summary};
use plugin_ref_local::{connect, spawn, string_ref, LocalRefProvider};
use sm_plugin_api::v1::{
    generate_thumbnails_response, storage_provider_client::StorageProviderClient, BrowseRequest,
    GenerateThumbnailsRequest, ReadImportFileRequest, ScanImportSourceRequest,
};
use tokio_stream::StreamExt;

/// 预热次数：把 HTTP/2 握手、线程池冷启动的影响隔离掉。
const UNARY_WARMUP: usize = 100;
/// 一元样本数。
const UNARY_SAMPLES: usize = 1000;
/// 流式预热次数。
const STREAM_WARMUP: usize = 20;
/// 流式样本数（每条样本是一次完整的流）。
const STREAM_SAMPLES: usize = 200;

/// 单机回环的宽松上限。真实观测值在百微秒量级，留一万倍余量只为兜住「卡死」。
const P95_CEILING_US: u128 = 1_000_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn round_trip_latency() {
    let root = scratch_root("latency");
    populate_tree(&root).await.expect("铺设本地数据集");
    let workspace = scratch_root("latency-thumbs");

    let (addr, _server) = spawn(LocalRefProvider::new(root.clone()))
        .await
        .expect("启动参考插件 server");
    let mut client = connect(addr).await.expect("连接本地 server");

    let unary = measure_unary(&mut client, UNARY_SAMPLES).await;
    let transport = measure_transport_only(&mut client, UNARY_SAMPLES).await;
    let first_frame = measure_stream(&mut client, &workspace, STREAM_SAMPLES).await;
    let scan_frame = measure_scan(&mut client, STREAM_SAMPLES).await;

    println!();
    println!("| 项目 | 样本 | min(μs) | p50(μs) | p95(μs) | max(μs) |");
    println!("|---|---:|---:|---:|---:|---:|");
    row("一元 RPC 纯传输（服务端零工作）", &transport);
    row("一元 RPC + 一次目录列举（Browse）", &unary);
    row("流式首帧（GenerateThumbnails，含写文件）", &first_frame);
    row("流式首帧（ScanImportSource，只读目录）", &scan_frame);
    println!();
    println!("环境：本机回环 HTTP/2，客户端与服务端同进程、同 tokio 多线程运行时（debug 构建）。");

    for (label, summary) in [
        ("纯传输", &transport),
        ("Browse", &unary),
        ("流式首帧", &first_frame),
        ("扫描首帧", &scan_frame),
    ] {
        assert!(
            summary.p95_us < P95_CEILING_US,
            "{label} p95 {}μs 超出单机回环的合理上限",
            summary.p95_us
        );
    }
}

/// 「把插件拆出去要付多少」这件事本身的价格：服务端什么都不做就返回。
///
/// 用 `read_import_file`（直接退 `Unimplemented`）而不是加一个 no-op rpc，
/// 因为 proto 不允许改 —— 这是本线唯一能拿到的零业务逻辑一元调用。
async fn measure_transport_only(
    client: &mut StorageProviderClient<tonic::transport::Channel>,
    samples: usize,
) -> Summary {
    let mut collected = Vec::with_capacity(samples);
    for round in 0..(UNARY_WARMUP + samples) {
        let started = Instant::now();
        client
            .read_import_file(ReadImportFileRequest {
                library: Some(library_handle(7)),
                source: None,
            })
            .await
            .expect_err("未实现的方法必须报错");
        let elapsed = started.elapsed();

        if round >= UNARY_WARMUP {
            collected.push(elapsed);
        }
    }
    summarize(&collected)
}

/// 一元 RPC 的往返延迟样本。前 `UNARY_WARMUP` 次不计入。
async fn measure_unary(
    client: &mut StorageProviderClient<tonic::transport::Channel>,
    samples: usize,
) -> Summary {
    let mut collected = Vec::with_capacity(samples);
    for round in 0..(UNARY_WARMUP + samples) {
        let started = Instant::now();
        let response = client
            .browse(BrowseRequest {
                library: Some(library_handle(7)),
                parent_ref: None,
                cursor: None,
                limit: 10,
            })
            .await
            .expect("Browse 延迟测量");
        let elapsed = started.elapsed();

        assert_eq!(response.into_inner().entries.len(), 3);
        if round >= UNARY_WARMUP {
            collected.push(elapsed);
        }
    }
    summarize(&collected)
}

/// 流式调用「建流 → 首帧」的延迟样本。
///
/// 每次都完整收完 5 帧再收下一次：这样测的是冷启动流量的首帧，
/// 而不是同一个生产者被反复唤醒的情况。
async fn measure_stream(
    client: &mut StorageProviderClient<tonic::transport::Channel>,
    workspace: &Path,
    samples: usize,
) -> Summary {
    let workspace = workspace.to_string_lossy().into_owned();
    let mut collected = Vec::with_capacity(samples);

    for round in 0..(STREAM_WARMUP + samples) {
        let started = Instant::now();
        let mut stream = client
            .generate_thumbnails(GenerateThumbnailsRequest {
                library: Some(library_handle(7)),
                media: Some(media_handle(
                    1,
                    library_handle(7),
                    "a-clips/clip-01.mp4",
                    40,
                )),
                workspace: workspace.clone(),
            })
            .await
            .expect("GenerateThumbnails 延迟测量")
            .into_inner();

        let first = stream.next().await.expect("首帧").expect("首帧不应出错");
        let elapsed = started.elapsed();
        let Some(generate_thumbnails_response::Payload::Progress(event)) = first.payload else {
            panic!("首帧应当是进度事件：{first:?}");
        };
        assert_eq!(event.current, 1);

        // 收完剩余帧，确保生产端不会因为「被丢下」而留下脏任务。
        let mut remaining = 1;
        while let Some(frame) = stream.next().await {
            frame.expect("后续帧不应出错");
            remaining += 1;
        }
        // 5 = 4 个进度 + 1 个终态 `done`（P1-1 修订后流多了一条）。
        // 进度条数 = `thumbnail_count(40s)` = 40 / 10 = 4（见 `provider.rs` 的采样规则）。
        assert_eq!(remaining, 5);

        if round >= STREAM_WARMUP {
            collected.push(elapsed);
        }
    }
    summarize(&collected)
}

/// 只读流的首帧延迟：服务端只读目录、不写文件。
///
/// 与 [`measure_stream`] 成对出现，是为了把「tokio 阻塞线程池上做文件 IO」
/// 的抖动和 gRPC 自身的抖动分开。
async fn measure_scan(
    client: &mut StorageProviderClient<tonic::transport::Channel>,
    samples: usize,
) -> Summary {
    let mut collected = Vec::with_capacity(samples);
    for round in 0..(STREAM_WARMUP + samples) {
        let started = Instant::now();
        let mut stream = client
            .scan_import_source(ScanImportSourceRequest {
                library: Some(library_handle(7)),
                source_ref: Some(string_ref("b-movies/c-nested")),
            })
            .await
            .expect("ScanImportSource 延迟测量")
            .into_inner();

        let first = stream.next().await.expect("首帧").expect("首帧不应出错");
        let elapsed = started.elapsed();
        assert_eq!(
            first.file.expect("ImportFile").relative_path,
            "b-movies/c-nested/deep-01.mov"
        );

        while let Some(entry) = stream.next().await {
            entry.expect("后续帧不应出错");
        }

        if round >= STREAM_WARMUP {
            collected.push(elapsed);
        }
    }
    summarize(&collected)
}

fn row(label: &str, summary: &Summary) {
    println!(
        "| {} | {} | {} | {} | {} | {} |",
        label, summary.count, summary.min_us, summary.p50_us, summary.p95_us, summary.max_us
    );
}
