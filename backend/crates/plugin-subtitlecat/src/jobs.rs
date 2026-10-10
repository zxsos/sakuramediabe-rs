//! 两个任务的处理逻辑（上游 `jobs.py`）。
//!
//! # 上游对应
//!
//! | 上游 | 这里 |
//! |---|---|
//! | `jobs.py:run_fetch` | [`run_fetch`] |
//! | `jobs.py:run_subscribed` | [`run_subscribed`] |
//! | `jobs.py:_import_subtitles` | [`import_subtitles`] |
//! | `jobs.py:_record_import_result` | [`ImportCounts::record`] |
//! | `jobs.py:_iter_subscribed_movies` | [`iter_subscribed_movies`] |
//! | `jobs.py:_is_old_release` / `_subtract_calendar_months` | [`is_old_release`] / [`subtract_calendar_months`] |
//! | `jobs.py:_MANUAL_STAT_KEYS` / `_SUBSCRIBED_STAT_KEYS` | 两个 `*_STAT_KEYS` 常量 |
//!
//! # 与上游不同的地方
//!
//! 1. **宿主能力走 trait**。上游 `context.movies.find_by_numbers` /
//!    `context.movies.list_page` / `context.import_subtitle` 是进程内对象；
//!    拆成 gRPC 后由 [`SubtitleHost`] 抽象，生产实现是 `service.rs` 的
//!    `GrpcHost`，单测用假的 —— 与 `plugin-actor-metadata/src/jobs.rs` 同一
//!    组织方式。
//! 2. **进度事件不带 `summary_patch`**。proto 的 `ProgressEvent` 只有
//!    `current / total / text` 三个字段，上游挂在进度上的累计统计没有位置，
//!    只留在终态结果里。
//!
//! # 抓取失败在两个任务里待遇不同（照抄上游）
//!
//! - 手动任务：`SubtitleCatError` 直接往上抛 → 整次任务失败（`?`）；
//! - 订阅任务：单部抓取失败只 `failed += 1` 后继续下一部 —— 定时任务不能
//!   因为一部片被来源拉黑而全线停摆。

use async_trait::async_trait;
use chrono::{DateTime, Datelike, NaiveDate, Utc};

use crate::settings::Settings;
use crate::state::FetchState;
use crate::subtitlecat::{SubtitleCatClient, SubtitleCatError};

/// 上游 `_MANUAL_STAT_KEYS`（顺序照抄）。
pub const MANUAL_STAT_KEYS: [&str; 6] = [
    "source_matches",
    "imported",
    "duplicate",
    "movie_not_found",
    "invalid_format",
    "failed",
];

/// 上游 `_SUBSCRIBED_STAT_KEYS`（顺序照抄）。
pub const SUBSCRIBED_STAT_KEYS: [&str; 10] = [
    "subscribed",
    "eligible",
    "fetched",
    "skipped_old",
    "source_matches",
    "imported",
    "duplicate",
    "movie_not_found",
    "invalid_format",
    "failed",
];

/// 导入时给宿主看的语言标记（上游写死的 `language="zh-CN"`）。
pub const SUBTITLE_LANGUAGE: &str = "zh-CN";

/// 订阅任务分页的每页条数（上游 `limit=500`）。
pub const SUBSCRIBED_PAGE_LIMIT: i32 = 500;

/// 宿主导入字幕的结果（宿主的 `ImportSubtitleResponse.status`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportStatus {
    Imported,
    Duplicate,
    MovieNotFound,
    InvalidFormat,
    /// 宿主回了没见过的串。上游 `_record_import_result` 的 `else` 分支把它算作
    /// `failed` —— 这里保留这个「不认识的都当失败」的语义，而不是报错停下。
    Other,
}

impl ImportStatus {
    pub fn parse(raw: &str) -> Self {
        match raw {
            "imported" => Self::Imported,
            "duplicate" => Self::Duplicate,
            "movie_not_found" => Self::MovieNotFound,
            "invalid_format" => Self::InvalidFormat,
            _ => Self::Other,
        }
    }
}

