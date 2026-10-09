//! 字幕资产**写/读两侧**的连库对拍（上游 `subtitle_asset_service.py` +
//! `movie_subtitle_service.py`）。
//!
//! # 这个文件盯的是什么
//!
//! 字幕是**两套东西拼出来的**：库里的 `subtitle` 行（只有 `movie_id` +
//! `file_path`），磁盘上的文件（大小、格式、内容都从它来）。两侧任何一侧漂了，
//! 症状都不是报错：
//!
//! * 写侧落盘路径与读侧校验的根目录不一致 → 播放页**永远没有字幕**；
//! * 读侧把「文件没了」当 404 而不是 409 → 客户端以为字幕被删，不再重试同步。

use sm_db::repo::{MovieRepository, NewSubtitle, SubtitleRepository};
use sm_db::testing::TestDb;
use sm_service::catalog::media_paths::movie_subtitle_dir;
use sm_service::catalog::movie_subtitle::MovieSubtitleService;
use sm_service::catalog::subtitle_asset::{
    SubtitleAssetService, SubtitleImportStatus, SubtitleRegistrationStatus,
};

mod support;
use support::{n, seed_movie_if_missing, ImageRoot};

/// 一份最小的合法 SRT。
const SRT: &[u8] = b"1\n00:00:01,000 --> 00:00:02,000\nhello\n";

async fn movie_id_of(db: &TestDb, movie_number: &str) -> i32 {
    MovieRepository::new(db.pool().clone())
        .find_by_number(movie_number)
        .await
        .expect("查 movie")
        .expect("movie 应该存在")
        .id
}

/// ★ 内容导入：落盘 + 登记 + 同内容去重。
///
/// 去重的判据是**内容指纹**，不是文件名 —— 同一部影片里
/// `ABC-123.srt` 与 `ABC-123.chs.srt` 内容相同时只该存一份。
#[tokio::test]
async fn importing_content_writes_the_file_and_the_row_then_detects_the_duplicate() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let service = SubtitleAssetService::new(db.pool(), &root.config);
    let movie_number = format!("SUB-{}", n());
    seed_movie_if_missing(&db, &movie_number).await;

    let first = service
        .import_subtitle_content(&movie_number, SRT, "zh.srt", None)
        .await
        .expect("导入第一份");
    assert_eq!(first.status, SubtitleImportStatus::Imported);
    assert!(first.subtitle_id.is_some(), "导入成功要回 id");
    assert!(first.reason.is_none(), "成功时没有拒绝原因");

    // 落点必须是**该番号的字幕目录**，而且文件名是分配出来的 `<番号>-1.srt`
    // （不是插件给的名字 —— 上游也是分配）。
    let dir = movie_subtitle_dir(&root.config, &movie_number).expect("字幕目录");
    let written = dir.join(format!("{movie_number}-1.srt"));
    assert!(written.is_file(), "{} 应该被写出来", written.display());
    assert_eq!(std::fs::read(&written).expect("读回"), SRT);

    // 同内容再来一次：`duplicate`，**不是失败**，也不该写出第二个文件。
    let second = service
        .import_subtitle_content(&movie_number, SRT, "zh.chs.srt", None)
        .await
        .expect("重复导入");
    assert_eq!(second.status, SubtitleImportStatus::Duplicate);
    assert!(second.subtitle_id.is_none());
    assert!(
        !dir.join(format!("{movie_number}-2.srt")).exists(),
        "重复内容不该再落一个文件"
    );

    // 不同内容 → 新分配一个序号。
    let other = b"1\n00:00:05,000 --> 00:00:06,000\nworld\n";
    let third = service
        .import_subtitle_content(&movie_number, other, "zh.srt", None)
        .await
        .expect("第三份");
    assert_eq!(third.status, SubtitleImportStatus::Imported);
    assert!(dir.join(format!("{movie_number}-2.srt")).is_file());
}

