//! 集成测试：**宿主侧的 storage 调用面**（`sm_plugins::provider_calls`）打到真实 provider。
//!
//! # 为什么另起一个文件，而不是加在 `roundtrip.rs` 里
//!
//! `roundtrip.rs` 用的是 tonic **生成的客户端**（`StorageProviderClient` 直接调
//! `client.browse(...)`）—— 它验证的是「插件实现对不对」。而宿主真正发出去的每
//! 一次 provider 调用走的都是 `provider_calls` 那 17 个包装函数：转不转得对字段、
//! 流收没收干、错误解不解得出来，全靠那层。**在 2026-10-09 之前，那层一次都没被
//! 真实的 provider 打过。**
//!
//! `provider_calls.rs` 里的单测（`mod tests`）覆盖的是 `classify_status` 的**分
//! 类逻辑**，用**手搓的 `Status`** 喂进去。所以这里刻意**不**重复那些：
//!
//! | 只可能在这里验的东西 | 为什么单测/`roundtrip.rs` 都验不到 |
//! |---|---|
//! | 不透明引用 `Struct` ↔ `serde_json::Value` 转换 | `roundtrip.rs` 用 `prost_types::Struct` 手搓请求，绕过了转换 |
//! | 结构化错误**过线**后仍完好 | 单测手搓 `Status`；编码/解码（`to_status` ↔ `from_status`）只有真发一次才知道 |
//! | `scan_import_source_all` 真的把流收干 | 单测碰不到流 |
//! | `generate_thumbnails` 的进度回调管不管用 | 同上 |
//! | 端点约定（`connect_storage` 的 `endpoint` 串） | 单测不连任何东西 |
//!
//! 数据集与端口分配的纪律同 `roundtrip.rs`：每个用例各自铺一棵临时树、各自起
//! server（内核分配端口），互不共享状态。

use std::path::PathBuf;

use plugin_ref_local::fixture::{
    expected_size, library_handle, media_handle, populate_tree, scratch_root, SCAN_RELATIVE_ORDER,
};
use plugin_ref_local::provider::PROVIDER_KEY;
use plugin_ref_local::{spawn, LocalRefProvider};
use sm_plugin_api::json_struct::struct_to_json;
use sm_plugin_api::v1::playback_plan;
use sm_plugin_api::v1::storage_provider_client::StorageProviderClient;
use sm_plugin_api::v1::EntryType;
use sm_plugins::provider_calls::{
    browse, connect_storage, generate_thumbnails, plan_playback, scan_import_source_all,
};
use tonic::transport::Channel;

/// 铺数据集 + 起 provider + 用**宿主侧**的 [`connect_storage`] 拿客户端。
///
/// ★ 客户端是 `connect_storage` 给的，不是 `plugin_ref_local::connect` —— 这一行
/// 本身就是被测对象的一部分（端点串怎么拼、连不上怎么报）。
async fn start() -> (PathBuf, StorageProviderClient<Channel>) {
    let root = scratch_root("provider-calls");
    populate_tree(&root).await.expect("铺设本地数据集");
    let (addr, _server) = spawn(LocalRefProvider::new(root.clone()))
        .await
        .expect("启动参考插件 server");
    let client = connect_storage(PROVIDER_KEY, &format!("http://{addr}"), "test_connect")
        .await
        .expect("宿主侧连上 provider");
    (root, client)
}

/// 本地 provider 的不透明引用格式：`{"path": "<相对 root 的路径>"}`。
///
/// 与 `plugin_ref_local::string_ref` 是同一份 schema 的两种表示 —— 这里刻意手写
/// JSON 而不是拿 `Struct` 转：宿主的调用面**只**认 JSON（`json_to_struct`）。
fn source_ref(path: &str) -> serde_json::Value {
    serde_json::json!({ "path": path })
}

// ── Browse（一元，**单页**）────────────────────────────────────────

