//! 内建任务声明：`JobSpec` 与 cron 解析。
//!
//! 对应上游 `src/scheduler/contracts.py`（`JobDefinition`）与
//! `src/scheduler/registry.py`（`BUILTIN_JOB_REGISTRY`，**19** 项）。
//!
//! # `manual_only` 与 cron 互斥
//!
//! 上游 `JobDefinition._validate_cron_source` 强制：`manual_only=True` 时
//! **不许**声明 cron（`cron_setting` / `default_cron`），反之必须声明其一。
//! 那条不变式在这里由类型承担 —— [`JobSpec::cron`] 是 `Option`，而
//! [`builtin_jobs`] 里带 cron 的项都写死字面量。
//!
//! # cron 在**运行时时区**求值，不是 UTC
//!
//! 上游 `CronTrigger.from_crontab(expr, timezone=get_runtime_timezone())`，
//! 时区取自 `TZ` 环境变量（`src/common/runtime_time.py:16-23`），其次系统
//! 时区，最后兜底 `Asia/Shanghai`。所以「每天凌晨 2 点」是**本地** 2 点。
//!
//! 本模块照此实现（[`RuntimeTimezone`]），并且**不**引入 `chrono-tz`：
//!
//! | `TZ` | 上游 | 本模块 |
//! |---|---|---|
//! | `Asia/Shanghai` | `ZoneInfo("Asia/Shanghai")` | [`RuntimeTimezone::System`] —— Linux 上系统时区**就是**它 |
//! | `UTC` | `ZoneInfo("UTC")` | [`RuntimeTimezone::Utc`] |
//! | `+08:00` | `ZoneInfo("+08:00")` 抛异常 → 回退系统 | [`RuntimeTimezone::FixedUtcOffset`] |
//! | 未设置 | 系统时区 | [`RuntimeTimezone::System`] |
//!
//! 唯一做不到的是「本机时区数据库里没有、但名字有意义」的 IANA 时区 ——
//! 而上游没有 `tzdata` 时同样失败。所以这不是退化，是等价。
//!
//! DST 由系统时区自己处理（`chrono::Local` 走 `/etc/localtime`）；
//! [`RuntimeTimezone::FixedUtcOffset`] **不**跟随 DST 切换，文档里写明了。

use std::str::FromStr;

use chrono::{DateTime, FixedOffset, Local, Utc};

/// 定时任务的声明。
///
/// 只描述「什么时候触发、叫什么」，**不含**执行逻辑 —— 执行在 worker 侧
/// （本仓库还没有 worker，见 crate 文档）。
///
/// 字段与上游 `JobDefinition` 同名同义（它还有 `handler` / `lane` /
/// `business_recovery` 等执行期字段，这里不做）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobSpec {
    /// 处理器定位键，也是 `mutex_key` 的来源（`aps:` + 本值）。
    pub task_key: String,
    /// 日志名。上游 `log_name`，任务中心与日志里都用它。
    pub log_name: String,
    /// CLI 子命令名。上游 `cli_name`。
    pub cli_name: String,
    /// 展示名。上游是 `TASK_NAME_REGISTRY.get(task_key) or cli_help`，
    /// 这里直接落 `cli_help` 那一路的值。
    pub display_name: String,
    /// 5 段 cron 表达式。`None` 表示 `manual_only`（只能手动触发）。
    ///
    /// 持有 `String` 而不是 `&'static str`：内建任务的键是编译期常量，但
    /// **插件任务的键来自注册响应**（`JobDefinition.default_cron`），只有
    /// 运行期才知道。上游同理 —— `JOB_REGISTRY` 在 import 阶段才成型。
    pub cron: Option<String>,
    /// 是否允许经 HTTP 手动触发。上游 `manual_trigger_allowed`，默认 `True`；
    /// `_validate_cron_source` 还强制 `manual_only` 的任务必须允许 ——
    /// 否则它既没有 cron 又不能手动触发，等于永远不会跑。
    pub manual_trigger_allowed: bool,
}

