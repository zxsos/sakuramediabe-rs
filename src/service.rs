//! gRPC 服务面：`PluginControl`（注册 + 后台任务）。
//!
//! # 上游对应
//!
//! `plugin.py:register` 声明一个 `JobDefinition`（定时任务），
//! `plugin.py:judge_movies` 是任务主循环。这里：
//! - `register` → [`Control::register`]，回 `JobDefinition`；
//! - `judge_movies` → [`Control::run_job`]，分页扫描 + 判定 + patch，
//!   每页推一个 `ProgressEvent`，最后推 `result` 终态。
//!
//! # 注册阶段不许联网
//!
//! proto 写在 `Register` 上的原话：「注册阶段只应构造声明与校验本地配置：
//! 不要联网、不要创建外部目录、不要启动后台线程。」所以 `register` 只回
//! 声明；宿主连接是 `run_job` 被调用时才建的。

use std::sync::Arc;

use futures::stream::BoxStream;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::{
    JobDefinition, JobEvent, ProgressEvent, RegisterRequest, RegisterResponse, RunJobRequest,
};
use tokio::sync::mpsc;
use tonic::{Request, Response, Status};

use crate::host::{HostMovies, PAGE_SIZE};
use crate::judge::{Decision, Stats, decide};
use crate::settings::DurationCollectionSettings;

/// 上游 `JobDefinition` 的各字段。
pub const TASK_KEY: &str = "sakuramedia_judge_collecttion_movie";
pub const LOG_NAME: &str = "judge-collection-by-duration";
pub const CLI_NAME: &str = "judge-collection-by-duration";
pub const CLI_HELP: &str = "按影片时长、番号特征或标签判定合集影片";
/// 上游 `default_cron="0 4 * * *"`（每天 04:00）。
pub const DEFAULT_CRON: &str = "0 4 * * *";

/// 宿主回调用的地址（`PluginHost` 服务）。宿主目前**不注入**这个变量
/// （生命周期协议只给了 ADDR / ID / DATA_DIR / SETTINGS_FILE），所以缺省
/// 时 `run_job` 直接回 `unimplemented` —— 等宿主侧把回调用地址接进来再摘
/// 掉这层。
pub const HOST_ADDR_ENV: &str = "SAKURAMEDIA_PLUGIN_HOST_ADDR";

/// 控制面。
pub struct Control {
    plugin_id: String,
    settings: DurationCollectionSettings,
    host: Option<Arc<dyn HostMovies>>,
}

impl Control {
    pub fn new(
        plugin_id: String,
        settings: DurationCollectionSettings,
        host: Option<Arc<dyn HostMovies>>,
    ) -> Self {
        Self { plugin_id, settings, host }
    }

    pub fn job_definition() -> JobDefinition {
        JobDefinition {
            task_key: TASK_KEY.to_owned(),
            log_name: LOG_NAME.to_owned(),
            cli_name: CLI_NAME.to_owned(),
            cli_help: CLI_HELP.to_owned(),
            default_cron: DEFAULT_CRON.to_owned(),
            manual_only: false,
            params_schema: None,
            required_capabilities: Vec::new(),
        }
    }
}

fn progress_event(stats: &Stats) -> JobEvent {
    JobEvent {
        event: Some(sm_plugin_api::v1::job_event::Event::Progress(ProgressEvent {
            text: stats.progress_text(),
            current: stats.scanned as i32,
            total: 0,
        })),
    }
}

fn result_event(stats: &Stats) -> JobEvent {
    let mut fields = std::collections::BTreeMap::new();
    for (k, v) in [
        ("scanned", stats.scanned),
        ("updated", stats.updated),
        ("unchanged", stats.unchanged),
        ("skipped_owned", stats.skipped_owned),
        ("patch_failed", stats.patch_failed),
    ] {
        fields.insert(
            k.to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::NumberValue(v as f64)),
            },
        );
    }
    JobEvent {
        event: Some(sm_plugin_api::v1::job_event::Event::Result(
            prost_types::Struct { fields },
        )),
    }
}