/// ★ 宿主侧只取**一页**：`next_cursor` 原样透传，由客户端决定要不要下一页。
///
/// 这条用例同时钉住三件客户端依赖的事：分页不丢条目、`next_cursor` 不被吞、
/// 条目的不透明引用与字段原样过线。
#[tokio::test]
async fn browse_returns_one_page_and_passes_the_cursor_through() {
    let (_root, mut client) = start().await;

    // 第一页：只取两条，剩下的靠游标。
    let first = browse(&mut client, PROVIDER_KEY, library_handle(7), None, None, 2)
        .await
        .expect("宿主侧 Browse 第一页");

    let names: Vec<&str> = first.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["a-clips", "b-movies"],
        "顺序即 provider 给的顺序"
    );
    // 游标是不透明的：宿主**只负责带回来**，不解析（参考插件用的是「本页最后一条的
    // 名字」，别的 provider 可以放 base64 —— 所以别在宿主侧假设它的形状）。
    assert_eq!(first.next_cursor.as_deref(), Some("b-movies"));

    let directory = &first.entries[0];
    assert_eq!(directory.entry_type, EntryType::Directory as i32);
    assert_eq!(directory.size_bytes, None, "目录没有大小");
    assert!(!directory.is_video, "目录不是视频");
    // ★ 不透明引用过线：宿主只保存与回传，前端点进子目录时原样带回来。
    assert_eq!(
        struct_to_json(directory.source_ref.as_ref()),
        source_ref("a-clips"),
        "目录的引用必须是 provider 给的那份"
    );

    // 第二页：带着上一页的游标继续。
    let second = browse(
        &mut client,
        PROVIDER_KEY,
        library_handle(7),
        None,
        first.next_cursor.as_deref(),
        2,
    )
    .await
    .expect("宿主侧 Browse 第二页");

    let names: Vec<&str> = second.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, vec!["z-readme.txt"]);
    assert_eq!(second.next_cursor, None, "最后一页不再给游标");
    let readme = &second.entries[0];
    assert_eq!(readme.entry_type, EntryType::File as i32);
    assert_eq!(readme.size_bytes, Some(expected_size("z-readme.txt")));
    assert!(!readme.is_video, "`.txt` 不是视频");
}

/// 进子目录：`parent_ref` 就是上一条的 `source_ref`，宿主一个字节都不改。
#[tokio::test]
async fn browse_descends_with_the_parent_ref_it_was_given() {
    let (_root, mut client) = start().await;

    let page = browse(
        &mut client,
        PROVIDER_KEY,
        library_handle(7),
        Some(&source_ref("b-movies")),
        None,
        10,
    )
    .await
    .expect("宿主侧 Browse 子目录");

    let names: Vec<&str> = page.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["c-nested", "movie-01.mkv"],
        "目录与文件同一个列表"
    );
    let movie = &page.entries[1];
    assert_eq!(movie.entry_type, EntryType::File as i32);
    assert!(movie.is_video);
    assert_eq!(
        movie.size_bytes,
        Some(expected_size("b-movies/movie-01.mkv"))
    );
    assert_eq!(
        struct_to_json(movie.source_ref.as_ref()),
        source_ref("b-movies/movie-01.mkv"),
        "下一级的引用应当是父引用 + 本级名字"
    );
}

/// 浏览的两个失败方向归到**不同的码** —— 客户端对它们的处置不同
/// （「换个目录」vs「这个源本身有问题」）。
#[tokio::test]
async fn browse_classifies_a_missing_directory_and_a_path_escape_differently() {
    let (_root, mut client) = start().await;

    // 目录不在：参考插件给裸 `Status::not_found`（没带结构），宿主按码猜。
    let missing = browse(
        &mut client,
        PROVIDER_KEY,
        library_handle(7),
        Some(&source_ref("not-there")),
        None,
        10,
    )
    .await
    .expect_err("不存在的目录必须报错");
    assert_eq!(missing.code(), "source_not_found");
    assert_eq!(missing.operation, "browse");
    assert!(!missing.retryable(), "目录不在，重试还是不在");

    // 逃出根目录：插件**主动**拒（`invalid_argument`），归到 `invalid_config`。
    // 与上一条的区别是「请求本身不合法」而不是「东西不在」。
    let escaped = browse(
        &mut client,
        PROVIDER_KEY,
        library_handle(7),
        Some(&source_ref("../../etc")),
        None,
        10,
    )
    .await
    .expect_err("逃出根目录必须报错");
    assert_eq!(escaped.code(), "invalid_config");
    assert_eq!(escaped.operation, "browse");
}

