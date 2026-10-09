//! 宿主能力出口的 gRPC 服务端（上游 `PluginContext`）。
//!
//! # 上游对应
//!
//! `src/plugins/context.py:1330-1375` 的 `PluginContext`：插件通过它回调宿主
//! （`actors` / `movies` / `media` / `downloads` / `imports` / `subscriptions`
//! / `notifications` …）。proto 侧是 `PluginHost`（`proto/host.proto:391`）。
//!
//! ⚠️ 方向是**反的**：`PluginControl` 是宿主调插件，而 `PluginHost` 是**插件
//! 调宿主**。所以插件进程拿到的两个地址是两回事（前者由宿主注入
//! `SAKURAMEDIA_PLUGIN_GRPC_ADDR`，后者是 `SAKURAMEDIA_HOST_GRPC_ADDR`）。
//!
//! # 已接线 / 未接线
//!
//! 本模块先把**只读**那一半接出来，其余一律 `unimplemented`：
//!
//! | 组 | rpc | 状态 |
//! |---|---|---|
//! | 影片 | `GetMovie` / `FindMoviesByNumbers` | **已接**（`sm_db` 直读）|
//! | 演员 | `GetActor` | **已接** |
//! | 其余 33 个 | | ❌ 未接（见下面「为什么先只接这三个」）|
//!
//! ## 为什么先只接这三个
//!
//! 剩下的大多要**写**：`PatchMovie` / `PatchActor` 要走主权网关（乐观并发 +
//! 字段归属，proto 注释写得明明白白），`SetSubscription` 要 `MovieService`，
//! `SubmitDownload` 要整个下载域。一个只读快照错了最多是插件拿到空值；
//! 一个写操作错了会**改坏用户数据**。所以先把只读那一半做成可验证的形状，
//! 写操作按组逐个接。
//!
//! ## 未接线的那 33 个**显式列出**，不用通配实现
//!
//! 插件拿到 `unimplemented` 时能对上这张表；而「漏了哪一个」在代码里一眼
//! 看得见 —— 用宏把形状收成一行一个，就是为了这个。
//!
//! ## `owners` / `revision` 不再猜
//!
//! 快照里这两个字段取自 `movie.field_owners` 与 `movie.mutation_revision`，
//! 不是宿主编出来的 —— 插件拿它们做乐观并发。`actors` / `tags` 两项目前是
//! 空数组：它们要 join `movie_actor` / `movie_tag`，本轮没接。空数组表示
//! 「这次没带」，不是「这部片子没有演员」—— 插件别据此删数据。

use std::collections::HashMap;
use std::net::SocketAddr;

use sm_db::repo::{ActorRepository, MovieRepository};
use sm_db::Db;
use sm_plugin_api::v1::plugin_host_server::{PluginHost, PluginHostServer};
use sm_plugin_api::v1::{
    ActorSnapshot, AddVideoItemsRequest, AddVideoItemsResponse, BrowseLibraryRequest,
    BrowseLibraryResponse, CountSubscriptionsByStatusRequest, CountSubscriptionsByStatusResponse,
    CreateNotificationRequest, CreateNotificationResponse, EnqueueImportRequest,
    EnqueueImportResponse, EnsureCollectionRequest, EnsureCollectionResponse, FindByNumbersRequest,
    FindByNumbersResponse, GetActorRequest, GetActorResponse, GetImportStatusRequest,
    GetImportStatusResponse, GetMovieRequest, GetMovieResponse, GetVideoRequest, GetVideoResponse,
    ImportMovieByNumberRequest, ImportMovieByNumberResponse, ImportSubtitleRequest,
    ImportSubtitleResponse, ListActorsRequest, ListActorsResponse, ListDownloadTargetsRequest,
    ListDownloadTargetsResponse, ListLibrariesRequest, ListLibrariesResponse, ListMediaRequest,
    ListMediaResponse, ListMovieNumbersRequest, ListMovieNumbersResponse, ListMoviesRequest,
    ListMoviesResponse, ListNotificationsRequest, ListNotificationsResponse,
    ListSubscriptionsRequest, ListSubscriptionsResponse, ListSubtitlesRequest,
    ListSubtitlesResponse, ListVideosRequest, ListVideosResponse, MovieSnapshot, PatchActorRequest,
    PatchActorResponse, PatchMovieRequest, PatchMovieResponse, PresenceRequest, PresenceResponse,
    ReadSubtitleRequest, ReadSubtitleResponse, RemoveVideoItemsRequest, RemoveVideoItemsResponse,
    ResetSubscriptionSearchRequest, ResetSubscriptionSearchResponse, ResolveNotificationRequest,
    ResolveNotificationResponse, SearchCandidatesRequest, SearchCandidatesResponse,
    SetCollectionMembersRequest, SetCollectionMembersResponse, SetSubscriptionRequest,
    SetSubscriptionResponse, SubmitDownloadRequest, SubmitDownloadResponse,
    SyncRankingSourcesRequest, SyncRankingSourcesResponse,
};
use tonic::transport::Server;
use tonic::{Request, Response, Status};

