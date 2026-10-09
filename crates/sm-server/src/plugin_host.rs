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
//! | 组 | rpc | 状态 |
//! |---|---|---|
//! | 影片 | `GetMovie` / `FindMoviesByNumbers` | **已接**（`sm_db` 直读）|
//! | 影片 | `ListMovies` / `PatchMovie` | **已接**（游标分页 / 主权网关）|
//! | 演员 | `GetActor` | **已接** |
//! | 字幕 | `ImportSubtitle` | **已接**（转发 `SubtitleAssetService`）|
//! | 其余 30 个 | | ❌ 未接（见下面「为什么只接了这些」）|
//!
//! ## 字幕导入是**转发**，业务不在这里重写
//!
//! `ImportSubtitle` 的四个状态（`imported` / `duplicate` / `movie_not_found` /
//! `invalid_format`）与去重规则（**内容 sha256**，不是文件名）全在 `sm-service` 的
//! `SubtitleAssetService::import_subtitle_content` 里 —— 那是上游
//! `subtitle_asset_service.py` 的逐行对拍实现。本文件只做两件事：转调用、把状态翻成
//! proto 在字段上写死的四个串。
//!
//! **业务分支不报成 gRPC 错误**：影片不存在、扩展名不支持、内容重复都是**结果**
//! （插件据此分桶统计），只有基础设施错误（连不上库、磁盘满）才走 `Err`。
//!
//! ⚠️ 一处**已知的对上游偏差**（在 service 层，不在本文件）：`import_subtitle_content`
//! 查影片走 `MovieRepository::find_by_number`（裸列相等，大小写与分隔符都敏感），而
//! 上游 `find_movie_by_number` 是「`UPPER(...)` 点查 + 分隔符互换」。于是插件传
//! `abc_123` 时这里判 `movie_not_found`，而上游能命中 `ABC-123`。对**本仓已移植的两个
//! 插件**没有影响（它们传的是快照里的库内规范番号），但第三方插件若直接透传用户输入
//! 就会撞上 —— 修法是把那一处换成 `MovieService::find_by_number`
//! （`sm-service/src/catalog/movie.rs:513`）。
//!
//! ## 为什么只接了这些
//!
//! `PatchActor` 与影片侧同构（走 `ActorOwnershipGateway`），但演员的 owner 多
//! 一层 `host:javdb`，且它的白名单与取值域是另一套；`SetSubscription` 要
//! `MovieService`；`SubmitDownload` 要整个下载域。一个只读快照错了最多是插件
//! 拿到空值；一个写操作错了会**改坏用户数据**。所以按组接，且每组都要有
//! 真库测试（`tests/plugin_host_integration.rs`）。
//!
//! ## 身份由宿主**分配**，不由插件声明
//!
//! 写操作的 owner 是 `plugin:{id}`（主权网关的字段归属），而契约
//! （`proto/host.proto`，v0.2.0）的 `PatchMovieRequest` 里**没有**「我是谁」
//! 这样的字段 —— 所以「一个共享服务端 + 请求里自报身份」这条路走不通，且
//! 自报本来就不如分配：宿主是**知道**谁连上来的，不是**相信**它说的。
//! [`serve_for`] 为每个插件各起一个能力出口，插件连的那个端点定义了它是谁；
//! [`PluginHostService::new`] 因此**必须**给 `plugin_id`（构造器就要求）。
//!
//! ## 未接线的那 31 个**显式列出**，不用通配实现
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
//!
//! ⚠️ `owners` 是**去重后的 owner 列表**，不是「字段 → owner」的映射
//! （见 `owners_of`）。要判「`is_collection` 这个字段归谁」的插件拿不到
//! 字段级信息 —— 那件事由写入口（主权网关）兜住，不靠插件读快照。
//!
//! ## 快照给出**全部 6 个可写字段**
//!
//! `values` 里除了身份与展示用的列，还把 `PROTECTED_MOVIE_FIELDS` 覆盖的六个
//! 字段都带上：`title` / `summary` / `maker_name` / `director_name` /
//! `is_collection` / `is_blacklisted`。判据是「**可写就得可读**」：判定类插件
//! 要先看现值再决定写不写（例：`is_collection` 已经是 `true` 就不重复写），
//! 读不到就只能盲写。