impl JobSpec {
    /// 是否只能手动触发。
    pub fn is_manual_only(&self) -> bool {
        self.cron.is_none()
    }
}

/// 声明的 cron 无法解析。
///
/// 在**注册期**返回而不是留到 tick 里 panic：一份写错的 cron 不该让服务
/// 带着一个每分钟 panic 的后台任务跑起来。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScheduleError {
    pub task_key: String,
    pub cron: String,
    pub reason: String,
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "task_key={} 的 cron {:?} 无法解析：{}",
            self.task_key, self.cron, self.reason
        )
    }
}

impl std::error::Error for ScheduleError {}

/// 一个已解析、可求值的任务。
#[derive(Debug, Clone)]
pub struct ScheduledJob {
    spec: JobSpec,
    schedule: cron::Schedule,
}

impl ScheduledJob {
    /// 解析并编译。`spec.cron` 为 `None` 时返回 `Ok(None)`。
    pub fn compile(spec: JobSpec) -> Result<Option<Self>, ScheduleError> {
        // 借而不取：解析失败时要带着 `task_key` 与**原始**表达式报错，
        // 而成功后还要把整个 `spec` 搬进 `Self`。
        let Some(expr) = spec.cron.as_ref() else {
            return Ok(None);
        };
        let normalized = to_cron_crate_expr(expr);
        let schedule = cron::Schedule::from_str(&normalized).map_err(|err| ScheduleError {
            task_key: spec.task_key.clone(),
            cron: expr.clone(),
            reason: err.to_string(),
        })?;
        Ok(Some(Self { spec, schedule }))
    }

    pub fn spec(&self) -> &JobSpec {
        &self.spec
    }

    /// 下一次触发时刻（UTC）。`after` 之后没有任何匹配（如 `0 0 30 2 *`）→ `None`。
    ///
    /// 传入的 `now` 会被转成 `tz` 再求值，所以「每月 30 日」这类表达式
    /// 是按**本地**日历判断的 —— 2 月没有 30 日，返回 `None` 而不是死循环。
    pub fn next_fire_after(
        &self,
        now: DateTime<Utc>,
        tz: &RuntimeTimezone,
    ) -> Option<DateTime<Utc>> {
        tz.next_after(&self.schedule, now)
    }
}

/// cron 求值时区。见模块文档的对照表。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeTimezone {
    /// 跟随系统时区（`TZ` / `/etc/localtime`）。**默认**。
    System,
    /// UTC。
    Utc,
    /// 固定偏移，**不**跟随 DST。
    FixedUtcOffset(i32),
}

impl Default for RuntimeTimezone {
    fn default() -> Self {
        Self::from_env()
    }
}

impl RuntimeTimezone {
    /// 按上游顺序解析：显式 `UTC` / 固定偏移 → `System`。
    ///
    /// 刻意**不**在解析失败时 panic：`TZ` 是运维随手写的字符串，
    /// 写错时退回系统时区远好过让后端起不来。上游在 `ZoneInfo` 找不到时
    /// 也是回退。
    pub fn from_env() -> Self {
        let raw = std::env::var("TZ").unwrap_or_default();
        let raw = raw.trim();
        if raw.is_empty() {
            return Self::System;
        }
        if raw.eq_ignore_ascii_case("utc") || raw.eq_ignore_ascii_case("etc/utc") {
            return Self::Utc;
        }
        Self::parse_fixed_offset(raw).map_or(Self::System, Self::FixedUtcOffset)
    }