/// ⚠️ 不透明引用的**根不是对象**时，宿主当场拒 —— 不能降级成「没给」。
///
/// 这是静默错误的典型：`{"path": …}` 传成 `"a/b"` 时，`None` 对 provider 意味着
/// **库根 / 整个库**，所以「传错了」会表现成「给你列了根目录」这种看起来正常的
/// 结果。上游靠 pydantic 的 `dict[str, Any]` 在路由层挡住（422），宿主侧这道闸
/// 是它的对应物 —— 两个入口各钉一条。
#[tokio::test]
async fn a_non_object_opaque_ref_is_rejected_instead_of_falling_back_to_the_root() {
    let (_root, mut client) = start().await;

    let browsed = browse(
        &mut client,
        PROVIDER_KEY,
        library_handle(7),
        Some(&serde_json::json!("a/b")),
        None,
        10,
    )
    .await
    .expect_err("字符串不是不透明引用");
    assert_eq!(browsed.code(), "invalid_config");
    assert_eq!(browsed.operation, "browse");
    assert!(!browsed.retryable());

    let scanned = scan_import_source_all(
        &mut client,
        PROVIDER_KEY,
        library_handle(7),
        &serde_json::json!(["a/b"]),
    )
    .await
    .expect_err("数组不是不透明引用");
    assert_eq!(scanned.code(), "invalid_config");
    assert_eq!(scanned.operation, "scan_import_source");
}

// ── ScanImportSource（server streaming，宿主侧收成 Vec）──────────────

#[tokio::test]
async fn scan_import_source_all_drains_the_stream_and_keeps_refs() {
    let (_root, mut client) = start().await;

    let entries = scan_import_source_all(
        &mut client,
        PROVIDER_KEY,
        library_handle(7),
        &source_ref(""),
    )
    .await
    .expect("宿主侧扫描");

    // 顺序与完整性：宿主侧「收干成 Vec」的语义就是把 provider 的流一字不改地排下来。
    let paths: Vec<&str> = entries.iter().map(|e| e.relative_path.as_str()).collect();
    assert_eq!(paths, SCAN_RELATIVE_ORDER.to_vec(), "顺序与条数都要完整");

    for entry in &entries {
        assert_eq!(
            entry.size_bytes,
            expected_size(&entry.relative_path),
            "{} 的大小不对",
            entry.relative_path
        );
        assert_eq!(
            entry.name,
            entry
                .relative_path
                .rsplit('/')
                .next()
                .expect("有文件名")
                .to_owned(),
            "`name` 应当是相对路径的末段"
        );
        // ★ 不透明引用过了一趟 `Struct` → `serde_json::Value`，宿主必须能原样回传。
        // 回传时它就靠这一份值（`parent_ref` / `source_ref` 都是这么走的）。
        assert_eq!(
            entry.source_ref,
            source_ref(&entry.relative_path),
            "{} 的不透明引用过线后必须还是同一份",
            entry.relative_path
        );
    }
}