/// 导入环节的五个计数（两个统计结构体共用）。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ImportCounts {
    pub imported: i64,
    pub duplicate: i64,
    pub movie_not_found: i64,
    pub invalid_format: i64,
    pub failed: i64,
}

impl ImportCounts {
    /// 上游 `_record_import_result`。
    pub fn record(&mut self, status: ImportStatus) {
        match status {
            ImportStatus::Imported => self.imported += 1,
            ImportStatus::Duplicate => self.duplicate += 1,
            ImportStatus::MovieNotFound => self.movie_not_found += 1,
            ImportStatus::InvalidFormat => self.invalid_format += 1,
            ImportStatus::Other => self.failed += 1,
        }
    }

    fn add(&mut self, other: Self) {
        self.imported += other.imported;
        self.duplicate += other.duplicate;
        self.movie_not_found += other.movie_not_found;
        self.invalid_format += other.invalid_format;
        self.failed += other.failed;
    }
}

/// 手动抓取的终态摘要（`_MANUAL_STAT_KEYS` 那六个键）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ManualStats {
    pub source_matches: i64,
    pub imports: ImportCounts,
}

impl ManualStats {
    /// 终态 `Struct`（键集与 `MANUAL_STAT_KEYS` 一致，零也在）。
    pub fn to_struct(&self) -> prost_types::Struct {
        sm_plugin_api::json_struct::json_to_struct(&serde_json::json!({
            "source_matches": self.source_matches,
            "imported": self.imports.imported,
            "duplicate": self.imports.duplicate,
            "movie_not_found": self.imports.movie_not_found,
            "invalid_format": self.imports.invalid_format,
            "failed": self.imports.failed,
        }))
        .unwrap_or_default()
    }
}

/// 订阅抓取的终态摘要（`_SUBSCRIBED_STAT_KEYS` 那十个键）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SubscribedStats {
    pub subscribed: i64,
    pub eligible: i64,
    pub fetched: i64,
    pub skipped_old: i64,
    pub source_matches: i64,
    pub imports: ImportCounts,
}

impl SubscribedStats {
    pub fn to_struct(&self) -> prost_types::Struct {
        sm_plugin_api::json_struct::json_to_struct(&serde_json::json!({
            "subscribed": self.subscribed,
            "eligible": self.eligible,
            "fetched": self.fetched,
            "skipped_old": self.skipped_old,
            "source_matches": self.source_matches,
            "imported": self.imports.imported,
            "duplicate": self.imports.duplicate,
            "movie_not_found": self.imports.movie_not_found,
            "invalid_format": self.imports.invalid_format,
            "failed": self.imports.failed,
        }))
        .unwrap_or_default()
    }
}

/// 宿主给的影片快照里本插件要的那几项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostMovie {
    pub movie_number: String,
    pub is_subscribed: bool,
    /// ISO 日期原样（宿主给 `YYYY-MM-DD`）；解析交给 [`parse_release_date`]。
    pub release_date: Option<String>,
}

/// 宿主机能力的窄接口（上游 `context.movies` / `context.import_subtitle`）。
///
/// 生产实现是 `service.rs` 的 `GrpcHost`；单测用它换掉网络。
#[async_trait]
pub trait SubtitleHost: Send {
    /// 上游 `context.movies.find_by_numbers`：找不到那一部就不出现在结果里。
    async fn find_movies_by_numbers(
        &mut self,
        movie_numbers: &[String],
    ) -> Result<Vec<HostMovie>, JobError>;
    /// 上游 `context.movies.list_page`：`id` 升序，`limit` 上限 1000。
    async fn list_movies(
        &mut self,
        after_id: i64,
        limit: i32,
    ) -> Result<(Vec<HostMovie>, Option<i64>), JobError>;
    /// 上游 `context.import_subtitle`。
    async fn import_subtitle(
        &mut self,
        movie_number: &str,
        content: &[u8],
        file_name: &str,
        language: &str,
    ) -> Result<ImportStatus, JobError>;
}