    /// 解析 `+08:00` / `-05:30` / `UTC+8` 这类固定偏移。
    ///
    /// 不认 IANA 名（`Asia/Shanghai`）—— 那种情况交给 [`Self::System`]，
    /// 因为 Linux 上系统时区数据库已经包含它。
    fn parse_fixed_offset(raw: &str) -> Option<i32> {
        let body = raw
            .strip_prefix("UTC")
            .or_else(|| raw.strip_prefix("utc"))
            .unwrap_or(raw);
        let (sign, rest) = match body.as_bytes().first()? {
            b'+' => (1, &body[1..]),
            b'-' => (-1, &body[1..]),
            _ => return None,
        };
        let (hours, minutes) = match rest.split_once(':') {
            Some((h, m)) => (h.parse::<i32>().ok()?, m.parse::<i32>().ok()?),
            // `+8` 这种写法 cron/上游都不接受，但容器里常见；宽松接受。
            None if rest.len() <= 2 => (rest.parse::<i32>().ok()?, 0),
            None => return None,
        };
        if !(0..=14).contains(&hours) || !(0..60).contains(&minutes) {
            return None;
        }
        let seconds = sign * (hours * 3600 + minutes * 60);
        // 超出 ±18:00 的偏移在现实中不存在，且 FixedOffset 会拒。
        (-64800..=64800).contains(&seconds).then_some(seconds)
    }

    /// 求下一次触发。
    ///
    /// 三个分支的返回类型不同（`DateTime<Utc>` / `DateTime<Local>` /
    /// `DateTime<FixedOffset>`），所以每支各自转成 UTC 后返回。
    fn next_after(&self, schedule: &cron::Schedule, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Self::Utc => schedule.after(&now).next(),
            Self::System => schedule
                .after(&now.with_timezone(&Local))
                .next()
                .map(|fire| fire.with_timezone(&Utc)),
            Self::FixedUtcOffset(seconds) => {
                let tz = FixedOffset::east_opt(*seconds)?;
                schedule
                    .after(&now.with_timezone(&tz))
                    .next()
                    .map(|fire| fire.with_timezone(&Utc))
            }
        }
    }

    /// 日志/诊断用的名字。
    pub fn display_name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::Utc => "UTC",
            Self::FixedUtcOffset(_) => "fixed-offset",
        }
    }
}