/// 生成一串「尚未接线」的 rpc 实现。
///
/// 36 个方法里只接了 3 个，剩下 33 个的形状**完全一样**（`unimplemented`）。
///
/// # 为什么这里写 `Pin<Box<dyn Future>>` 而不是 `async fn`
///
/// `#[async_trait]` 展开的是这个签名；而它**看不见**宏调用里的 `async fn`
/// （属性先于宏展开处理），于是那些方法会以「不带生命周期的 async fn」留在
/// impl 里 —— 与 trait 对不上。所以这里直接写成展开后的形状。
macro_rules! unwired {
    ($($name:ident($request:ty, $response:ty);)*) => {
        $(
            fn $name<'life0, 'async_trait>(
                &'life0 self,
                _request: Request<$request>,
            ) -> std::pin::Pin<
                Box<
                    dyn std::future::Future<Output = Result<Response<$response>, Status>>
                        + Send
                        + 'async_trait,
                >,
            >
            where
                'life0: 'async_trait,
                Self: 'async_trait,
            {
                Box::pin(async {
                    Err(Status::unimplemented(concat!(
                        stringify!($name),
                        " 尚未接线（见 plugin_host 模块文档的已接线/未接线表）"
                    )))
                })
            }
        )*
    };
}

/// 宿主能力出口。
pub struct PluginHostService {
    movies: MovieRepository,
    actors: ActorRepository,
}

impl PluginHostService {
    pub fn new(db: &Db) -> Self {
        Self {
            movies: MovieRepository::new(db.clone()),
            actors: ActorRepository::new(db.clone()),
        }
    }
}

#[tonic::async_trait]
impl PluginHost for PluginHostService {
    /// 按 id 取一部影片。**找不到是 `NOT_FOUND`** —— proto 注释要求插件据此
    /// 返回 `None`，而不是当成错误。
    async fn get_movie(
        &self,
        request: Request<GetMovieRequest>,
    ) -> Result<Response<GetMovieResponse>, Status> {
        let movie_id = i32::try_from(request.into_inner().movie_id)
            .map_err(|_| Status::invalid_argument("movie_id 超出范围"))?;
        let movie = self
            .movies
            .find_by_id(movie_id)
            .await
            .map_err(|_| Status::internal("读取影片失败"))?
            .ok_or_else(|| Status::not_found("影片不存在"))?;
        Ok(Response::new(GetMovieResponse {
            movie: Some(movie_snapshot(&movie)),
        }))
    }

    /// 按番号批量定位。**找不到那一部就不出现在结果里**（不是错误）——
    /// 上游 `MovieApi.find_by_numbers` 就是这个语义。
    ///
    /// 走批量查询而不是逐条 [`MovieRepository::find_by_number`]：番号可能几十个，
    /// 逐条就是 N+1（同 `movie.rs` 里那些批量方法的理由）。
    async fn find_movies_by_numbers(
        &self,
        request: Request<FindByNumbersRequest>,
    ) -> Result<Response<FindByNumbersResponse>, Status> {
        let numbers = request.into_inner().movie_numbers;
        let found = self
            .movies
            .find_by_numbers(&numbers)
            .await
            .map_err(|_| Status::internal("读取影片失败"))?;
        // 按**请求顺序**输出：调用方按位置对应自己传进来的番号。
        let movies = numbers
            .iter()
            .filter_map(|number| found.get(number).map(movie_snapshot))
            .collect();
        Ok(Response::new(FindByNumbersResponse { movies }))
    }