/// 进度出口（上游 `reporter.emit` 的能力子集）。
///
/// `Send` 是给 `tokio::spawn` 的：任务整个在后台跑，`&mut dyn ProgressReporter`
/// 会跨 await 活着。
pub trait ProgressReporter: Send {
    fn report(&mut self, current: i64, total: i64, text: String);
}

/// 任务失败。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobError {
    /// 给用户看的消息。
    pub message: String,
    /// 该翻成哪个 gRPC 码 —— `service.rs` 里翻。
    pub code: ErrorCode,
}

/// 错误的类别（只为了让失败在 gRPC 那层带上正确的码）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// 调用方给的东西不对（站点 4xx、参数）。
    InvalidArgument,
    /// 对端暂时不可用（网络、超时、5xx、宿主不在）。
    Unavailable,
    /// 其余（状态文件坏了、来源返回的不是字幕…）。
    Internal,
}

impl JobError {
    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: ErrorCode::Internal,
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: ErrorCode::InvalidArgument,
        }
    }

    /// 宿主回调失败：宿主不在或连接断了 —— 对插件而言就是「对端不可用」。
    pub fn host(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            code: ErrorCode::Unavailable,
        }
    }
}

impl std::fmt::Display for JobError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for JobError {}

impl From<SubtitleCatError> for JobError {
    fn from(err: SubtitleCatError) -> Self {
        let code = match &err {
            // 4xx：上游不重试、直接抛 —— 是「请求本身不对」。
            SubtitleCatError::ClientError(_) => ErrorCode::InvalidArgument,
            // 网络/5xx 重试耗尽 —— 是「站点暂时不行」。
            SubtitleCatError::Request(_) => ErrorCode::Unavailable,
            SubtitleCatError::InvalidSubtitle(_) => ErrorCode::Internal,
        };
        Self {
            message: err.to_string(),
            code,
        }
    }
}

impl From<crate::state::StateError> for JobError {
    fn from(err: crate::state::StateError) -> Self {
        Self::internal(err.to_string())
    }
}

/// 上游 `_utc_now`。
pub fn utc_now() -> DateTime<Utc> {
    Utc::now()
}

/// 上游 `_subtract_calendar_months`：按**日历月**回退，日超出当月天数时取当月最后一天。
pub fn subtract_calendar_months(value: NaiveDate, months: u32) -> Option<NaiveDate> {
    let index = i64::from(value.year()) * 12 + i64::from(value.month0()) - i64::from(months);
    let year = i32::try_from(index.div_euclid(12)).ok()?;
    let month = u32::try_from(index.rem_euclid(12)).ok()?.checked_add(1)?;
    let day = value.day().min(days_in_month(year, month)?);
    NaiveDate::from_ymd_opt(year, month, day)
}

/// 当月天数（`chrono` 没有直接的 API，用下月一日回退一天取）。
fn days_in_month(year: i32, month: u32) -> Option<u32> {
    if !(1..=12).contains(&month) {
        return None;
    }
    let (next_year, next_month) = if month == 12 {
        (year.checked_add(1)?, 1)
    } else {
        (year, month + 1)
    };
    let first_of_next = NaiveDate::from_ymd_opt(next_year, next_month, 1)?;
    Some(first_of_next.pred_opt()?.day())
}

/// 上游 `_parse_release_date`：只认能取到日期部分的值，认不出就当没有。
pub fn parse_release_date(raw: Option<&str>) -> Option<NaiveDate> {
    let text = raw?.trim();
    if text.is_empty() {
        return None;
    }
    // 快照通常给 `YYYY-MM-DD`；多带时间后缀时取前 10 个字符（上游 `text[:10]`）。
    let head: String = text.chars().take(10).collect();
    NaiveDate::parse_from_str(&head, "%Y-%m-%d").ok()
}

/// 上游 `_is_old_release`：没有发布时间时**无法证明**它是老片，保守地继续抓。
pub fn is_old_release(release_date: Option<&str>, now: NaiveDate, months: u32) -> bool {
    let Some(release_date) = parse_release_date(release_date) else {
        return false;
    };
    let Some(cutoff) = subtract_calendar_months(now, months) else {
        return false;
    };
    // 上游是 `<=`：正好卡在边界那天也算老片。
    release_date <= cutoff
}