#[tokio::test]
async fn a_structured_provider_error_survives_the_wire() {
    let (_root, mut client) = start().await;

    let error = scan_import_source_all(
        &mut client,
        PROVIDER_KEY,
        library_handle(7),
        &source_ref("no-such-dir"),
    )
    .await
    .expect_err("不存在的来源必须报错");

    // 参考插件按契约报**结构化**错误（见 `provider.rs` 里那段示范），所以
    // `code` / `safe_message` / `retryable` 三项都该无损到达 —— 一个字段掉在
    // tonic 的 `details` 编解码里，这里就会看出来。
    assert_eq!(error.code(), "source_not_found");
    assert_eq!(
        error.safe_message(),
        "导入来源不存在",
        "provider 给的对外文案要原样用，不是宿主模板"
    );
    assert!(
        !error.retryable(),
        "provider 显式说了 retryable=false，宿主不能自作主张"
    );
    // 归属信息由宿主填（它才知道自己在替谁发这条 rpc）。
    assert_eq!(error.provider_key, PROVIDER_KEY);
    assert_eq!(error.operation, "scan_import_source");
}

// ── PlanPlayback（一元）────────────────────────────────────────────

#[tokio::test]
async fn plan_playback_returns_the_local_path_plan() {
    let (_root, mut client) = start().await;

    let response = plan_playback(
        &mut client,
        PROVIDER_KEY,
        media_handle(42, library_handle(7), "b-movies/movie-01.mkv", 120),
        "",
        sm_plugin_api::v1::PlaybackDelivery::Redirect as i32,
    )
    .await
    .expect("宿主侧 PlanPlayback");

    let plan = response.plan.expect("plan 不能为空");
    assert!(!plan.unavailable);
    assert_eq!(plan.file_name, "movie-01.mkv");
    assert_eq!(
        plan.size_bytes,
        Some(expected_size("b-movies/movie-01.mkv"))
    );
    match plan.delivery.expect("必须给出投放方式") {
        playback_plan::Delivery::LocalPath(local) => {
            // 是**路径**不是 URL（同 `roundtrip.rs` 的红线）：宿主直接拿它 `open`。
            assert!(!local.path.starts_with("file://"), "给的是路径");
            assert!(
                std::path::Path::new(&local.path).ends_with("b-movies/movie-01.mkv"),
                "应当指向那个文件：{}",
                local.path
            );
        }
        other => panic!("应当是 LocalPath，实际是 {other:?}"),
    }
}

/// ★ 否定结果是**正常应答**：`Ok` + `unavailable`，不是 `Err`。
///
/// 宿主侧包装把这两种情况分开得很细：`Err` = provider 出问题（502 一类），
/// `Ok(unavailable)` = 资源不在了（404）。混起来会把「影片文件被删」报成
/// 「插件坏了」—— 这条只有真的打一次才能确认它没被包装层吞掉。
#[tokio::test]
async fn plan_playback_negative_answer_is_still_ok() {
    let (_root, mut client) = start().await;

    let plan = plan_playback(
        &mut client,
        PROVIDER_KEY,
        media_handle(43, library_handle(7), "b-movies/gone.mkv", 120),
        "",
        sm_plugin_api::v1::PlaybackDelivery::Proxy as i32,
    )
    .await
    .expect("文件不在不是调用失败")
    .plan
    .expect("plan");

    assert!(plan.unavailable);
    assert_eq!(plan.delivery, None);
}

// ── GenerateThumbnails（server streaming + 进度回调）────────────────