    async fn get_actor(
        &self,
        request: Request<GetActorRequest>,
    ) -> Result<Response<GetActorResponse>, Status> {
        let actor_id = i32::try_from(request.into_inner().actor_id)
            .map_err(|_| Status::invalid_argument("actor_id 超出范围"))?;
        let actor = self
            .actors
            .find_by_id(actor_id)
            .await
            .map_err(|_| Status::internal("读取演员失败"))?
            .ok_or_else(|| Status::not_found("演员不存在"))?;
        Ok(Response::new(GetActorResponse {
            actor: Some(ActorSnapshot {
                actor_id: i64::from(actor.id),
                revision: actor.mutation_revision,
                values: actor_values(&actor),
                owners: owners_of(&actor.field_owners),
            }),
        }))
    }

    unwired! {
        list_movies(ListMoviesRequest, ListMoviesResponse);
        patch_movie(PatchMovieRequest, PatchMovieResponse);
        list_actors(ListActorsRequest, ListActorsResponse);
        patch_actor(PatchActorRequest, PatchActorResponse);
        list_subscriptions(ListSubscriptionsRequest, ListSubscriptionsResponse);
        count_subscriptions_by_status(
            CountSubscriptionsByStatusRequest,
            CountSubscriptionsByStatusResponse
        );
        set_subscription(SetSubscriptionRequest, SetSubscriptionResponse);
        reset_subscription_search(ResetSubscriptionSearchRequest, ResetSubscriptionSearchResponse);
        create_notification(CreateNotificationRequest, CreateNotificationResponse);
        create_notification_once(CreateNotificationRequest, CreateNotificationResponse);
        list_notifications(ListNotificationsRequest, ListNotificationsResponse);
        resolve_notification(ResolveNotificationRequest, ResolveNotificationResponse);
        ensure_collection(EnsureCollectionRequest, EnsureCollectionResponse);
        set_collection_members(SetCollectionMembersRequest, SetCollectionMembersResponse);
        add_video_items(AddVideoItemsRequest, AddVideoItemsResponse);
        remove_video_items(RemoveVideoItemsRequest, RemoveVideoItemsResponse);
        list_subtitles(ListSubtitlesRequest, ListSubtitlesResponse);
        read_subtitle(ReadSubtitleRequest, ReadSubtitleResponse);
        import_subtitle(ImportSubtitleRequest, ImportSubtitleResponse);
        list_media(ListMediaRequest, ListMediaResponse);
        presence_for_movies(PresenceRequest, PresenceResponse);
        list_download_targets(ListDownloadTargetsRequest, ListDownloadTargetsResponse);
        search_download_candidates(SearchCandidatesRequest, SearchCandidatesResponse);
        submit_download(SubmitDownloadRequest, SubmitDownloadResponse);
        list_libraries(ListLibrariesRequest, ListLibrariesResponse);
        browse_library(BrowseLibraryRequest, BrowseLibraryResponse);
        enqueue_import(EnqueueImportRequest, EnqueueImportResponse);
        get_import_status(GetImportStatusRequest, GetImportStatusResponse);
        get_video(GetVideoRequest, GetVideoResponse);
        list_videos(ListVideosRequest, ListVideosResponse);
        import_movie_by_number(ImportMovieByNumberRequest, ImportMovieByNumberResponse);
        list_existing_movie_numbers(ListMovieNumbersRequest, ListMovieNumbersResponse);
        sync_ranking_sources(SyncRankingSourcesRequest, SyncRankingSourcesResponse);
    }
}

/// 起一个 `PluginHost` 服务端。返回插件要用的端点串。
///
/// 与插件进程同一个「先占端口、再交给别人 bind」的套路
/// （[`sm_plugins::supervisor::reserve_addr`]）：宿主自己也要一个端口，而端口
/// 分配方式两处一致才不会出现「一边自选、一边注入」的两套语义。
pub async fn serve(db: &Db) -> Result<String, std::io::Error> {
    let addr: SocketAddr = sm_plugins::supervisor::reserve_addr()?;
    let service = PluginHostService::new(db);
    tokio::spawn(async move {
        if let Err(error) = Server::builder()
            .add_service(PluginHostServer::new(service))
            .serve(addr)
            .await
        {
            tracing::error!(%addr, %error, "PluginHost 服务退出");
        }
    });
    Ok(format!("http://{addr}"))
}

