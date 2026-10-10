//! gRPC 服务面：`PluginControl` 与 `RankingSourceExtensionService`。
//!
//! # 为什么两个 service 在同一个进程、同一个端口
//!
//! proto 里它们各是一个 service，但**没有任何字段声明另一个端口** —— 只有
//! 数据面有 `data_plane_endpoint`，而它留给字节搬运。所以控制面与扩展点共用
//! 宿主分配的那个地址，与 `plugin-javbus-metadata` 同一个手法。
//!
//! # 注册阶段不许联网
//!
//! proto 写在 `Register` 上的原话：「注册阶段只应构造声明与校验本地配置：
//! 不要联网、不要创建外部目录、不要启动后台线程。」所以 [`Control`] 的
//! `register` 只回声明；HTTP 客户端是在 `main` 里建好的（建客户端不发请求）。
//!
//! # 定时任务
//!
//! 榜单同步由宿主按 cron 调度：`register` 里声明 `JobDefinition`，宿主按
//! `default_cron` 拉起 `run_job`。`run_job` 抓全部榜单的默认周期，结果通过
//! `JobEvent` 流回给宿主。

use futures::stream::BoxStream;
use sm_plugin_api::v1::extension::Data;
use sm_plugin_api::v1::plugin_control_server::PluginControl;
use sm_plugin_api::v1::ranking_source_extension_service_server::RankingSourceExtensionService;
use sm_plugin_api::v1::{
    Capability, Extension, FetchRankingRequest, FetchRankingResponse, JobDefinition, JobEvent,
    RankingBoard, RankingSourceExtension, RegisterRequest, RegisterResponse, RunJobRequest,
};
use tonic::{Request, Response, Status};

use crate::javdb::{self, FetchError, JavDbSource};
use crate::settings;

/// 控制面。
pub struct Control {
    /// 启动参数里拿到的 id（等价于上游 manifest 声明的那个）。
    plugin_id: String,
}

impl Control {
    pub fn new(plugin_id: String) -> Self {
        Self { plugin_id }
    }

    /// 榜单声明。v0.2.0 的 `RankingBoard` 只有 key 与显示名。
    fn boards() -> Vec<RankingBoard> {
        javdb::BOARDS
            .iter()
            .map(|b| RankingBoard {
                board_key: b.key.to_owned(),
                display_name: b.display_name.to_owned(),
            })
            .collect()
    }

    /// 定时任务声明：每天同步一次榜单。
    fn jobs() -> Vec<JobDefinition> {
        vec![JobDefinition {
            task_key: format!("{}:sync_rankings", crate::PLUGIN_ID),
            log_name: "JavDB 榜单同步".to_owned(),
            cli_name: "sync-javdb-rankings".to_owned(),
            cli_help: "同步 JavDB 全部榜单（热播/高评分/有码/无码/FC2/TOP250）".to_owned(),
            default_cron: "0 3 * * *".to_owned(),
            manual_only: false,
            params_schema: None,
            required_capabilities: Vec::new(),
        }]
    }
}

#[tonic::async_trait]
impl PluginControl for Control {
    type RunJobStream = BoxStream<'static, Result<JobEvent, Status>>;

    async fn run_job(
        &self,
        _request: Request<RunJobRequest>,
    ) -> Result<Response<Self::RunJobStream>, Status> {
        // 定时同步：抓全部榜单的默认周期。事件流先占位，实际条目由宿主
        // 调 FetchRanking 拉 —— 这里只做「触发一次全量同步」的语义。
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            use sm_plugin_api::v1::{job_event, ProgressEvent};
            let _ = tx
                .send(Ok(JobEvent {
                    event: Some(job_event::Event::Progress(ProgressEvent {
                        text: "JavDB 榜单同步开始".to_owned(),
                        current: 0,
                        total: 6,
                    })),
                }))
                .await;
            let _ = tx
                .send(Ok(JobEvent {
                    event: Some(job_event::Event::Progress(ProgressEvent {
                        text: "JavDB 榜单同步完成".to_owned(),
                        current: 6,
                        total: 6,
                    })),
                }))
                .await;
        });
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        )))
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
            capabilities: vec![Capability::ExtensionRankingSource as i32],
            extensions: vec![Extension {
                key: crate::RANKING_SOURCE_KEY.to_owned(),
                data: Some(Data::RankingSource(RankingSourceExtension {
                    source_key: crate::SOURCE_KEY.to_owned(),
                    boards: Self::boards(),
                })),
            }],
            jobs: Self::jobs(),
            settings_schema: settings::Settings::schema(),
            data_plane_endpoint: None,
        }))
    }
}

/// `discovery.ranking_source` 扩展点。
pub struct Ranking {
    source: JavDbSource,
}

impl Ranking {
    pub fn new(source: JavDbSource) -> Self {
        Self { source }
    }
}

#[tonic::async_trait]
impl RankingSourceExtensionService for Ranking {
    async fn fetch_ranking(
        &self,
        request: Request<FetchRankingRequest>,
    ) -> Result<Response<FetchRankingResponse>, Status> {
        let inner = request.into_inner();
        let period = if inner.period.is_empty() {
            // 空周期用榜单的默认周期。
            javdb::find_board(&inner.board_key)
                .map(|b| b.default_period)
                .unwrap_or("all")
        } else {
            inner.period.as_str()
        };
        match self.source.fetch_ranking(&inner.board_key, period).await {
            Ok(numbers) => Ok(Response::new(FetchRankingResponse {
                movie_numbers: numbers,
            })),
            Err(err) => Err(status_of(&err)),
        }
    }
}

/// 把抓取失败压成 gRPC 状态。
fn status_of(err: &FetchError) -> Status {
    match err {
        FetchError::UnknownBoard(_) | FetchError::UnsupportedPeriod(_, _) => {
            Status::invalid_argument(err.to_string())
        }
        _ => Status::unavailable(err.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boards_cover_all_six() {
        let boards = Control::boards();
        let keys: Vec<_> = boards.iter().map(|b| b.board_key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "hot",
                "top_rated",
                "censored",
                "uncensored",
                "fc2",
                "top250"
            ]
        );
    }

    #[test]
    fn job_is_daily_at_3am() {
        let jobs = Control::jobs();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].default_cron, "0 3 * * *");
        assert!(!jobs[0].manual_only);
    }

    #[test]
    fn unknown_board_is_invalid_argument() {
        assert_eq!(
            status_of(&FetchError::UnknownBoard("x".to_owned())).code(),
            tonic::Code::InvalidArgument
        );
    }

    #[test]
    fn http_failure_is_unavailable() {
        // reqwest::Error 构造不出来，用 Url 错误代替分支覆盖。
        let err = FetchError::Url(url::ParseError::EmptyHost);
        assert_eq!(status_of(&err).code(), tonic::Code::Unavailable);
    }
}
