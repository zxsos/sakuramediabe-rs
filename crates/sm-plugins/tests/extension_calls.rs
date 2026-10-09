//! 扩展点调用面的集成测试：起**真实** gRPC server（`127.0.0.1` + 内核分配端口）。
//!
//! # 为什么要用真服务
//!
//! 调用面要验证的是「rpc 真的发出去了、状态真的回来了」—— 用假的 client 只能
//! 测到自己写的那层包装。端口交给内核分配，避免与其它测试抢。
//!
//! # 逐条锁定的语义
//!
//! | 用例 | 断言 | 为什么重要 |
//! |---|---|---|
//! | 命中 | `Found`，字段原样回来 | 「接进调用面」的落点 |
//! | 未命中 | `NotFound` **而不是**错误 | proto：`found=false` 时宿主试下一个来源 |
//! | 插件报错 | `Call` 错误，带上游消息 | 不能把插件内部错误吞成「没收录」 |
//! | 榜单 | 番号列表顺序即排名 | `FetchRankingResponse` 的约定 |

use std::net::SocketAddr;

use sm_plugin_api::v1::metadata_source_extension_service_client::MetadataSourceExtensionServiceClient;
use sm_plugin_api::v1::metadata_source_extension_service_server::{
    MetadataSourceExtensionService, MetadataSourceExtensionServiceServer,
};
use sm_plugin_api::v1::ranking_source_extension_service_client::RankingSourceExtensionServiceClient;
use sm_plugin_api::v1::ranking_source_extension_service_server::{
    RankingSourceExtensionService, RankingSourceExtensionServiceServer,
};
use sm_plugin_api::v1::{
    FetchMovieRequest, FetchMovieResponse, FetchRankingRequest, FetchRankingResponse,
    ResolveRankingPeriodsRequest, ResolveRankingPeriodsResponse,
};
use sm_plugins::extension_calls::{
    fetch_movie, fetch_ranking, interpret_fetch_movie, resolve_ranking_periods, ExtensionCallError,
    MovieLookup,
};
use tokio::net::TcpListener;
use tokio_stream::wrappers::TcpListenerStream;
use tonic::transport::{Channel, Server};
use tonic::{Request, Response, Status};

/// 一个假的元数据/排行榜插件：只收录 `ABC-123`，其余一律「未收录」。
struct FakePlugin {
    /// 为 `true` 时 `FetchMovie` 直接返回内部错误。
    fail: bool,
}

#[tonic::async_trait]
impl MetadataSourceExtensionService for FakePlugin {
    async fn fetch_movie(
        &self,
        request: Request<FetchMovieRequest>,
    ) -> Result<Response<FetchMovieResponse>, Status> {
        if self.fail {
            return Err(Status::internal("插件取数时炸了"));
        }
        let number = request.into_inner().movie_number;
        if number != "ABC-123" {
            // 未收录 = 空响应（`found` 取默认 false），不是错误。
            return Ok(Response::new(FetchMovieResponse::default()));
        }
        Ok(Response::new(FetchMovieResponse {
            found: true,
            movie_number: number,
            title: "一部片子".to_owned(),
            release_date: "2026-01-02".to_owned(),
            duration_minutes: 120,
            cover_image_path: "/tmp/cover.jpg".to_owned(),
            ..Default::default()
        }))
    }
}

#[tonic::async_trait]
impl RankingSourceExtensionService for FakePlugin {
    async fn fetch_ranking(
        &self,
        request: Request<FetchRankingRequest>,
    ) -> Result<Response<FetchRankingResponse>, Status> {
        let request = request.into_inner();
        // 榜单内容随 period 变，便于断言请求真的带过去了。
        Ok(Response::new(FetchRankingResponse {
            movie_numbers: match request.period.as_str() {
                "daily" => vec!["ABC-123".to_owned(), "DEF-456".to_owned()],
                _ => Vec::new(),
            },
        }))
    }

    /// 假插件照 TOP250 那条规则回：`2026`（今年）永远抓，历史年份**已有条目
    /// 就不抓** —— 这正是宿主递 `periods_with_items` 的用处。
    async fn resolve_ranking_periods(
        &self,
        request: Request<ResolveRankingPeriodsRequest>,
    ) -> Result<Response<ResolveRankingPeriodsResponse>, Status> {
        if self.fail {
            return Err(Status::internal("插件算周期时炸了"));
        }
        let request = request.into_inner();
        if request.board_key != "top250" {
            // 没有动态周期的榜单：声明里的静态周期就是全部，这里不必回。
            return Ok(Response::new(ResolveRankingPeriodsResponse::default()));
        }
        let periods = ["2026", "2025", "2024"]
            .into_iter()
            .filter(|year| !request.periods_with_items.iter().any(|p| p == year))
            .map(str::to_owned)
            .collect();
        Ok(Response::new(ResolveRankingPeriodsResponse { periods }))
    }
}

/// 起一个真 server，返回它的地址。句柄丢掉即可：测试进程退出时自然结束。
async fn spawn(fail: bool) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("监听");
    let addr = listener.local_addr().expect("地址");
    tokio::spawn(async move {
        Server::builder()
            .add_service(MetadataSourceExtensionServiceServer::new(FakePlugin {
                fail,
            }))
            .add_service(RankingSourceExtensionServiceServer::new(FakePlugin {
                fail,
            }))
            .serve_with_incoming(TcpListenerStream::new(listener))
            .await
            .expect("server 不该自己退出");
    });
    addr
}