/// 扫描主循环：与上游 `judge_movies` 的分支顺序一致。
pub async fn run_scan(
    host: &dyn HostMovies,
    config: &DurationCollectionSettings,
    plugin_id: &str,
    mut on_progress: impl FnMut(&Stats),
) -> Result<Stats, Box<dyn std::error::Error + Send + Sync>> {
    let mut stats = Stats::default();
    let mut after_id: i64 = 0;

    loop {
        let page = host.list_page(after_id, PAGE_SIZE).await?;
        if page.movies.is_empty() {
            break;
        }
        for movie in &page.movies {
            stats.scanned += 1;
            match decide(movie, config, plugin_id) {
                Decision::Unchanged | Decision::AlreadyCollection => {
                    stats.unchanged += 1;
                }
                Decision::SkippedOwned => {
                    stats.skipped_owned += 1;
                }
                Decision::Mark => {
                    let ok = host
                        .patch_is_collection(movie.movie_id, movie.revision)
                        .await?;
                    if ok {
                        stats.updated += 1;
                    } else {
                        stats.patch_failed += 1;
                    }
                }
            }
        }
        on_progress(&stats);
        match page.next_cursor {
            Some(cursor) => after_id = cursor,
            None => break,
        }
    }
    Ok(stats)
}

#[tonic::async_trait]
impl PluginControl for Control {
    type RunJobStream = BoxStream<'static, Result<JobEvent, Status>>;