/// 内建任务注册表：**19** 项，其中 16 项带 cron、3 项 `manual_only`。
///
/// cron 表达式取自上游 `src/config/config.py` 的 `Scheduler` 默认值
/// （逐条抄写，那是「不在配置文件里覆盖时」的实际生效值）。
///
/// # 为什么表里只有声明、没有 handler
///
/// 上游 `JobDefinition.handler` 指向各域的 service，而这些 service 分布在
/// `catalog` / `playback` / `transfers` / `discovery` / `system` 五个域里
/// （共 114 个文件），本仓库目前只落地了 `system` 与 `collections`。
/// 所以本模块只负责**到点入队**；领取与执行留给 worker 侧那个切片。
///
/// 因此这里刻意**不**放 `TaskHandler` trait 的实现清单 —— 那会是一份
/// 指向不存在代码的清单。
pub fn builtin_jobs() -> Vec<JobSpec> {
    vec![
        // ---- catalog ----------------------------------------------------
        job(
            "movie_javdb_backfill",
            "movie-javdb-backfill",
            "backfill-movie-javdb",
            "尝试从 JavDB 补录插件影片",
            Some("30 5 * * *"),
        ),
        job(
            "actor_subscription_sync",
            "actor-subscription-sync",
            "sync-subscribed-actor-movies",
            "执行一次订阅女优影片抓取",
            Some("0 2 * * *"),
        ),
        job(
            "subscribed_movie_auto_download",
            "subscribed-movie-auto-download",
            "auto-download-subscribed-movies",
            "执行一次已订阅缺失影片自动下载",
            Some("30 2 * * *"),
        ),
        job(
            "movie_heat_update",
            "movie-heat-update",
            "update-movie-heat",
            "执行一次影片热度重算",
            Some("15 0 * * *"),
        ),
        job(
            "movie_interaction_sync",
            "movie-interaction-sync",
            "sync-movie-interactions",
            "执行一次影片互动数同步",
            Some("0 5 * * *"),
        ),
        job(
            "movie_similarity_recompute",
            "movie-similarity-recompute",
            "recompute-movie-similarities",
            "执行一次影片相似度全量重算",
            Some("30 3 * * *"),
        ),
        job(
            "movie_asset_pack_backfill",
            "movie-asset-pack-backfill",
            "backfill-movie-asset-packs",
            "影片图片打包回填（封面/薄封面/剧情图 → assets.zip）",
            None,
        ),
        // ---- playback ----------------------------------------------------
        job(
            "media_file_hash_backfill",
            "media-file-hash-backfill",
            "backfill-media-file-hashes",
            "执行一次空媒体文件哈希补算",
            Some("0 3 * * *"),
        ),
        job(
            "media_video_info_backfill",
            "media-video-info-backfill",
            "backfill-media-video-info",
            "媒体信息回填",
            None,
        ),
        // 上游注释：115 用整库远端清单对账；每天一次且 provider 内部限速。
        job(
            "media_file_scan",
            "media-file-scan",
            "scan-media-files",
            "执行一次媒体文件巡检",
            Some("0 4 * * *"),
        ),
        // 上游注释：空跑只查 DB 不读盘，30 分钟一次足够。
        job(
            "media_thumbnail_generation",
            "media-thumbnail-generation",
            "generate-media-thumbnails",
            "执行一次媒体缩略图生成",
            Some("*/30 * * * *"),
        ),
        job(
            "media_thumbnail_pack_backfill",
            "media-thumbnail-pack-backfill",
            "backfill-media-thumbnail-packs",
            "媒体缩略图打包回填（存量单文件 → thumbnails.zip）",
            None,
        ),
        // ---- transfers ---------------------------------------------------
        job(
            "download_task_sync",
            "download-task-sync",
            "sync-download-tasks",
            "执行一次下载任务状态同步",
            Some("* * * * *"),
        ),
        job(
            "download_task_auto_import",
            "download-task-auto-import",
            "auto-import-download-tasks",
            "执行一次已完成下载自动导入",
            Some("* * * * *"),
        ),
        // ---- discovery ---------------------------------------------------
        job(
            "image_search_index",
            "image-search-index",
            "index-image-search",
            "持续构建缩略图和剧情图的搜索向量索引，直到待处理队列为空",
            Some("*/5 * * * *"),
        ),
        job(
            "moment_recommendation_generate",
            "moment-recommendation-generate",
            "generate-moment-recommendations",
            "执行一次推荐时刻生成",
            Some("0 4 * * *"),
        ),
        job(
            "daily_recommendation_generate",
            "daily-recommendation-generate",
            "generate-daily-recommendations",
            "执行一次每日推荐快照生成",
            Some("0 5 * * *"),
        ),
        // ---- system ------------------------------------------------------
        // 上游注释：GFriends Filetree 缓存刷新，默认每周一 04:00，对齐 disk
        // cache 的 7 天 TTL。
        job(
            "gfriends_filetree_refresh",
            "gfriends-filetree-refresh",
            "refresh-gfriends-filetree",
            "拉取一次 GFriends Filetree 并写入本地缓存",
            Some("0 4 * * 1"),
        ),
        job(
            "activity_record_cleanup",
            "activity-record-cleanup",
            "cleanup-activity-records",
            "执行一次活动中心记录清理（任务运行 / 已读通知）",
            Some("30 5 * * *"),
        ),
    ]
}

#[allow(clippy::too_many_arguments)]
fn job(
    task_key: &'static str,
    log_name: &'static str,
    cli_name: &'static str,
    display_name: &'static str,
    cron: Option<&'static str>,
) -> JobSpec {
    JobSpec {
        task_key: task_key.to_owned(),
        log_name: log_name.to_owned(),
        cli_name: cli_name.to_owned(),
        display_name: display_name.to_owned(),
        cron: cron.map(str::to_owned),
        // 上游默认 `True`，且 `manual_only` 必须为 `True` —— 三项内建
        // manual_only 任务也只能手动触发，没有第二条路。
        manual_trigger_allowed: true,
    }
}