/// ★ 扩展名不在白名单 → `invalid_format`，且**不落盘、不登记**。
///
/// `.sub` 也要拒：上游白名单是四项（`.srt/.ass/.ssa/.vtt`），没有 `.sub`。
#[tokio::test]
async fn an_unsupported_extension_is_rejected_without_touching_the_disk() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let service = SubtitleAssetService::new(db.pool(), &root.config);
    let movie_number = format!("SUB-{}", n());
    seed_movie_if_missing(&db, &movie_number).await;

    for file_name in ["a.mkv", "a.sub", "a", ".srt"] {
        let result = service
            .import_subtitle_content(&movie_number, SRT, file_name, None)
            .await
            .expect("不该抛错");
        assert_eq!(
            result.status,
            SubtitleImportStatus::InvalidFormat,
            "{file_name} 该被拒"
        );
        assert!(
            result
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("不支持的扩展名")),
            "{file_name} 的拒绝原因要照上游文案"
        );
    }
    let dir = movie_subtitle_dir(&root.config, &movie_number).expect("字幕目录");
    assert!(!dir.exists(), "被拒的导入不该建目录/落文件");
}

/// ★ 番号不存在 → `movie_not_found`（不是抛错，也不是 500）。
#[tokio::test]
async fn an_unknown_movie_number_is_movie_not_found() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let service = SubtitleAssetService::new(db.pool(), &root.config);

    let result = service
        .import_subtitle_content(&format!("NOPE-{}", n()), SRT, "a.srt", None)
        .await
        .expect("不该抛错");
    assert_eq!(result.status, SubtitleImportStatus::MovieNotFound);
    assert!(result.subtitle_id.is_none());
    assert!(result
        .reason
        .as_deref()
        .is_some_and(|reason| reason.contains("影片不存在")));
}

/// ★ 列表的 `format` / `size_bytes` / `file_name` 全部来自**磁盘**。
///
/// 库里没有这几列（`subtitle` 只有 `movie_id` + `file_path`），所以一旦实现从
/// 别处取，就会出现「库里说有、磁盘上没有」的死链。
#[tokio::test]
async fn the_listing_reads_size_and_format_from_disk() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let writer = SubtitleAssetService::new(db.pool(), &root.config);
    let reader = MovieSubtitleService::new(db.pool(), &root.config);
    let movie_number = format!("SUB-{}", n());
    seed_movie_if_missing(&db, &movie_number).await;
    writer
        .import_subtitle_content(&movie_number, SRT, "zh.srt", None)
        .await
        .expect("导入");

    let listed = reader
        .get_movie_subtitles(&movie_number)
        .await
        .expect("列表");
    assert_eq!(listed.movie_number, movie_number);
    assert_eq!(listed.items.len(), 1, "只该有一条有效字幕");
    let item = &listed.items[0];
    assert_eq!(item.file_name, format!("{movie_number}-1.srt"));
    assert_eq!(item.format, "srt", "扩展名不带点、小写");
    assert_eq!(item.size_bytes, SRT.len() as i64, "大小来自 stat");
    assert!(item.created_at.is_some(), "登记时刻来自记录");
}

/// ★ 读内容：字节原样返回 + sha256 稳定。
#[tokio::test]
async fn reading_returns_the_bytes_and_a_stable_sha256() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let writer = SubtitleAssetService::new(db.pool(), &root.config);
    let reader = MovieSubtitleService::new(db.pool(), &root.config);
    let movie_number = format!("SUB-{}", n());
    seed_movie_if_missing(&db, &movie_number).await;
    let imported = writer
        .import_subtitle_content(&movie_number, SRT, "zh.srt", None)
        .await
        .expect("导入");
    let subtitle_id = imported.subtitle_id.expect("有 id");
    let movie_id = movie_id_of(&db, &movie_number).await;

    let content = reader
        .read_subtitle_content(movie_id, subtitle_id)
        .await
        .expect("读内容");
    assert_eq!(content.subtitle_id, subtitle_id);
    assert_eq!(content.content, SRT);
    assert_eq!(content.sha256.len(), 64, "SHA-256 十六进制 64 位");

    // 内容不同 → 指纹不同（用另一份内容互证，避免「自己等于自己」空转）。
    let other = writer
        .import_subtitle_content(
            &movie_number,
            b"1\n00:00:09,000 --> 00:00:10,000\nx\n",
            "en.srt",
            None,
        )
        .await
        .expect("再导入一份");
    let other_content = reader
        .read_subtitle_content(movie_id, other.subtitle_id.expect("有 id"))
        .await
        .expect("读第二份");
    assert_ne!(other_content.sha256, content.sha256);
}