use std::collections::HashMap;
use std::net::SocketAddr;

use prost_types::value::Kind;
use prost_types::Value as PbValue;
use sm_db::repo::gateway::{FieldPatch, MovieOwnershipGateway};
use sm_db::repo::{ActorRepository, MovieRepository};
use sm_db::{Db, DbError};
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
use sm_service::catalog::subtitle_asset::{SubtitleAssetService, SubtitleImportStatus};
use sm_service::error::ServiceError;
use sm_service::system::config::ConfigService;
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
///
/// 一个实例**绑定一个调用方**（构造时给的 `plugin_id`）—— 见模块文档的
/// 「身份由宿主分配」一节。
pub struct PluginHostService {
    /// 拥有这个端点的插件。写操作的 owner 就是 `plugin:{plugin_id}`。
    plugin_id: String,
    movies: MovieRepository,
    actors: ActorRepository,
    /// 影片受保护字段的**唯一**写入口（乐观并发 + 字段级归属）。
    gateway: MovieOwnershipGateway,
    /// 字幕资产（写侧）。导入要用它：去重要读**内容指纹**，落盘要读配置里的图片根。
    subtitles: SubtitleAssetService,
}

impl PluginHostService {
    /// `plugin_id` 是**必填**：没有身份的写操作会让「这块字段归谁」失去依据。
    ///
    /// `config` 也是必填：字幕落盘的位置（`media.import_image_root_path` 下面的
    /// `<图片根>/movies/<shard>/<番号>/subtitles`）由它决定，宿主不该自己拼路径
    /// （`sm-service::catalog::media_paths` 里那份才是唯一实现）。
    pub fn new(db: &Db, config: &ConfigService, plugin_id: &str) -> Self {
        Self {
            plugin_id: plugin_id.to_owned(),
            movies: MovieRepository::new(db.clone()),
            actors: ActorRepository::new(db.clone()),
            gateway: MovieOwnershipGateway::new(db.clone()),
            subtitles: SubtitleAssetService::new(db, config),
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

    /// 游标分页的影片快照，`id` 升序。
    ///
    /// `limit` 上限 1000（proto 写在字段上的原话）。超出时**收窄**而不报错：
    /// 分页参数由调用方循环消费，收窄只是让它多走一趟，语义不变；报错则把
    /// 「proto 的约定」变成调用方必须记住的知识。
    ///
    /// `next_cursor` 只在**确实还有下一页**时给出：内部多取一条来判，所以
    /// 「取满一页」不会被误判成「后面还有」（那会让插件白跑一趟空查询），
    /// 「最后一页恰好取满」也不会被误判成「到底了」（那会漏数据）。
    async fn list_movies(
        &self,
        request: Request<ListMoviesRequest>,
    ) -> Result<Response<ListMoviesResponse>, Status> {
        let inner = request.into_inner();
        if inner.filters.is_some() {
            // 筛选条件的结构对应上游 `MovieQueryFilters`，还没映射 —— 显式拒，
            // 免得调用方以为「传了筛选」而其实拿到了全库。
            return Err(Status::unimplemented(
                "ListMovies.filters 尚未映射（结构对应 MovieQueryFilters）",
            ));
        }
        let after_id = i32::try_from(inner.after_id)
            .map_err(|_| Status::invalid_argument("after_id 超出范围"))?;
        let limit = i64::from(inner.limit.clamp(1, MAX_LIST_MOVIES_LIMIT));

        // 多要一条：回来的比 limit 多就说明后面还有。
        let mut rows = self
            .movies
            .list_page_after_id(after_id, limit + 1)
            .await
            .map_err(db_status)?;
        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        let next_cursor = if has_more {
            rows.last().map(|movie| i64::from(movie.id))
        } else {
            None
        };

        Ok(Response::new(ListMoviesResponse {
            movies: rows.iter().map(movie_snapshot).collect(),
            next_cursor,
        }))
    }

    /// 插件写影片的**受保护字段**。
    ///
    /// 规则全在主权网关里（`sm-db/src/repo/gateway.rs`，与上游
    /// `movie_ownership_gateway.py` 对应）：`expected_revision` 乐观并发 +
    /// 每个字段「未接管或 owner 是当前插件」，**任一字段不满足则整次零修改**，
    /// 返回 `updated = false`。本方法只做两件事：把 proto 的 `fields` 翻成
    /// [`FieldPatch`]、把 owner 换成**本端点绑定的那个插件**。
    ///
    /// ⚠️ `updated = false` **不是错误**：它同时表示「版本过期」与「字段被别人
    /// 接管」两种情形，proto 里只有一个布尔位（插件据此重新读快照即可）。
    async fn patch_movie(
        &self,
        request: Request<PatchMovieRequest>,
    ) -> Result<Response<PatchMovieResponse>, Status> {
        let inner = request.into_inner();
        let movie_id = i32::try_from(inner.movie_id)
            .map_err(|_| Status::invalid_argument("movie_id 超出范围"))?;
        let patch = field_patch(&inner.fields)?;
        let updated = self
            .gateway
            .patch_plugin(movie_id, &self.plugin_id, &patch, inner.expected_revision)
            .await
            .map_err(db_status)?;
        Ok(Response::new(PatchMovieResponse { updated }))
    }

    /// 把一份字幕交给宿主：查影片 → 校验扩展名 → 内容指纹去重 → 落盘 → 登记。
    ///
    /// 规则全在 [`SubtitleAssetService::import_subtitle_content`] 里（上游逐行对拍），
    /// 这里只做两件事：转调用、把状态翻成 proto 写死的字符串。
    ///
    /// `subtitle_id` 只有 `imported` 才有值；其余三态给 `0` —— proto 的字段不是
    /// optional，而 `0` 在库里不会出现（`id` 从 1 起）。
    async fn import_subtitle(
        &self,
        request: Request<ImportSubtitleRequest>,
    ) -> Result<Response<ImportSubtitleResponse>, Status> {
        let inner = request.into_inner();
        let result = self
            .subtitles
            .import_subtitle_content(
                &inner.movie_number,
                &inner.content,
                &inner.file_name,
                inner.language.as_deref(),
            )
            .await
            .map_err(service_status)?;
        Ok(Response::new(ImportSubtitleResponse {
            status: import_status_name(result.status).to_owned(),
            subtitle_id: i64::from(result.subtitle_id.unwrap_or(0)),
        }))
    }

    unwired! {
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

/// 起一个 `PluginHost` 服务端，返回插件要用的端点串。
///
/// 与插件进程同一个「先占端口、再交给别人 bind」的套路
/// （[`sm_plugins::supervisor::reserve_addr`]）：宿主自己也要一个端口，而端口
/// 分配方式两处一致才不会出现「一边自选、一边注入」的两套语义。
///
/// # 为什么参数是 `plugin_id`
///
/// 这个端点**属于某个插件**：它决定了写操作的 owner（见模块文档）。
/// 组合根为每个启用的插件各起一个（`sm-server/src/lib.rs` 的装配步骤 4a），
/// 于是「插件连的是哪个端点」就是它的身份 —— 不需要请求里自报。
pub async fn serve_for(
    db: &Db,
    config: &ConfigService,
    plugin_id: &str,
) -> Result<String, std::io::Error> {
    let addr: SocketAddr = sm_plugins::supervisor::reserve_addr()?;
    let service = PluginHostService::new(db, config, plugin_id);
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
    // ── `PROTECTED_MOVIE_FIELDS` 的六个可写字段：**可写就得可读** ──
    // 判定类插件要先看现值再决定写不写（`is_collection` 已经是 true 就别重复
    // 写），读不到就只能盲写 —— 那会平白推进 mutation_revision。
    put_str(&mut values, "title", &movie.title);
    put_str(&mut values, "summary", &movie.summary);
    put_opt(&mut values, "maker_name", movie.maker_name.as_deref());
    put_opt(&mut values, "director_name", movie.director_name.as_deref());
    put_flag(&mut values, "is_collection", movie.is_collection);
    put_flag(&mut values, "is_blacklisted", movie.is_blacklisted);
    // `is_subscribed` **不在**可写白名单里（订阅是人工决策），但遍历类插件要靠它
    // 筛「已订阅影片」—— `sakuramedia_subtitlecat` 的定时任务就是先分页遍历、
    // 再按这一位过滤。缺了它插件只会看到「一部都没订阅」而**静默什么都不做**。
    put_flag(&mut values, "is_subscribed", movie.is_subscribed);
    // ── 其余是身份与展示用的列 ──
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

/// `ListMovies.limit` 的上限（proto 写在字段上的原话：「游标分页，上限 1000」）。
const MAX_LIST_MOVIES_LIMIT: i32 = 1000;

/// proto 的 `fields` → [`FieldPatch`]。
///
/// # 字段名为什么要过一遍 `match`
///
/// [`FieldPatch`] 只收 `&'static str` 的字段名 —— 列名是代码里的字面量，没有
/// 任何路径能把外部输入拼进 SQL（网关的模块文档写了为什么）。所以这里把六个
/// 受保护字段**逐个列出**。
///
/// # 未知字段是**报错**，不是静默跳过
///
/// 上游影片侧对白名单外的字段是静默跳过。这里改成明确报错：插件多发一个字段
/// 就整次不生效（fail-closed），比「悄悄少写一个字段、还回报成功」容易发现 ——
/// 后者会让插件以为自己写进去了。
fn field_patch(fields: &HashMap<String, PbValue>) -> Result<FieldPatch, Status> {
    if fields.is_empty() {
        return Err(Status::invalid_argument("fields 不能为空"));
    }

    let mut patch = FieldPatch::new();
    for (name, value) in fields {
        match name.as_str() {
            // 文本字段。`NullValue` 传成 `None`：影片侧的插件写路径不允许写
            // NULL（上游如此），由网关的 `validate_fields` 拒掉 —— 这里不替它做决定。
            "title" => patch.text("title", text_of(name, value)?),
            "summary" => patch.text("summary", text_of(name, value)?),
            "maker_name" => patch.text("maker_name", text_of(name, value)?),
            "director_name" => patch.text("director_name", text_of(name, value)?),
            "is_collection" => patch.flag("is_collection", flag_of(name, value)?),
            "is_blacklisted" => patch.flag("is_blacklisted", flag_of(name, value)?),
            other => {
                return Err(Status::invalid_argument(format!(
                    "字段 {other} 不是受保护字段（可写：title / summary / maker_name / \
                     director_name / is_collection / is_blacklisted）"
                )))
            }
        };
    }
    Ok(patch)
}

/// 取字符串字段；`NullValue` 视为「显式清空」。
fn text_of<'a>(name: &str, value: &'a PbValue) -> Result<Option<&'a str>, Status> {
    match value.kind.as_ref() {
        Some(Kind::StringValue(text)) => Ok(Some(text.as_str())),
        Some(Kind::NullValue(_)) => Ok(None),
        _ => Err(Status::invalid_argument(format!("字段 {name} 期望字符串"))),
    }
}

/// 取布尔字段。
fn flag_of(name: &str, value: &PbValue) -> Result<bool, Status> {
    match value.kind.as_ref() {
        Some(Kind::BoolValue(flag)) => Ok(*flag),
        _ => Err(Status::invalid_argument(format!("字段 {name} 期望布尔值"))),
    }
}

/// `SubtitleImportStatus` → proto 在 `ImportSubtitleResponse.status` 上写死的四个串。
///
/// **跟着 `match` 走，而不是再走一遍 serde**：新增一个状态时编译期就会被逼着处理，
/// 而 `serde(rename_all = "snake_case")` 那条路会悄悄把它序列化成别的字符串发出去。
/// 两边一致由单测 `import_status_strings_match_the_enum_serialization` 钉住。
fn import_status_name(status: SubtitleImportStatus) -> &'static str {
    match status {
        SubtitleImportStatus::Imported => "imported",
        SubtitleImportStatus::Duplicate => "duplicate",
        SubtitleImportStatus::MovieNotFound => "movie_not_found",
        SubtitleImportStatus::InvalidFormat => "invalid_format",
    }
}

/// `ServiceError`（HTTP 状态码 + 响应体）→ gRPC `Status`。
///
/// service 层已经把「哪类错误配哪个状态码」定死了（`sm-service/src/error.rs` 开头
/// 那张表），这里**按它的结论翻译，不重新判类**：
///
/// | service | gRPC |
/// |---|---|
/// | 422 校验失败 | `InvalidArgument` |
/// | 404 不存在 | `NotFound` |
/// | 409 冲突 | `Aborted` |
/// | 502 / 503 上游不可用 | `Unavailable` |
/// | 其余 | `Internal` |
///
/// 消息里带上 `code`：ABI 没有日志通道，插件侧只有这一条线索。
fn service_status(error: ServiceError) -> Status {
    let code = match error.status {
        422 => tonic::Code::InvalidArgument,
        404 => tonic::Code::NotFound,
        409 => tonic::Code::Aborted,
        502 | 503 => tonic::Code::Unavailable,
        _ => tonic::Code::Internal,
    };
    Status::new(code, format!("{}（{}）", error.api.message, error.api.code))
}

/// `DbError` → `Status`。
///
/// 业务拒绝（前缀不在白名单、类型不匹配、值越界）是**调用方**传错了，报
/// `InvalidArgument` 并把它的话原样带出去 —— 那是插件作者唯一能看到的线索。
/// 其余归 `Internal`：连接断了、SQL 写错了，都不是插件能修的。
fn db_status(error: DbError) -> Status {
    match error {
        DbError::Business { reason, .. } => Status::invalid_argument(reason),
        DbError::NotFound { entity, key } => Status::not_found(format!("{entity} {key} 不存在")),
        other => Status::internal(other.to_string()),
    }
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

fn put_flag(values: &mut HashMap<String, prost_types::Value>, key: &str, value: bool) {
    values.insert(
        key.to_owned(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(value)),
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use sm_db::repo::gateway::FieldValue;

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

    // ---------------------------------------------------------- patch 的翻译

    fn text_value(text: &str) -> PbValue {
        PbValue {
            kind: Some(Kind::StringValue(text.to_owned())),
        }
    }

    fn bool_value(flag: bool) -> PbValue {
        PbValue {
            kind: Some(Kind::BoolValue(flag)),
        }
    }

    fn null_value() -> PbValue {
        PbValue {
            kind: Some(Kind::NullValue(0)),
        }
    }

    /// ★ 六个可写字段全部翻得出来，类型一一对上。
    ///
    /// 少一个的后果不是编译失败：那个字段会落进 `other` 分支被当「未知字段」拒，
    /// 于是插件写一个**本来合法**的字段却拿到 `InvalidArgument`。
    #[test]
    fn every_protected_field_is_mapable() {
        let fields = HashMap::from([
            ("title".to_owned(), text_value("标题")),
            ("summary".to_owned(), null_value()),
            ("maker_name".to_owned(), text_value("厂商")),
            ("director_name".to_owned(), text_value("导演")),
            ("is_collection".to_owned(), bool_value(true)),
            ("is_blacklisted".to_owned(), bool_value(false)),
        ]);

        let patch = field_patch(&fields).expect("六个字段都该收");
        assert_eq!(patch.len(), 6);
        // `FieldValue` 没有 `PartialEq`（它的语义是「绑到 SQL 上的值」，比相等
        // 不是它的用途），所以用 `matches!`。
        assert!(matches!(
            patch.get("is_collection"),
            Some(FieldValue::Bool(true))
        ));
        assert!(matches!(
            patch.get("title"),
            Some(FieldValue::Text(Some(title))) if title == "标题"
        ));
        // `NullValue` = 显式清空。影片侧的插件路径不接受它，但**拒在网关**
        // （`validate_fields`），不在这里 —— 这一层只做翻译。
        assert!(matches!(patch.get("summary"), Some(FieldValue::Text(None))));
    }

    /// 空白名单外的字段 → `InvalidArgument`，**不是**静默跳过。
    #[test]
    fn an_unknown_field_is_rejected_by_name() {
        let fields = HashMap::from([
            ("is_collection".to_owned(), bool_value(true)),
            ("score".to_owned(), bool_value(true)),
        ]);
        let status = field_patch(&fields).expect_err("score 不在白名单");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("score"), "{:?}", status.message());
    }

    /// 类型不对 → 指名是哪个字段。
    #[test]
    fn a_wrong_type_names_the_field() {
        for (name, value) in [
            ("is_collection", text_value("true")),
            ("title", bool_value(true)),
        ] {
            let fields = HashMap::from([(name.to_owned(), value)]);
            let status = field_patch(&fields).expect_err("类型不对该拒");
            assert_eq!(status.code(), tonic::Code::InvalidArgument);
            assert!(status.message().contains(name), "{:?}", status.message());
        }
    }

    /// 空 `fields` 一律拒 —— 它只会白跑一条 UPDATE。
    #[test]
    fn empty_fields_are_rejected() {
        let status = field_patch(&HashMap::new()).expect_err("空 patch 该拒");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }

    /// 业务拒绝把网关的话**原样**带出去（那是插件作者唯一的线索）。
    #[test]
    fn a_business_refusal_keeps_the_gateway_message() {
        let status = db_status(DbError::business("Movie", "字段 title 值类型错误"));
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), "字段 title 值类型错误");
    }

    /// 数据库本身的错归 `Internal` —— 插件改不了它。
    #[test]
    fn a_database_failure_is_internal() {
        assert_eq!(
            db_status(DbError::Db(sqlx::Error::RowNotFound)).code(),
            tonic::Code::Internal
        );
    }

    // ---------------------------------------------------------- 字幕导入

    fn service_error(status: u16) -> ServiceError {
        ServiceError {
            status,
            api: Box::new(sm_core::ApiError::new("some_code", "消息")),
        }
    }

    /// ★ 手写的四个串必须与枚举的 serde 输出一致 —— 后者是 `sm-service` 那条单测
    /// 钉住的「与上游枚举一致」的口径。两边分头维护，漂移就在这里红。
    #[test]
    fn import_status_strings_match_the_enum_serialization() {
        for status in [
            SubtitleImportStatus::Imported,
            SubtitleImportStatus::Duplicate,
            SubtitleImportStatus::MovieNotFound,
            SubtitleImportStatus::InvalidFormat,
        ] {
            let serialized = serde_json::to_value(status).expect("枚举能序列化");
            assert_eq!(
                serialized.as_str(),
                Some(import_status_name(status)),
                "{status:?}"
            );
        }
    }

    /// service 的错误码**照它的结论翻译**，不在这里重判。
    #[test]
    fn service_errors_are_translated_by_their_own_status() {
        for (http, expected) in [
            (422, tonic::Code::InvalidArgument),
            (404, tonic::Code::NotFound),
            (409, tonic::Code::Aborted),
            (502, tonic::Code::Unavailable),
            (503, tonic::Code::Unavailable),
            (500, tonic::Code::Internal),
        ] {
            let status = service_status(service_error(http));
            assert_eq!(status.code(), expected, "HTTP {http}");
            assert!(status.message().contains("消息"), "{http}");
            assert!(status.message().contains("some_code"), "带上错误码：{http}");
        }
    }
}