/// 上游 `_iter_subscribed_movies`：按 id 游标翻页，只留 `is_subscribed`。
pub async fn iter_subscribed_movies(
    host: &mut dyn SubtitleHost,
) -> Result<Vec<HostMovie>, JobError> {
    let mut after_id = 0i64;
    let mut subscribed = Vec::new();
    loop {
        let (movies, next_cursor) = host.list_movies(after_id, SUBSCRIBED_PAGE_LIMIT).await?;
        subscribed.extend(movies.into_iter().filter(|movie| movie.is_subscribed));
        let Some(cursor) = next_cursor else {
            return Ok(subscribed);
        };
        if cursor <= after_id {
            // 上游在这里 `raise RuntimeError` —— 游标不动就是死循环，宁可失败。
            return Err(JobError::host("宿主影片分页游标没有向前推进"));
        }
        after_id = cursor;
    }
}

/// 上游 `_import_subtitles`：逐份交给宿主导入，边导边报进度。
///
/// 传输层的失败往上抛（上游同样没有 catch）；`invalid_format` 之类是**结果**，
/// 只计数。
async fn import_subtitles(
    host: &mut dyn SubtitleHost,
    movie_number: &str,
    subtitles: &[Vec<u8>],
    reporter: &mut dyn ProgressReporter,
    progress_current: Option<i64>,
    progress_total: i64,
    progress_prefix: &str,
) -> Result<ImportCounts, JobError> {
    let total = subtitles.len();
    let mut counts = ImportCounts::default();
    for (offset, content) in subtitles.iter().enumerate() {
        let index = offset as i64 + 1;
        // 上游 `f"{movie_number}-{index}.srt"`（从 1 开始）。
        let file_name = format!("{movie_number}-{index}.srt");
        let status = host
            .import_subtitle(movie_number, content, &file_name, SUBTITLE_LANGUAGE)
            .await?;
        counts.record(status);
        reporter.report(
            progress_current.unwrap_or(index),
            progress_total,
            format!("{progress_prefix} {index}/{total}"),
        );
    }
    Ok(counts)
}

/// 上游 `run_fetch`：手动抓单部。**先查宿主影片**，避免对不存在的番号打外部请求。
pub async fn run_fetch(
    host: &mut dyn SubtitleHost,
    client: &SubtitleCatClient,
    state: &FetchState,
    movie_number: &str,
    reporter: &mut dyn ProgressReporter,
    now: DateTime<Utc>,
) -> Result<ManualStats, JobError> {
    let mut stats = ManualStats::default();

    let found = host
        .find_movies_by_numbers(std::slice::from_ref(&movie_number.to_owned()))
        .await?;
    let Some(movie) = found.into_iter().next() else {
        stats.imports.movie_not_found = 1;
        reporter.report(0, 0, format!("影片不存在: {movie_number}"));
        return Ok(stats);
    };
    // 用户可能敲的是 `ssni 888`，宿主里存的是 `SSNI-888`；以宿主的为准。
    let canonical = if movie.movie_number.is_empty() {
        movie_number.to_owned()
    } else {
        movie.movie_number.clone()
    };

    // 手动任务里抓取失败 = 整次任务失败（上游直接 raise）。
    let subtitles = client.fetch_chinese_subtitles(movie_number).await?;

    stats.source_matches = subtitles.len() as i64;
    reporter.report(
        0,
        subtitles.len() as i64,
        format!("找到 {} 份中文字幕", subtitles.len()),
    );
    stats.imports = import_subtitles(
        host,
        &canonical,
        &subtitles,
        reporter,
        None,
        subtitles.len() as i64,
        "处理字幕",
    )
    .await?;
    state.mark_fetched(&canonical, now)?;
    Ok(stats)
}