/// ★ 记录在、文件没了 → 列表**跳过**，读报 **409**（不是 404）。
///
/// 报 404 会让客户端以为字幕被删了，从而不再重试同步。
#[tokio::test]
async fn a_missing_file_is_skipped_by_the_list_but_conflicts_on_read() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let writer = SubtitleAssetService::new(db.pool(), &root.config);
    let reader = MovieSubtitleService::new(db.pool(), &root.config);
    let movie_number = format!("SUB-{}", n());
    seed_movie_if_missing(&db, &movie_number).await;
    let imported = writer
        .import_subtitle_content(&movie_number, SRT, "zh.srt", None)
        .await
        .expect("导入");
    let subtitle_id = imported.subtitle_id.expect("有 id");
    let movie_id = movie_id_of(&db, &movie_number).await;

    let dir = movie_subtitle_dir(&root.config, &movie_number).expect("字幕目录");
    std::fs::remove_file(dir.join(format!("{movie_number}-1.srt"))).expect("删掉文件");

    let listed = reader
        .get_movie_subtitles(&movie_number)
        .await
        .expect("列表");
    assert!(listed.items.is_empty(), "文件没了就不该出现在列表里");

    let error = reader
        .read_subtitle_content(movie_id, subtitle_id)
        .await
        .expect_err("读不到");
    assert_eq!(error.status, 409, "记录还在 —— 是冲突不是 404");
    assert_eq!(error.code(), "subtitle_unavailable");
}

/// ★ 记录指向字幕目录**之外**（或别的分片目录）→ 读报 **403**，不是 409/404。
///
/// 这条是路径逃逸防护的连库版：`file_path` 字段是插件写进来的，读侧不能信它。
#[tokio::test]
async fn a_record_pointing_outside_the_subtitle_dir_is_a_403() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let reader = MovieSubtitleService::new(db.pool(), &root.config);
    let movie_number = format!("SUB-{}", n());
    seed_movie_if_missing(&db, &movie_number).await;
    let movie_id = movie_id_of(&db, &movie_number).await;

    // 文件**真的存在**（不是 `.srt` 在白名单外的写法），但落在该番号的字幕目录
    // 之外 —— 分片目录写错是最像「正常数据」的那种坏数据。
    let outside = root.root().join("elsewhere").join("outside.srt");
    std::fs::create_dir_all(outside.parent().expect("父目录")).expect("建目录");
    std::fs::write(&outside, SRT).expect("写文件");
    let row = SubtitleRepository::new(db.pool().clone())
        .create(&NewSubtitle {
            movie_id,
            file_path: outside.to_string_lossy().into_owned(),
        })
        .await
        .expect("登记");

    let error = reader
        .read_subtitle_content(movie_id, row.id)
        .await
        .expect_err("路径非法");
    assert_eq!(error.status, 403);
    assert_eq!(error.code(), "subtitle_path_invalid");

    // 列表同样跳过它（上游 `:44-45` 捕异常后 continue）。
    let listed = reader
        .get_movie_subtitles(&movie_number)
        .await
        .expect("列表");
    assert!(listed.items.is_empty(), "非法路径不该出现在列表里");
}