    async fn run_job(
        &self,
        request: Request<RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        let inner = request.into_inner();
        if inner.task_key != TASK_KEY {
            return Err(Status::invalid_argument(format!(
                "未知任务：{}（本插件只提供 {TASK_KEY}）",
                inner.task_key
            )));
        }
        let host = match &self.host {
            Some(h) => h.clone(),
            None => {
                return Err(Status::unimplemented(
                    "宿主未注入回调用地址（SAKURAMEDIA_PLUGIN_HOST_ADDR），\
                     插件无法访问影片库；等宿主侧接好 PluginHost 回调后再启用",
                ));
            }
        };
        let config = self.settings.clone();
        let plugin_id = self.plugin_id.clone();
        let (tx, rx) = mpsc::channel::<Result<JobEvent, Status>>(16);

        tokio::spawn(async move {
            let on_progress = |stats: &Stats| {
                let _ = tx.try_send(Ok(progress_event(stats)));
            };
            match run_scan(host.as_ref(), &config, &plugin_id, on_progress).await {
                Ok(stats) => {
                    let _ = tx.send(Ok(result_event(&stats))).await;
                }
                Err(e) => {
                    let _ = tx
                        .send(Err(Status::internal(format!("判定任务失败：{e}"))))
                        .await;
                }
            }
        });

        let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(stream)))
    }

    async fn register(
        &self,
        request: Request<RegisterRequest>,
    ) -> Result<Response<RegisterResponse>, Status> {
        let injected = request.into_inner().plugin_id;
        if injected != self.plugin_id {
            return Err(Status::invalid_argument(format!(
                "宿主注入的 plugin_id 与启动参数不一致：注入 {injected}，本进程 {}",
                self.plugin_id
            )));
        }
        Ok(Response::new(RegisterResponse {
            plugin_id: injected,
            display_name: crate::DISPLAY_NAME.to_owned(),
            version: env!("CARGO_PKG_VERSION").to_owned(),
            abi_major: sm_plugin_api::ABI_MAJOR,
            capabilities: Vec::new(),
            extensions: Vec::new(),
            jobs: vec![Self::job_definition()],
            settings_schema: DurationCollectionSettings::schema(),
            data_plane_endpoint: None,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Page;
    use crate::judge::MovieInput;
    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    struct FakeHost {
        pages: Mutex<VecDeque<Page>>,
        pub patches: Mutex<Vec<(i64, i64)>>,
        pub patch_results: Mutex<VecDeque<bool>>,
    }

    impl FakeHost {
        fn new(pages: Vec<Page>) -> Self {
            Self {
                pages: Mutex::new(pages.into()),
                patches: Mutex::new(Vec::new()),
                patch_results: Mutex::new(VecDeque::new()),
            }
        }
    }

    #[async_trait]
    impl HostMovies for FakeHost {
        async fn list_page(
            &self,
            _after_id: i64,
            _limit: i32,
        ) -> Result<crate::host::Page, Box<dyn std::error::Error + Send + Sync>> {
            Ok(self.pages.lock().unwrap().pop_front().unwrap_or(crate::host::Page {
                movies: Vec::new(),
                next_cursor: None,
            }))
        }

        async fn patch_is_collection(
            &self,
            movie_id: i64,
            expected_revision: i64,
        ) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
            self.patches.lock().unwrap().push((movie_id, expected_revision));
            Ok(self.patch_results.lock().unwrap().pop_front().unwrap_or(true))
        }
    }

    fn input(id: i64, duration: u64, number: &str, is_collection: bool) -> MovieInput {
        MovieInput {
            movie_id: id,
            revision: 7,
            duration_minutes: duration,
            movie_number: number.to_owned(),
            is_collection,
            collection_owner: None,
            tag_names: vec![],
        }
    }

    fn page(movies: Vec<MovieInput>, next_cursor: Option<i64>) -> crate::host::Page {
        crate::host::Page { movies, next_cursor }
    }

    #[tokio::test]
    async fn scan_marks_only_collections_and_respects_owner() {
        // 对齐上游 test_judge_movies_marks_only_collections_and_respects_owner。
        let mut owned = input(3, 180, "ABP-001", false);
        owned.collection_owner = Some("host:manual".to_owned());
        let host = FakeHost::new(vec![page(
            vec![
                input(1, 180, "ABP-001", false), // 命中（阈值 120）
                input(2, 60, "ABP-001", true),   // 已是合集
                owned,                            // 手动判定，跳过
                input(4, 0, "ABP-001", false),   // 太短
            ],
            None,
        )]);
        let config = DurationCollectionSettings {
            duration_threshold_minutes: 120,
            ..Default::default()
        };
        let mut progresses = Vec::new();
        let stats = run_scan(&host, &config, "test-plugin", |s| progresses.push(s.scanned))
            .await
            .unwrap();
        assert_eq!(stats.scanned, 4);
        assert_eq!(stats.updated, 1);
        assert_eq!(stats.unchanged, 2);
        assert_eq!(stats.skipped_owned, 1);
        assert_eq!(stats.patch_failed, 0);
        assert_eq!(*host.patches.lock().unwrap(), vec![(1, 7)]);
        assert_eq!(progresses, vec![4]);
    }

    #[tokio::test]
    async fn scan_paginates_with_cursor() {
        let host = FakeHost::new(vec![
            page(vec![input(1, 400, "ABP-001", false)], Some(1)),
            page(vec![input(2, 400, "ABP-002", false)], None),
        ]);
        let config = DurationCollectionSettings::default();
        let stats = run_scan(&host, &config, "test-plugin", |_| {}).await.unwrap();
        assert_eq!(stats.scanned, 2);
        assert_eq!(stats.updated, 2);
    }

    #[tokio::test]
    async fn scan_counts_patch_failures() {
        let host = FakeHost::new(vec![page(vec![input(1, 400, "ABP-001", false)], None)]);
        host.patch_results.lock().unwrap().push_back(false);
        let config = DurationCollectionSettings::default();
        let stats = run_scan(&host, &config, "test-plugin", |_| {}).await.unwrap();
        assert_eq!(stats.updated, 0);
        assert_eq!(stats.patch_failed, 1);
    }

    #[test]
    fn job_definition_matches_upstream() {
        let job = Control::job_definition();
        assert_eq!(job.task_key, TASK_KEY);
        assert_eq!(job.log_name, LOG_NAME);
        assert_eq!(job.cli_name, CLI_NAME);
        assert_eq!(job.default_cron, DEFAULT_CRON);
        assert!(!job.manual_only);
    }
}