/// 上游 `run_subscribed`：定时抓所有已订阅影片。
pub async fn run_subscribed(
    host: &mut dyn SubtitleHost,
    client: &SubtitleCatClient,
    state: &FetchState,
    settings: &Settings,
    reporter: &mut dyn ProgressReporter,
    now: DateTime<Utc>,
) -> Result<SubscribedStats, JobError> {
    let mut stats = SubscribedStats::default();

    let snapshots = iter_subscribed_movies(host).await?;
    let total = snapshots.len() as i64;
    stats.subscribed = total;
    reporter.report(0, total, format!("发现 {total} 部已订阅影片"));
    if snapshots.is_empty() {
        return Ok(stats);
    }

    let now_date = now.date_naive();
    for (offset, movie) in snapshots.iter().enumerate() {
        let index = offset as i64 + 1;
        if movie.movie_number.is_empty() {
            stats.imports.failed += 1;
            reporter.report(index, total, "影片快照缺少番号".to_owned());
            continue;
        }

        // 「抓过 + 老片」才跳过：新片即使抓过也可能有新字幕。
        if state.has_fetched(&movie.movie_number)?
            && is_old_release(
                movie.release_date.as_deref(),
                now_date,
                settings.release_age_months,
            )
        {
            stats.skipped_old += 1;
            reporter.report(index, total, format!("跳过老片: {}", movie.movie_number));
            continue;
        }

        stats.eligible += 1;
        // 订阅任务里单部失败不打断整轮（上游 catch 住了）。
        let subtitles = match client.fetch_chinese_subtitles(&movie.movie_number).await {
            Ok(subtitles) => subtitles,
            Err(_) => {
                stats.imports.failed += 1;
                reporter.report(index, total, format!("抓取失败: {}", movie.movie_number));
                continue;
            }
        };

        stats.fetched += 1;
        stats.source_matches += subtitles.len() as i64;
        reporter.report(
            index,
            total,
            format!("{}: 找到 {} 份中文字幕", movie.movie_number, subtitles.len()),
        );
        let counts = import_subtitles(
            host,
            &movie.movie_number,
            &subtitles,
            reporter,
            Some(index),
            total,
            &format!("处理 {} 字幕", movie.movie_number),
        )
        .await?;
        stats.imports.add(counts);
        // 只有外部抓取和字幕导入流程都正常返回，才把本片记为已抓取。
        // 空结果也算一次成功抓取，避免老片无字幕时每天重复访问来源。
        state.mark_fetched(&movie.movie_number, now)?;
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Settings;
    use chrono::TimeZone;

    /// 记下所有进度文本的假 reporter。
    #[derive(Default)]
    struct RecordingReporter {
        lines: Vec<String>,
    }

    impl ProgressReporter for RecordingReporter {
        fn report(&mut self, current: i64, total: i64, text: String) {
            self.lines.push(format!("{current}/{total} {text}"));
        }
    }

    /// 只按脚本回话的假宿主；顺便记下被请求过什么。
    #[derive(Default)]
    struct FakeHost {
        /// `find_by_numbers` 的返回。
        found: Vec<HostMovie>,
        /// 分页：每页 `(movies, next_cursor)`，按调用顺序消费。
        pages: Vec<(Vec<HostMovie>, Option<i64>)>,
        /// 导入时按顺序回的状态（用完后回 `imported`）。
        import_statuses: Vec<ImportStatus>,
        /// 每次导入的 `(movie_number, file_name, language)`。
        imports: Vec<(String, String, String)>,
        list_calls: Vec<(i64, i32)>,
        find_calls: Vec<Vec<String>>,
    }

    #[async_trait]
    impl SubtitleHost for FakeHost {
        async fn find_movies_by_numbers(
            &mut self,
            movie_numbers: &[String],
        ) -> Result<Vec<HostMovie>, JobError> {
            self.find_calls.push(movie_numbers.to_vec());
            Ok(self.found.clone())
        }

        async fn list_movies(
            &mut self,
            after_id: i64,
            limit: i32,
        ) -> Result<(Vec<HostMovie>, Option<i64>), JobError> {
            self.list_calls.push((after_id, limit));
            if self.pages.is_empty() {
                return Ok((Vec::new(), None));
            }
            Ok(self.pages.remove(0))
        }

        async fn import_subtitle(
            &mut self,
            movie_number: &str,
            _content: &[u8],
            file_name: &str,
            language: &str,
        ) -> Result<ImportStatus, JobError> {
            self.imports.push((
                movie_number.to_owned(),
                file_name.to_owned(),
                language.to_owned(),
            ));
            if self.import_statuses.is_empty() {
                return Ok(ImportStatus::Imported);
            }
            Ok(self.import_statuses.remove(0))
        }
    }

    /// 一个不会被真的访问的客户端（本模块的用例不该出网）。
    ///
    /// `base_url` 指向一个必然连不上的本机端口：真被调到时也是**立刻**失败，
    /// 不会是「等 20 秒超时」。
    fn offline_client() -> SubtitleCatClient {
        let settings = Settings::from_json(&serde_json::json!({
            "base_url": "http://127.0.0.1:9/",
            "request_retries": 0,
        }));
        SubtitleCatClient::new(&settings).expect("客户端必能建")
    }

    fn movie(number: &str, subscribed: bool, release_date: Option<&str>) -> HostMovie {
        HostMovie {
            movie_number: number.to_owned(),
            is_subscribed: subscribed,
            release_date: release_date.map(str::to_owned),
        }
    }

    fn temp_state() -> (FetchState, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let state = FetchState::open(&dir.path().join("fetch_state.sqlite3")).unwrap();
        (state, dir)
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(seconds, 0).unwrap()
    }

    fn date(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    // ── 统计的键集 ──────────────────────────────────────────────

    #[test]
    fn manual_stats_carry_exactly_the_upstream_six_keys() {
        let stats = ManualStats::default();
        let struct_value = stats.to_struct();
        let mut keys: Vec<&str> = struct_value.fields.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected = MANUAL_STAT_KEYS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    #[test]
    fn subscribed_stats_carry_exactly_the_upstream_ten_keys() {
        let stats = SubscribedStats::default();
        let struct_value = stats.to_struct();
        let mut keys: Vec<&str> = struct_value.fields.keys().map(String::as_str).collect();
        keys.sort_unstable();
        let mut expected = SUBSCRIBED_STAT_KEYS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected);
    }

    #[test]
    fn zero_valued_counters_are_still_present() {
        // 上游 `_new_stats` 把每个键都初始化成 0，宿主的统计面板靠键存在与否
        // 判断「这次任务跑了没有」。
        let struct_value = ManualStats::default().to_struct();
        for key in MANUAL_STAT_KEYS {
            assert!(struct_value.fields.contains_key(key), "{key} 该在");
        }
    }

    // ── 导入状态 ────────────────────────────────────────────────

    #[test]
    fn known_import_statuses_are_recognized() {
        assert_eq!(ImportStatus::parse("imported"), ImportStatus::Imported);
        assert_eq!(ImportStatus::parse("duplicate"), ImportStatus::Duplicate);
        assert_eq!(
            ImportStatus::parse("movie_not_found"),
            ImportStatus::MovieNotFound
        );
        assert_eq!(
            ImportStatus::parse("invalid_format"),
            ImportStatus::InvalidFormat
        );
    }

    #[test]
    fn an_unknown_import_status_counts_as_failed() {
        let mut counts = ImportCounts::default();
        counts.record(ImportStatus::parse("teapot"));
        assert_eq!(counts.failed, 1);
        assert_eq!(counts.imported, 0);
    }

    #[test]
    fn record_maps_each_status_to_its_own_counter() {
        let mut counts = ImportCounts::default();
        for status in [
            ImportStatus::Imported,
            ImportStatus::Duplicate,
            ImportStatus::MovieNotFound,
            ImportStatus::InvalidFormat,
            ImportStatus::Other,
        ] {
            counts.record(status);
        }
        assert_eq!(
            counts,
            ImportCounts {
                imported: 1,
                duplicate: 1,
                movie_not_found: 1,
                invalid_format: 1,
                failed: 1,
            }
        );
    }

    // ── 老片判定 ────────────────────────────────────────────────

    #[test]
    fn calendar_month_subtraction_clamps_the_day() {
        // 3 月 31 日 - 1 个月：2 月没有 31 号，取 2 月最后一天。
        assert_eq!(subtract_calendar_months(date(2026, 3, 31), 1), Some(date(2026, 2, 28)));
        // 闰年。
        assert_eq!(subtract_calendar_months(date(2024, 3, 31), 1), Some(date(2024, 2, 29)));
        // 跨年。
        assert_eq!(subtract_calendar_months(date(2026, 1, 15), 1), Some(date(2025, 12, 15)));
        assert_eq!(subtract_calendar_months(date(2026, 1, 15), 13), Some(date(2024, 12, 15)));
    }

    #[test]
    fn the_cutoff_day_itself_counts_as_old() {
        // now = 2026-06-10，months = 3 → cutoff = 2026-03-10，`<=` 所以当天算老片。
        assert!(is_old_release(Some("2026-03-10"), date(2026, 6, 10), 3));
        assert!(!is_old_release(Some("2026-03-11"), date(2026, 6, 10), 3));
    }

    #[test]
    fn a_missing_or_unparsable_release_date_is_never_old() {
        // 没有发布时间时无法证明它是老片，保守地继续抓（上游原话）。
        assert!(!is_old_release(None, date(2026, 6, 10), 3));
        assert!(!is_old_release(Some(""), date(2026, 6, 10), 3));
        assert!(!is_old_release(Some("   "), date(2026, 6, 10), 3));
        assert!(!is_old_release(Some("去年"), date(2026, 6, 10), 3));
    }

    #[test]
    fn a_release_date_with_a_time_suffix_still_parses() {
        // 上游 `text[:10]`：`2026-01-02T03:04:05` 取日期部分。
        assert_eq!(parse_release_date(Some("2026-01-02T03:04:05")), Some(date(2026, 1, 2)));
        assert_eq!(parse_release_date(Some("2026-01-02")), Some(date(2026, 1, 2)));
    }

    // ── 订阅遍历 ────────────────────────────────────────────────

    #[tokio::test]
    async fn subscribed_listing_walks_pages_and_keeps_only_subscribed() {
        let mut host = FakeHost {
            pages: vec![
                (
                    vec![movie("A-1", true, None), movie("A-2", false, None)],
                    Some(2),
                ),
                (
                    vec![movie("A-3", true, None)],
                    // 最后一页：宿主说没有下一页了。
                    None,
                ),
            ],
            ..FakeHost::default()
        };
        let subscribed = iter_subscribed_movies(&mut host).await.unwrap();
        let numbers: Vec<&str> = subscribed
            .iter()
            .map(|m| m.movie_number.as_str())
            .collect();
        assert_eq!(numbers, vec!["A-1", "A-3"]);
        assert_eq!(
            host.list_calls,
            vec![(0, SUBSCRIBED_PAGE_LIMIT), (2, SUBSCRIBED_PAGE_LIMIT)]
        );
    }

    #[tokio::test]
    async fn a_cursor_that_does_not_advance_is_an_error() {
        let mut host = FakeHost {
            pages: vec![
                (vec![movie("A-1", true, None)], Some(7)),
                (vec![], Some(7)),
            ],
            ..FakeHost::default()
        };
        let err = iter_subscribed_movies(&mut host).await.unwrap_err();
        assert!(err.message.contains("游标"), "{err}");
    }

    // ── 手动任务 ────────────────────────────────────────────────

    #[tokio::test]
    async fn a_missing_movie_is_reported_without_touching_the_source() {
        let (state, _dir) = temp_state();
        let mut host = FakeHost::default();
        let mut reporter = RecordingReporter::default();
        let stats = run_fetch(
            &mut host,
            &offline_client(),
            &state,
            "SSNI-888",
            &mut reporter,
            at(1),
        )
        .await
        .unwrap();

        assert_eq!(stats.imports.movie_not_found, 1);
        assert_eq!(stats.source_matches, 0);
        assert!(host.imports.is_empty());
        // 不存在的影片不该被标记成「抓过」。
        assert!(!state.has_fetched("SSNI-888").unwrap());
        // 上游也会报一条进度再返回。
        assert!(reporter.lines[0].contains("影片不存在"), "{:?}", reporter.lines);
    }

    #[tokio::test]
    async fn the_manual_lookup_uses_the_normalized_number_the_caller_gave() {
        let (state, _dir) = temp_state();
        let mut host = FakeHost::default();
        let mut reporter = RecordingReporter::default();
        run_fetch(
            &mut host,
            &offline_client(),
            &state,
            "SSNI-888",
            &mut reporter,
            at(1),
        )
        .await
        .unwrap();
        assert_eq!(host.find_calls, vec![vec!["SSNI-888".to_owned()]]);
    }

    // ── 订阅任务 ────────────────────────────────────────────────

    #[tokio::test]
    async fn an_old_already_fetched_movie_is_skipped() {
        let (state, _dir) = temp_state();
        state.mark_fetched("OLD-1", at(1)).unwrap();
        let mut host = FakeHost {
            pages: vec![(
                vec![movie("OLD-1", true, Some("2020-01-01"))],
                None,
            )],
            ..FakeHost::default()
        };
        let mut reporter = RecordingReporter::default();
        let stats = run_subscribed(
            &mut host,
            &offline_client(),
            &state,
            &Settings::default(),
            &mut reporter,
            at(1_700_000_000),
        )
        .await
        .unwrap();

        assert_eq!(stats.subscribed, 1);
        assert_eq!(stats.skipped_old, 1);
        assert_eq!(stats.eligible, 0);
        assert_eq!(stats.fetched, 0);
        assert!(host.imports.is_empty());
    }

    #[tokio::test]
    async fn a_new_movie_is_fetched_even_if_it_was_seen_before() {
        let (state, _dir) = temp_state();
        state.mark_fetched("NEW-1", at(1)).unwrap();
        // 但这条路径会真的去抓（连不上）→ 计 `failed`，重点是它**没有**被跳过。
        let mut host = FakeHost {
            pages: vec![(
                vec![movie("NEW-1", true, Some("2026-10-01"))],
                None,
            )],
            ..FakeHost::default()
        };
        let mut reporter = RecordingReporter::default();
        let stats = run_subscribed(
            &mut host,
            &offline_client(),
            &state,
            &Settings::default(),
            &mut reporter,
            at(1_700_000_000),
        )
        .await
        .unwrap();

        assert_eq!(stats.skipped_old, 0);
        assert_eq!(stats.eligible, 1);
        assert_eq!(stats.fetched, 0);
        assert_eq!(stats.imports.failed, 1);
    }

    #[tokio::test]
    async fn a_snapshot_without_a_number_counts_as_failed() {
        let (state, _dir) = temp_state();
        let mut host = FakeHost {
            pages: vec![(vec![movie("", true, None)], None)],
            ..FakeHost::default()
        };
        let mut reporter = RecordingReporter::default();
        let stats = run_subscribed(
            &mut host,
            &offline_client(),
            &state,
            &Settings::default(),
            &mut reporter,
            at(1),
        )
        .await
        .unwrap();
        assert_eq!(stats.imports.failed, 1);
        assert_eq!(stats.eligible, 0);
    }

    #[tokio::test]
    async fn an_empty_subscription_list_returns_zeroed_stats() {
        let (state, _dir) = temp_state();
        let mut host = FakeHost::default();
        let mut reporter = RecordingReporter::default();
        let stats = run_subscribed(
            &mut host,
            &offline_client(),
            &state,
            &Settings::default(),
            &mut reporter,
            at(1),
        )
        .await
        .unwrap();
        assert_eq!(stats, SubscribedStats::default());
        assert_eq!(reporter.lines.len(), 1);
        assert!(reporter.lines[0].contains("发现 0 部"), "{:?}", reporter.lines);
    }
}