/// ★ 双向同步：补登记扫到的文件，删掉失效记录。
#[tokio::test]
async fn sync_creates_missing_rows_and_deletes_stale_ones() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let service = MovieSubtitleService::new(db.pool(), &root.config);
    let movie_number = format!("SUB-{}", n());
    seed_movie_if_missing(&db, &movie_number).await;
    let movie_id = movie_id_of(&db, &movie_number).await;

    // 磁盘上有一个「别的途径放进来的」字幕文件（没登记过）。
    let dir = movie_subtitle_dir(&root.config, &movie_number).expect("字幕目录");
    std::fs::create_dir_all(&dir).expect("建字幕目录");
    std::fs::write(dir.join(format!("{movie_number}-1.srt")), SRT).expect("写文件");

    // 两条坏记录：一条路径合法但文件不在；一条路径不合法（别的分片目录）。
    let repo = SubtitleRepository::new(db.pool().clone());
    repo.create(&NewSubtitle {
        movie_id,
        file_path: dir
            .join(format!("{movie_number}-99.srt"))
            .to_string_lossy()
            .into_owned(),
    })
    .await
    .expect("登记不存在的文件");
    repo.create(&NewSubtitle {
        movie_id,
        file_path: root
            .root()
            .join("movies")
            .join("zz")
            .join(&movie_number)
            .join("subtitles")
            .join("bad.srt")
            .to_string_lossy()
            .into_owned(),
    })
    .await
    .expect("登记非法路径");

    let summary = service.sync_movie_subtitles(movie_id).await.expect("同步");
    assert_eq!(summary["created_subtitles"], 1, "扫到 1 个并补登记");
    assert_eq!(summary["deleted_subtitles"], 2, "两条坏记录都该删");
    assert_eq!(summary["total_subtitles"], 1, "同步后只剩那个真实文件");

    // 再同步一次是幂等的：什么都不增不减。
    let again = service
        .sync_movie_subtitles(movie_id)
        .await
        .expect("再同步");
    assert_eq!(again["created_subtitles"], 0);
    assert_eq!(again["deleted_subtitles"], 0);
    assert_eq!(again["total_subtitles"], 1);
}

/// ★ 本地文件登记：硬链接优先，同内容第二次是 `skipped` +
/// `duplicate_fingerprint`。
#[tokio::test]
async fn registering_a_local_file_skips_duplicates_and_reports_the_reason() {
    let db = TestDb::require().await;
    let root = ImageRoot::new();
    let service = SubtitleAssetService::new(db.pool(), &root.config);
    let movie_number = format!("SUB-{}", n());
    seed_movie_if_missing(&db, &movie_number).await;
    let movie = MovieRepository::new(db.pool().clone())
        .find_by_number(&movie_number)
        .await
        .expect("查 movie")
        .expect("存在");

    // 源文件放在字幕目录之外（模拟「从媒体库搬过来」）。
    let source_dir = root.root().join("incoming");
    std::fs::create_dir_all(&source_dir).expect("建目录");
    let source = source_dir.join("from-library.srt");
    std::fs::write(&source, SRT).expect("写源文件");

    let first = service
        .register_subtitle_file(&movie, &source, None, "auto")
        .await
        .expect("登记");
    assert_eq!(first.status, SubtitleRegistrationStatus::Imported);
    assert!(first.reason.is_none());
    assert!(
        std::path::Path::new(&first.detail).is_file(),
        "detail 是目标路径：{}",
        first.detail
    );

    // 同内容再从**另一个文件名**来一次 → skipped/duplicate_fingerprint。
    let second_source = source_dir.join("again.srt");
    std::fs::write(&second_source, SRT).expect("写第二份源文件");
    let second = service
        .register_subtitle_file(&movie, &second_source, None, "auto")
        .await
        .expect("登记第二份");
    assert_eq!(second.status, SubtitleRegistrationStatus::Skipped);
    assert_eq!(second.reason.as_deref(), Some("duplicate_fingerprint"));
    assert_eq!(second.detail, "again.srt", "跳过时 detail 是源文件名");
}