#[tokio::test]
async fn generate_thumbnails_collects_artifacts_and_feeds_progress_back() {
    let (_root, mut client) = start().await;
    let workspace = scratch_root("provider-calls-thumbnails");

    let duration_seconds = 100;
    let expected_total = 10; // 100s / 10s
                             // 回调必须**自己拥有**缓冲区：`ThumbnailProgress` 这个别名里的
                             // `dyn FnMut` 默认带 `'static`（类型别名里的 trait object 不带省略生命周期），
                             // 借用栈上的 `Vec` 过不了编译。
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<(String, i32, i32)>::new()));
    let sink = std::sync::Arc::clone(&seen);
    let mut callback = move |text: &str, current: i32, total: i32| {
        sink.lock()
            .expect("进度缓冲锁")
            .push((text.to_owned(), current, total));
    };

    let result = generate_thumbnails(
        &mut client,
        PROVIDER_KEY,
        library_handle(7),
        media_handle(
            1,
            library_handle(7),
            "b-movies/movie-01.mkv",
            duration_seconds,
        ),
        &workspace.to_string_lossy(),
        Some(&mut callback),
    )
    .await
    .expect("宿主侧 GenerateThumbnails");
    let seen = seen.lock().expect("进度缓冲锁");

    // 回调与计数必须对得上：宿主靠回调驱动任务进度，靠计数落库。
    assert_eq!(result.progress_events, expected_total as u32);
    assert_eq!(
        seen.len(),
        usize::try_from(result.progress_events).expect("计数为小正整数"),
        "每条进度事件都该回调一次"
    );
    assert_eq!(result.expected_count, expected_total as u32);
    assert_eq!(
        result.artifacts.len(),
        usize::try_from(result.expected_count).expect("数量"),
        "产物清单就是落库依据"
    );

    // 回调拿到的序数必须与流里的顺序一致（宿主拿它算百分比）。
    for (index, (text, current, total)) in seen.iter().enumerate() {
        let step = i32::try_from(index + 1).expect("步进");
        assert_eq!(*current, step, "回调顺序不能乱");
        assert_eq!(*total, expected_total);
        assert!(text.contains(&format!("{step}/{expected_total}")), "{text}");
    }

    // `relative_path` 必须能直接拼到 workspace 上 —— 宿主就是这么找文件的。
    for artifact in &result.artifacts {
        assert!(
            workspace.join(&artifact.relative_path).is_file(),
            "产物 {} 应当真的在 workspace 里",
            artifact.relative_path
        );
    }
}

// ── 未实现的 rpc：宿主侧归类 ────────────────────────────────────────

/// 参考插件那 28 个未实现的 rpc 返回 `Unimplemented`，宿主必须归类成
/// `unsupported`（而不是「未知失败」）。**这类错误的来源只有真打一次才知道**。
#[tokio::test]
async fn an_unimplemented_rpc_is_classified_as_unsupported() {
    let (_root, mut client) = start().await;

    let error = sm_plugins::provider_calls::delete_media(
        &mut client,
        PROVIDER_KEY,
        library_handle(7),
        media_handle(1, library_handle(7), "b-movies/movie-01.mkv", 120),
    )
    .await
    .expect_err("参考插件没实现 delete_media");

    assert_eq!(error.code(), "unsupported");
    assert!(
        !error.retryable(),
        "重试同一个不支持的操作没有意义（provider 没给 retryable，宿主按码猜）"
    );
    assert_eq!(error.operation, "delete_media");
}

// ── 宿主侧的失败：连不上 ────────────────────────────────────────────

/// 端点没人听 → `unavailable` 且**可重试**。
///
/// 与上面那条刚好相反：那条是「provider 说不行，别重试」，这条是「宿主自己连不
/// 上，值得重试」。两者都走 `ProviderOperationError` 信封，只有 `code` 能分开 ——
/// 所以这条错了会让「插件还没起来」被当成「你的源不支持」。
#[tokio::test]
async fn connecting_to_a_dead_endpoint_reports_unavailable() {
    // 先占一个端口再放掉：内核给的这个号此刻没人听。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("占一个临时端口");
    let addr = listener.local_addr().expect("拿到端口");
    drop(listener);

    let error = connect_storage(PROVIDER_KEY, &format!("http://{addr}"), "test_connect")
        .await
        .expect_err("没人听的端口必须报错");

    assert_eq!(error.code(), "unavailable");
    assert!(error.retryable(), "稍后起来了就好了，值得重试");
    // 排障信息要带上端点 —— 否则「连不上」这句话没法查。
    assert!(
        error.plugin_detail().contains(&addr.to_string()),
        "细节里要有端点：{}",
        error.plugin_detail()
    );
}