/// 影片快照。
fn movie_snapshot(movie: &sm_db::Movie) -> MovieSnapshot {
    MovieSnapshot {
        movie_id: i64::from(movie.id),
        revision: movie.mutation_revision,
        values: movie_values(movie),
        owners: owners_of(&movie.field_owners),
        // 这两项要 join `movie_actor` / `movie_tag`，本轮没接 —— **空数组**
        // 表示「这次没带」，不是「没有演员」。
        actors: Vec::new(),
        tags: Vec::new(),
    }
}

fn movie_values(movie: &sm_db::Movie) -> HashMap<String, prost_types::Value> {
    let mut values = HashMap::new();
    put_str(&mut values, "movie_number", &movie.movie_number);
    put_str(&mut values, "title", &movie.title);
    put_str(&mut values, "summary", &movie.summary);
    put_opt(&mut values, "javdb_id", movie.javdb_id.as_deref());
    put_opt(
        &mut values,
        "release_date",
        movie.release_date.map(|date| date.to_string()).as_deref(),
    );
    put_number(
        &mut values,
        "duration_minutes",
        f64::from(movie.duration_minutes),
    );
    put_number(&mut values, "score", movie.score);
    put_number(&mut values, "score_number", f64::from(movie.score_number));
    values
}

fn actor_values(actor: &sm_db::Actor) -> HashMap<String, prost_types::Value> {
    let mut values = HashMap::new();
    put_str(&mut values, "name", &actor.name);
    put_str(&mut values, "alias_name", &actor.alias_name);
    put_str(&mut values, "javdb_id", &actor.javdb_id);
    put_number(&mut values, "gender", f64::from(actor.gender));
    values
}

/// 从 `field_owners`（`{字段: owner}`）取出**去重**后的 owner 列表。
///
/// proto 的 `owners` 是 `repeated string`：它回答的是「谁动过这行」，不是
/// 「哪个字段归谁」。所以这里取值的并集。
fn owners_of(field_owners: &serde_json::Value) -> Vec<String> {
    let mut owners: Vec<String> = field_owners
        .as_object()
        .map(|map| {
            map.values()
                .filter_map(serde_json::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    owners.sort();
    owners.dedup();
    owners
}

fn put_str(values: &mut HashMap<String, prost_types::Value>, key: &str, value: &str) {
    values.insert(key.to_owned(), string_value(value));
}

/// 缺失的可选字段**不进**快照 —— 空串会被插件当成「这个值就是空的」。
fn put_opt(values: &mut HashMap<String, prost_types::Value>, key: &str, value: Option<&str>) {
    if let Some(value) = value {
        put_str(values, key, value);
    }
}

fn put_number(values: &mut HashMap<String, prost_types::Value>, key: &str, value: f64) {
    values.insert(
        key.to_owned(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(value)),
        },
    );
}

fn string_value(value: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(value.to_owned())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ `field_owners` 是 `{字段: owner}`，而 proto 的 `owners` 是**去重后的
    /// owner 列表** —— 形状不同，别把字段名塞进去。
    #[test]
    fn owners_are_deduped_values_not_keys() {
        let owners = owners_of(&serde_json::json!({
            "title": "plugin:javbus",
            "summary": "plugin:javbus",
            "score": "host:manual",
        }));
        assert_eq!(owners, vec!["host:manual", "plugin:javbus"]);
        assert!(owners_of(&serde_json::json!({})).is_empty());
        assert!(owners_of(&serde_json::Value::Null).is_empty());
    }

    /// 缺失的可选字段不进快照（见 [`put_opt`] 的注释）。
    #[test]
    fn missing_optional_fields_are_omitted() {
        let mut values = HashMap::new();
        put_opt(&mut values, "javdb_id", None);
        put_opt(&mut values, "release_date", Some("2026-01-02"));
        assert!(!values.contains_key("javdb_id"));
        assert_eq!(
            values["release_date"].kind,
            Some(prost_types::value::Kind::StringValue(
                "2026-01-02".to_owned()
            ))
        );
    }
}