// cron 方言转换住在 `sm_core::crontab` —— 配置校验（`config_schema` 验
// `scheduler.*_cron` 字段）与本模块的运行期求值必须用**同一套**规则，否则
// 会出现「配置校验通过但调度时解析失败」或「星期编号没映射而错一天」。
use sm_core::crontab::to_cron_crate_expr;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Datelike, TimeZone};

    fn at(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text)
            .expect("时间戳")
            .with_timezone(&Utc)
    }

    fn spec(task_key: &str, cron: Option<&str>) -> JobSpec {
        JobSpec {
            task_key: task_key.to_owned(),
            log_name: task_key.to_owned(),
            cli_name: task_key.to_owned(),
            display_name: task_key.to_owned(),
            cron: cron.map(str::to_owned),
            manual_trigger_allowed: true,
        }
    }

    #[test]
    fn the_registry_has_nineteen_jobs_sixteen_with_cron() {
        let jobs = builtin_jobs();
        assert_eq!(jobs.len(), 19, "上游 BUILTIN_JOB_REGISTRY 是 19 项");
        let with_cron = jobs.iter().filter(|j| !j.is_manual_only()).count();
        assert_eq!(with_cron, 16, "其中 16 项带 cron");
        // 三个 manual_only 的 task_key 逐条对上 —— 它们在 HTTP 侧是
        // 「可手动触发但无 cron」，少了 cron 就变成永不入队。
        let manual: Vec<String> = jobs
            .iter()
            .filter(|j| j.is_manual_only())
            .map(|j| j.task_key.clone())
            .collect();
        assert_eq!(
            manual,
            vec![
                "movie_asset_pack_backfill",
                "media_video_info_backfill",
                "media_thumbnail_pack_backfill",
            ]
        );
    }

    #[test]
    fn task_keys_are_unique() {
        // 唯一性是**互斥键**的前提：两个任务共用 `aps:<task_key>` 会让它们
        // 互相顶掉，而症状是「一个任务永远不跑」，极难定位。
        assert_unique(
            builtin_jobs().iter().map(|j| j.task_key.clone()),
            "task_key",
        );
    }

    #[test]
    fn log_names_and_cli_names_are_unique_too() {
        // 上游 `_build_job_registry` 校验的是**三个**字段（task_key / cli_name /
        // log_name），且插件与内建冲突时隔离插件。CLI 与日志靠后两个定位任务，
        // 重复会让「跑的是哪个任务」变成猜谜。
        assert_unique(
            builtin_jobs().iter().map(|j| j.log_name.clone()),
            "log_name",
        );
        assert_unique(
            builtin_jobs().iter().map(|j| j.cli_name.clone()),
            "cli_name",
        );
    }

    fn assert_unique(values: impl Iterator<Item = String>, field: &str) {
        let mut values: Vec<String> = values.collect();
        let before = values.len();
        values.sort_unstable();
        values.dedup();
        assert_eq!(values.len(), before, "{field} 重复");
    }

    #[test]
    fn every_job_carries_its_three_names() {
        // 任务中心（`GET /system/jobs`）要原样吐这三个字段，缺一个前端就显示空。
        for spec in builtin_jobs() {
            assert!(!spec.log_name.is_empty(), "{} 缺 log_name", spec.task_key);
            assert!(!spec.cli_name.is_empty(), "{} 缺 cli_name", spec.task_key);
            assert!(!spec.display_name.is_empty(), "{} 缺展示名", spec.task_key);
        }
    }

    #[test]
    fn a_manual_only_job_is_still_allowed_to_be_triggered_manually() {
        // 上游 `_validate_cron_source`：`manual_only` 的任务**必须**允许手动触发
        // —— 否则它既没有 cron 又不能手动触发，等于永远不会跑。
        for spec in builtin_jobs() {
            if spec.is_manual_only() {
                assert!(
                    spec.manual_trigger_allowed,
                    "{} 是 manual_only 却不允许手动触发",
                    spec.task_key
                );
            }
        }
    }

    #[test]
    fn every_cron_expression_parses() {
        // 写错的表达式在**注册期**就该失败，而不是每分钟 panic 一次。
        for spec in builtin_jobs() {
            assert!(
                ScheduledJob::compile(spec.clone()).is_ok(),
                "{} 的 cron 无法解析",
                spec.task_key
            );
        }
    }

    #[test]
    fn an_invalid_cron_is_rejected_at_compile_time() {
        let spec = spec("bad", Some("not a cron"));
        let err = ScheduledJob::compile(spec).expect_err("非法表达式应在编译期失败");
        assert_eq!(err.task_key, "bad");
        // 错误信息要同时带上 task_key 与**原始**表达式（不是转换后的 6 段
        // 形式）—— 运维看到的是自己在配置文件里写的那一串。
        let text = err.to_string();
        assert!(text.contains("bad"), "{text}");
        assert!(text.contains("not a cron"), "{text}");
    }

    #[test]
    fn the_weekly_job_fires_on_monday_not_sunday() {
        // 这条断言是本模块存在的原因：`0 4 * * 1` 若不映射星期编号，
        // 会变成每周**日** 04:00 —— 不报错，只是错一天。
        let spec = builtin_jobs()
            .into_iter()
            .find(|j| j.task_key == "gfriends_filetree_refresh")
            .unwrap();
        let job = ScheduledJob::compile(spec).unwrap().unwrap();
        // 2026-10-04 是周日，所以「下周一 04:00」应当是 10-05。
        let fire = job
            .next_fire_after(at("2026-10-04T10:00:00Z"), &RuntimeTimezone::Utc)
            .unwrap();
        assert_eq!(fire, at("2026-10-05T04:00:00Z"));
        assert_eq!(fire.weekday(), chrono::Weekday::Mon);
    }

    #[test]
    fn a_sunday_expression_lands_on_sunday() {
        // crontab 的 `0` 也是周日，映射后不能跑到周六去。
        let spec = spec("weekly_sunday", Some("0 5 * * 0"));
        let job = ScheduledJob::compile(spec).unwrap().unwrap();
        let fire = job
            .next_fire_after(at("2026-10-04T10:00:00Z"), &RuntimeTimezone::Utc)
            .unwrap();
        assert_eq!(fire.weekday(), chrono::Weekday::Sun);
        assert_eq!(fire, at("2026-10-11T05:00:00Z"));
    }

    #[test]
    fn a_manual_only_job_has_no_schedule() {
        let spec = builtin_jobs()
            .into_iter()
            .find(|j| j.is_manual_only())
            .expect("至少一个 manual_only");
        assert!(ScheduledJob::compile(spec).expect("不该报错").is_none());
    }

    #[test]
    fn a_daily_job_fires_at_its_local_hour() {
        // `movie_heat_cron = "15 0 * * *"` → 本地 00:15。
        let spec = builtin_jobs()
            .into_iter()
            .find(|j| j.task_key == "movie_heat_update")
            .unwrap();
        let job = ScheduledJob::compile(spec).unwrap().unwrap();

        let tz = RuntimeTimezone::FixedUtcOffset(8 * 3600);
        // UTC 2026-10-04T00:00 → 本地 08:00，所以下一次是本地 00:15
        // = UTC 前一天 16:15 之后的那一个，即 UTC 2026-10-04T16:15。
        let fire = job
            .next_fire_after(at("2026-10-04T00:00:00Z"), &tz)
            .expect("应当有下一次");
        assert_eq!(fire, at("2026-10-04T16:15:00Z"));
    }

    #[test]
    fn the_same_expression_means_different_instants_in_different_zones() {
        let spec = builtin_jobs()
            .into_iter()
            .find(|j| j.task_key == "movie_heat_update")
            .unwrap();
        let job = ScheduledJob::compile(spec).unwrap().unwrap();
        // 00:00 UTC = 本地 08:00（UTC+8），所以 UTC 下一次是今天 00:15，
        // 东八区下一次是**明天**本地 00:15 = 今天 16:15 UTC。
        let now = at("2026-10-04T00:00:00Z");

        let utc = job.next_fire_after(now, &RuntimeTimezone::Utc).unwrap();
        let shanghai = job
            .next_fire_after(now, &RuntimeTimezone::FixedUtcOffset(8 * 3600))
            .unwrap();
        assert_eq!(utc, at("2026-10-04T00:15:00Z"));
        assert_eq!(shanghai, at("2026-10-04T16:15:00Z"));
        // 不变量不是「相差 8 小时」（那取决于 now 落在哪一侧），而是
        // **本地墙钟时刻相同**：两边都是各自时区的 00:15。
        assert_eq!(utc.with_timezone(&Utc).time().to_string(), "00:15:00");
        assert_eq!(
            FixedOffset::east_opt(8 * 3600)
                .unwrap()
                .from_utc_datetime(&utc.naive_utc())
                .time()
                .to_string(),
            "08:15:00",
            "UTC 的 00:15 在东八区是 08:15"
        );
    }

    #[test]
    fn step_expressions_fire_within_their_step() {
        // `image_search_index_cron = "*/5 * * * *"`
        let spec = builtin_jobs()
            .into_iter()
            .find(|j| j.task_key == "image_search_index")
            .unwrap();
        let job = ScheduledJob::compile(spec).unwrap().unwrap();
        let tz = RuntimeTimezone::Utc;
        let now = at("2026-10-04T10:02:30Z");
        let fire = job.next_fire_after(now, &tz).unwrap();
        assert_eq!(fire, at("2026-10-04T10:05:00Z"));
    }

    #[test]
    fn a_minutely_job_fires_one_minute_later() {
        // `download_task_sync_cron = "* * * * *"` —— 上游两个下载任务都是
        // 每分钟；tick 粒度必须够细，否则会系统性迟到。
        let spec = builtin_jobs()
            .into_iter()
            .find(|j| j.task_key == "download_task_sync")
            .unwrap();
        let job = ScheduledJob::compile(spec).unwrap().unwrap();
        let now = at("2026-10-04T10:02:30Z");
        assert_eq!(
            job.next_fire_after(now, &RuntimeTimezone::Utc).unwrap(),
            at("2026-10-04T10:03:00Z")
        );
    }

    #[test]
    fn a_weekly_job_lands_on_its_weekday() {
        // `gfriends_filetree_refresh_cron = "0 4 * * 1"` → 本地周一 04:00。
        let spec = builtin_jobs()
            .into_iter()
            .find(|j| j.task_key == "gfriends_filetree_refresh")
            .unwrap();
        let job = ScheduledJob::compile(spec).unwrap().unwrap();
        let tz = RuntimeTimezone::Utc;
        // 2026-10-04 是周日。
        let fire = job
            .next_fire_after(at("2026-10-04T10:00:00Z"), &tz)
            .unwrap();
        assert_eq!(fire, at("2026-10-05T04:00:00Z"));
        assert_eq!(fire.weekday(), chrono::Weekday::Mon);
    }

    #[test]
    fn an_impossible_date_yields_none_instead_of_looping() {
        // 「2 月 30 日」永不发生。必须返回 None，不能死循环。
        let spec = spec("impossible", Some("0 0 30 2 *"));
        let job = ScheduledJob::compile(spec).unwrap().unwrap();
        assert_eq!(
            job.next_fire_after(at("2026-01-01T00:00:00Z"), &RuntimeTimezone::Utc),
            None
        );
    }

    #[test]
    fn fixed_offsets_parse_in_the_shapes_operators_write() {
        assert_eq!(
            RuntimeTimezone::parse_fixed_offset("+08:00"),
            Some(8 * 3600)
        );
        assert_eq!(
            RuntimeTimezone::parse_fixed_offset("-05:30"),
            Some(-(5 * 3600 + 30 * 60))
        );
        assert_eq!(RuntimeTimezone::parse_fixed_offset("UTC+8"), Some(8 * 3600));
        // IANA 名不当作固定偏移 —— 交给系统时区。
        assert_eq!(RuntimeTimezone::parse_fixed_offset("Asia/Shanghai"), None);
        // 越界与畸形
        assert_eq!(RuntimeTimezone::parse_fixed_offset("+99:00"), None);
        assert_eq!(RuntimeTimezone::parse_fixed_offset("+08:99"), None);
        assert_eq!(RuntimeTimezone::parse_fixed_offset("garbage"), None);
    }

    /// 内建任务键必须在 service 侧的显示名注册表里。
    ///
    /// **这条断言只能放在这个 crate** —— 依赖方向是
    /// `sm-scheduler → sm-service`，写在 `sm-service` 侧会成环。
    ///
    /// # 两条已知的例外是**上游缺陷**，刻意照抄
    ///
    /// `movie_asset_pack_backfill` 与 `media_thumbnail_pack_backfill` 在上游
    /// `BUILTIN_JOB_REGISTRY`（`registry.py:176-185`）里有，
    /// `TASK_NAME_REGISTRY`（`task_catalog.py`）里**没有**。于是上游
    /// `resolve_task_name` 对这两个回落到 `task_key`，任务中心里显示英文。
    ///
    /// 照抄理由与 `docs/handoff.md` 第五节那两条一致：修它会改变客户端
    /// 已渲染的文字，属于契约变更，该单独开一个 fix 并同步上游。
    /// **新增**内建任务若缺显示名，那不是照抄而是真缺陷 —— 断言会拦。
    const UPSTREAM_REGISTRY_GAPS: [&str; 2] =
        ["movie_asset_pack_backfill", "media_thumbnail_pack_backfill"];

    #[test]
    fn every_builtin_task_key_has_a_display_name_in_the_service_registry() {
        let jobs = builtin_jobs();
        let missing: Vec<&str> = jobs
            .iter()
            .map(|spec| spec.task_key.as_str())
            .filter(|key| sm_service::system::activity::lookup_task_name(key).is_none())
            .filter(|key| !UPSTREAM_REGISTRY_GAPS.contains(key))
            .collect();
        assert!(
            missing.is_empty(),
            "这些内建任务键在 TASK_NAME_REGISTRY 里没有显示名：{missing:?}"
        );
    }

    /// 记录在上游那两个缺口**之外**没有新增缺口。
    ///
    /// 与上一条分开是为了让「上游缺了」和「我们多漏了」在报告里可区分：
    /// 两条都失败时，先看这一条更省事。
    #[test]
    fn the_upstream_registry_gaps_have_not_grown() {
        for key in UPSTREAM_REGISTRY_GAPS {
            assert!(
                sm_service::system::activity::lookup_task_name(key).is_none(),
                "{key} 已有了显示名 —— 把它从 UPSTREAM_REGISTRY_GAPS 里去掉，\
                 并考虑这条记录是否还成立"
            );
        }
    }

    /// 没有 cron 的任务**必须**允许手动触发。
    ///
    /// 上游 `JobDefinition._validate_cron_source`
    /// （`scheduler/contracts.py:47-49`）强制这条：`manual_only` 任务若不允许
    /// 手动触发就等于永远不会跑。这里没有 import 阶段的校验器，所以靠断言
    /// 兜住 —— 症状是任务既不自动跑、也不出现在手动列表里，**不报错**。
    #[test]
    fn a_task_without_a_cron_must_still_be_manually_triggerable() {
        for spec in &builtin_jobs() {
            if spec.cron.is_none() {
                assert!(
                    spec.manual_trigger_allowed,
                    "{} 没有 cron 就必须允许手动触发，否则它永远不会跑",
                    spec.task_key
                );
            }
        }
    }
}