async fn metadata_client(addr: SocketAddr) -> MetadataSourceExtensionServiceClient<Channel> {
    MetadataSourceExtensionServiceClient::connect(format!("http://{addr}"))
        .await
        .expect("连接")
}

async fn ranking_client(addr: SocketAddr) -> RankingSourceExtensionServiceClient<Channel> {
    RankingSourceExtensionServiceClient::connect(format!("http://{addr}"))
        .await
        .expect("连接")
}

#[tokio::test]
async fn a_hit_comes_back_with_its_payload() {
    let mut client = metadata_client(spawn(false).await).await;
    let response = fetch_movie(
        &mut client,
        FetchMovieRequest {
            movie_number: "ABC-123".to_owned(),
            delivery_dir: "/tmp/delivery".to_owned(),
        },
        None,
    )
    .await
    .expect("调用应当成功");

    let MovieLookup::Found(found) = interpret_fetch_movie(response) else {
        panic!("收录了的片子应判为 Found");
    };
    assert_eq!(found.movie_number, "ABC-123");
    assert_eq!(found.title, "一部片子");
    assert_eq!(found.duration_minutes, 120);
}

#[tokio::test]
async fn a_miss_is_not_an_error() {
    // proto：`found=false` 时宿主尝试下一个来源 —— 所以它必须是**成功调用**
    // 里的一种结果，而不是 Err。当成 Err 会让兜底链路在第一个插件就停下。
    let mut client = metadata_client(spawn(false).await).await;
    let response = fetch_movie(
        &mut client,
        FetchMovieRequest {
            movie_number: "NOPE-000".to_owned(),
            delivery_dir: "/tmp/delivery".to_owned(),
        },
        None,
    )
    .await
    .expect("未收录也应当是成功的调用");
    assert_eq!(interpret_fetch_movie(response), MovieLookup::NotFound);
}

#[tokio::test]
async fn a_plugin_failure_is_reported_as_a_call_error() {
    // 插件内部出错与「没收录」必须分开：前者的处置是记失败，后者是换来源。
    let mut client = metadata_client(spawn(true).await).await;
    let error = fetch_movie(
        &mut client,
        FetchMovieRequest {
            movie_number: "ABC-123".to_owned(),
            delivery_dir: "/tmp/delivery".to_owned(),
        },
        None,
    )
    .await
    .expect_err("插件报错应当成为 Err");

    assert_eq!(error.code(), "extension_call_failed");
    assert!(
        matches!(&error, ExtensionCallError::Call(detail) if detail.contains("插件取数时炸了")),
        "错误要带上插件给的消息：{error:?}"
    );
}

#[tokio::test]
async fn ranking_numbers_come_back_in_rank_order() {
    let mut client = ranking_client(spawn(false).await).await;
    let response = fetch_ranking(
        &mut client,
        FetchRankingRequest {
            board_key: "daily".to_owned(),
            period: "daily".to_owned(),
        },
        None,
    )
    .await
    .expect("调用应当成功");
    assert_eq!(response.movie_numbers, vec!["ABC-123", "DEF-456"]);

    // 另一个 period 是空榜 —— 空列表是「榜上没东西」，不是错误。
    let response = fetch_ranking(
        &mut client,
        FetchRankingRequest {
            board_key: "daily".to_owned(),
            period: "weekly".to_owned(),
        },
        None,
    )
    .await
    .expect("调用应当成功");
    assert!(response.movie_numbers.is_empty());
}

#[tokio::test]
async fn the_host_tells_the_plugin_which_periods_already_have_items() {
    // 上游 `should_fetch(period, has_items)`：宿主不知道插件的账号配置，插件
    // 不知道库里的条目 —— 所以这条 rpc 把后者递过去，裁决权留在插件。
    let mut client = ranking_client(spawn(false).await).await;
    let response = resolve_ranking_periods(
        &mut client,
        ResolveRankingPeriodsRequest {
            board_key: "top250".to_owned(),
            // 2024 已经抓过了，今年（2026）没有。
            periods_with_items: vec!["2024".to_owned()],
        },
        None,
    )
    .await
    .expect("调用应当成功");
    assert_eq!(
        response.periods,
        vec!["2026", "2025"],
        "已有条目的周期不再抓"
    );

    // 空列表是「本次不抓」，是**正常结果**（如账号未配置），不是错误。
    let response = resolve_ranking_periods(
        &mut client,
        ResolveRankingPeriodsRequest {
            board_key: "playback_all".to_owned(),
            periods_with_items: vec![],
        },
        None,
    )
    .await
    .expect("没有动态周期的榜单回空列表也应当成功");
    assert!(response.periods.is_empty());
}

#[tokio::test]
async fn a_plugin_failure_while_resolving_periods_is_a_call_error() {
    let mut client = ranking_client(spawn(true).await).await;
    let error = resolve_ranking_periods(
        &mut client,
        ResolveRankingPeriodsRequest {
            board_key: "top250".to_owned(),
            periods_with_items: vec![],
        },
        None,
    )
    .await
    .expect_err("插件报错应当成为 Err");
    assert_eq!(error.code(), "extension_call_failed");
}
