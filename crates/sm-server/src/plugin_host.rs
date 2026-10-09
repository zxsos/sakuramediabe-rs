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
//! | 影片 | `GetMovie` / `FindMoviesByNumbers` / `ListMovies` / `PatchMovie` | **已接**（直读 + 游标分页 + 主权网关）|
//! | 演员 | `GetActor` / `ListActors` / `PatchActor` | **已接**（游标分页 + 主权网关）|
//! | 字幕 | `ImportSubtitle` | **已接**（转发 `SubtitleAssetService`）|
//! | JavDB 榜单 | `GetJavdbRankNumbers` | **已接**（只读出网，见下面「榜单那条」）|
//! | 榜单写侧 | `SyncRankingSources` / `SyncRankingBoard` | **已接**（宿主驱动抓取 + 整榜替换入库，见 `sm_service::discovery::ranking`）|
//! | 其余 26 个 | | ❌ 未接（见下面「为什么只接了这些」）|
//!
//! ## 演员那组的语义与影片侧**不同**（照抄影片侧会错）
//!
//! 1. **白名单更窄**：演员只有九个可写字段（`PROTECTED_ACTOR_FIELDS`：
//!    `gender` / `birthday` / `height_cm` / `bust_cm` / `waist_cm` / `hips_cm` /
//!    `cup` / `birthplace` / `blood_type`）—— **`name` / `javdb_id` /
//!    `is_subscribed` 不可写**（身份与订阅是人工决策，放开 `javdb_id` 等于能把演员
//!    挂到别的 JavDB 条目上）。
//! 2. **`None` 的语义相反**：影片侧插件路径拒绝清空，演员侧**允许**（`gender` 除外
//!    —— 它没有「未知」态，必须在 `{1,2}` 里）。这条规则在网关的
//!    `validate_actor_fields` 里，本文件不重判。
//! 3. **墓碑要解析**：合并掉的演员行是墓碑，`GetActor` / `PatchActor` 先沿
//!    `merged_into_id` 解析到保留记录（`resolve_canonical`），而 `ListActors`
//!    **不返回**墓碑。三者一致才不会出现「列不出来、但按旧 id 能写到墓碑上」。
//!
//! ## 影片快照里的 `actors`
//!
//! `MovieSnapshot.actors` 现在**真的带演员快照**（每次调用两条批量查询：
//! `movie_actor WHERE movie_id = ANY(…)` → `actor WHERE id = ANY(…)`），同一部影片内按
//! `actor_id` 升序（与上游 `context.py:122-127` 的 `.order_by(movie, actor)` 一致）。
//! 上游 `actor_metadata` 插件靠它算「关联的非合集影片数」来排补全顺序 —— 空数组会让
//! 那个排序退化成「按 id 排」（不报错，但优先级失效）。`tags` 仍是空数组（要 join
//! `movie_tag`，还没接）。
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
//! 剩下的大多要另一套服务：`SetSubscription` 要 `MovieService`、`SubmitDownload` 要
//! 整个下载域、`ListSubtitles` / `ReadSubtitle` 要字幕的**读**侧（导入那条只需要写
//! 侧）。一个只读快照错了最多是插件拿到空值；一个写操作错了会**改坏用户数据**。
//! 所以按组接，且每组都要有真库测试（`tests/plugin_host_integration.rs`）。
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
//! ## 榜单那条：**只读**、且是唯一会出网的 rpc
//!
//! `GetJavdbRankNumbers` 是插件**问宿主要数据**（而不是给宿主交数据），方向与
//! 其余写操作相反，风险也最低 —— 它不改任何用户数据，最坏的结果是一次网络失败。
//! 上游对应 `context.build_javdb_provider(username, password)`（`context.py:1401`）：
//! 出网的 UA / `jdsignature` / 代理策略与登录态都留在宿主，插件只传**它自己的**
//! 账号，宿主不保管。
//!
//! ★ 一个容易写反的语义：**空榜单是成功**（`Ok` + 空数组），不是错误。TOP250 的
//! 历史年份、任何当日无数据的榜都会返回空 —— 把它当失败会让那个榜每天重试。
//! 分类见本文件底部的 `javdb_rank_status`（私有辅助，故不作链接）。
//!
//! ## 未接线的那 28 个**显式列出**，不用通配实现
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
use std::sync::Arc;

use prost_types::value::Kind;
use prost_types::Value as PbValue;
use sm_db::repo::gateway::{
    parse_iso_date_exact, ActorOwnershipGateway, FieldPatch, MovieOwnershipGateway,
};
use sm_db::repo::{ActorRepository, MovieActorRepository, MovieRepository};
use sm_db::{Db, DbError};
use sm_plugin_api::v1::get_javdb_rank_numbers_request;
use sm_plugin_api::v1::plugin_host_server::{PluginHost, PluginHostServer};
use sm_plugin_api::v1::{
    ActorSnapshot, AddVideoItemsRequest, AddVideoItemsResponse, BrowseLibraryRequest,
    BrowseLibraryResponse, CountSubscriptionsByStatusRequest, CountSubscriptionsByStatusResponse,
    CreateNotificationRequest, CreateNotificationResponse, EnqueueImportRequest,
    EnqueueImportResponse, EnsureCollectionRequest, EnsureCollectionResponse, FindByNumbersRequest,
    FindByNumbersResponse, GetActorRequest, GetActorResponse, GetImportStatusRequest,
    GetImportStatusResponse, GetJavdbRankNumbersRequest, GetJavdbRankNumbersResponse,
    GetMovieRequest, GetMovieResponse, GetVideoRequest, GetVideoResponse,
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
    SyncRankingBoardRequest, SyncRankingBoardResponse, SyncRankingSourcesRequest,
    SyncRankingSourcesResponse,
};
use sm_service::catalog::javdb::{JavdbAccount, JavdbProvider, JavdbRankError};
use sm_service::catalog::metadata_source::MetadataSourceError;
use sm_service::catalog::subtitle_asset::{SubtitleAssetService, SubtitleImportStatus};
use sm_service::discovery::ranking::RankingSyncService;
use sm_service::error::ServiceError;
use sm_service::system::config::ConfigService;
use sm_service::system::status::JAVDB_HOST;
use tonic::transport::Server;

use crate::ranking_gateway::RankingSyncSlot;
use tonic::{Request, Response, Status};

/// 生成一串「尚未接线」的 rpc 实现。
///
/// 37 个方法里接了 11 个，剩下 26 个的形状**完全一样**（`unimplemented`）。
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
    /// 演员受保护字段的**唯一**写入口。与影片那个同构，但白名单更窄、`None` 语义
    /// 相反（见模块文档「演员那组」那节）。
    actor_gateway: ActorOwnershipGateway,
    /// 影片 ↔ 演员关联。影片快照要带上演员，批量查靠它（`actor_ids_for_movies`）。
    movie_actors: MovieActorRepository,
    /// 字幕资产（写侧）。导入要用它：去重要读**内容指纹**，落盘要读配置里的图片根。
    subtitles: SubtitleAssetService,
    /// JavDB 榜单客户端（**只读、出网**）。一个服务实例一个 —— 连接池与 TLS
    /// 会话都复用，不要每次 rpc 重建。
    ///
    /// 存 `Result` 而不是 `JavdbProvider`：构造只在 host 为空时失败，而 host 是
    /// 编译期常量 [`JAVDB_HOST`]。「在构造器里 panic」比「在 rpc 里回一个
    /// internal」糟糕得多，所以那个不可能的分支保留成一种**可报告**的错误。
    javdb: Result<JavdbProvider, MetadataSourceError>,
    /// 排行同步服务（写侧）。**延迟填槽** —— 端点比排行源目录先出生，理由见
    /// [`RankingSyncSlot`]。
    rankings: RankingSyncSlot,
}

impl PluginHostService {
    /// `plugin_id` 是**必填**：没有身份的写操作会让「这块字段归谁」失去依据。
    ///
    /// `config` 也是必填：字幕落盘的位置（`media.import_image_root_path` 下面的
    /// `<图片根>/movies/<shard>/<番号>/subtitles`）由它决定，宿主不该自己拼路径
    /// （`sm-service::catalog::media_paths` 里那份才是唯一实现）。
    /// `rankings` 是排行同步服务的**槽**，由组合根在插件加载后填 —— 端点起得比
    /// 排行源目录早，所以不能按值传（见 [`RankingSyncSlot`]）。
    pub fn new(
        db: &Db,
        config: &ConfigService,
        plugin_id: &str,
        rankings: RankingSyncSlot,
    ) -> Self {
        Self {
            plugin_id: plugin_id.to_owned(),
            movies: MovieRepository::new(db.clone()),
            actors: ActorRepository::new(db.clone()),
            gateway: MovieOwnershipGateway::new(db.clone()),
            actor_gateway: ActorOwnershipGateway::new(db.clone()),
            movie_actors: MovieActorRepository::new(db.clone()),
            subtitles: SubtitleAssetService::new(db, config),
            javdb: JavdbProvider::new(JAVDB_HOST),
            rankings,
        }
    }

    /// 取排行同步服务。槽还没填 → `Unavailable`。
    ///
    /// **不退化成「0 个目标」**：那会让「宿主没接上取数能力」表现成「同步成功」。
    fn rankings(&self) -> Result<Arc<RankingSyncService>, Status> {
        self.rankings
            .get()
            .ok_or_else(|| Status::unavailable("排行同步尚不可用：宿主还没接上插件的取数能力"))
    }

    /// 把 JavDB 客户端指到别的 base URL（**测试打桩用**）。
    ///
    /// 与 `JavdbProvider::with_base_url` 是同一个缝：生产把 host 写死在
    /// [`JAVDB_HOST`]（照上游 `metadata/factory.py:15` 硬编码），于是「本机起
    /// 一个假 JavDB」不可能 —— 而那是唯一能覆盖「`success != 1` 不是空榜单」与
    /// 「登录请求带哪些字段」这两个实现级断言的办法。
    pub fn with_javdb_base(mut self, base: &str) -> Self {
        self.javdb = JavdbProvider::with_base_url(base);
        self
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
        let mut actors = self.actors_by_movie(&[movie.id]).await?;
        Ok(Response::new(GetMovieResponse {
            movie: Some(movie_snapshot(
                &movie,
                actors.remove(&movie.id).unwrap_or_default(),
            )),
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
        let mut actors = self
            .actors_by_movie(&found.values().map(|movie| movie.id).collect::<Vec<_>>())
            .await?;
        // 按**请求顺序**输出：调用方按位置对应自己传进来的番号。
        let movies = numbers
            .iter()
            .filter_map(|number| found.get(number))
            .map(|movie| movie_snapshot(movie, actors.remove(&movie.id).unwrap_or_default()))
            .collect();
        Ok(Response::new(FindByNumbersResponse { movies }))
    }

    async fn get_actor(
        &self,
        request: Request<GetActorRequest>,
    ) -> Result<Response<GetActorResponse>, Status> {
        let actor_id = i32::try_from(request.into_inner().actor_id)
            .map_err(|_| Status::invalid_argument("actor_id 超出范围"))?;
        // ★ 沿墓碑解析（上游 `ActorApi.get` 同样如此，`context.py:64`）：插件手里的
        // id 可能是合并**之前**的，直接 `find_by_id` 会把墓碑当成那位演员返回。
        let actor = self
            .actors
            .resolve_canonical(actor_id)
            .await
            .map_err(|_| Status::internal("读取演员失败"))?
            .map(|(actor, _chain)| actor)
            .ok_or_else(|| Status::not_found("演员不存在"))?;
        Ok(Response::new(GetActorResponse {
            actor: Some(actor_snapshot(&actor)),
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

        let mut actors = self
            .actors_by_movie(&rows.iter().map(|movie| movie.id).collect::<Vec<_>>())
            .await?;
        Ok(Response::new(ListMoviesResponse {
            movies: rows
                .iter()
                .map(|movie| movie_snapshot(movie, actors.remove(&movie.id).unwrap_or_default()))
                .collect(),
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

    /// 游标分页的演员快照，`id` 升序，**不含墓碑**（合并掉的演员不在名单里）。
    ///
    /// proto 对演员这组**一句注释都没有**（对比 `ListMoviesRequest` 写了「游标分页，
    /// 上限 1000」），所以语义照上游 `ActorApi.list_page`（`context.py:67-84`）与影片
    /// 那组对齐：`limit` 上限 1000、超出**收窄**；内部多取一条判「还有下一页」。
    async fn list_actors(
        &self,
        request: Request<ListActorsRequest>,
    ) -> Result<Response<ListActorsResponse>, Status> {
        let inner = request.into_inner();
        if inner.filters.is_some() {
            return Err(Status::unimplemented(
                "ListActors.filters 尚未映射（结构对应 ActorQueryFilters）",
            ));
        }
        let after_id = i32::try_from(inner.after_id)
            .map_err(|_| Status::invalid_argument("after_id 超出范围"))?;
        let limit = i64::from(inner.limit.clamp(1, MAX_LIST_ACTORS_LIMIT));

        // 多要一条：回来的比 limit 多就说明后面还有。
        let mut rows = self
            .actors
            .list_page_after_id(after_id, limit + 1)
            .await
            .map_err(db_status)?;
        let has_more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        let next_cursor = if has_more {
            rows.last().map(|actor| i64::from(actor.id))
        } else {
            None
        };

        Ok(Response::new(ListActorsResponse {
            actors: rows.iter().map(actor_snapshot).collect(),
            next_cursor,
        }))
    }

    /// 插件写演员资料字段。
    ///
    /// 与 `patch_movie` 同形（翻译 + 交给主权网关），三处不同都在网关里：白名单是
    /// 演员那九个、`None` 允许清空（`gender` 除外）、以及**先解析墓碑** —— 合并掉的
    /// 演员行是墓碑，把资料写到它上面等于写进一条已经作废的记录。
    ///
    /// `updated = false` 有三种成因（版本过期 / 字段被别人接管 / 目标演员没了），
    /// 都是**结果**不是错误 —— 插件据此重新读快照即可。
    async fn patch_actor(
        &self,
        request: Request<PatchActorRequest>,
    ) -> Result<Response<PatchActorResponse>, Status> {
        let inner = request.into_inner();
        let actor_id = i32::try_from(inner.actor_id)
            .map_err(|_| Status::invalid_argument("actor_id 超出范围"))?;
        let patch = actor_field_patch(&inner.fields)?;
        let Some((actor, _chain)) = self
            .actors
            .resolve_canonical(actor_id)
            .await
            .map_err(db_status)?
        else {
            // 上游 `ActorApi.patch` 在这个分支直接返回 `False`（`context.py:86-95`）。
            return Ok(Response::new(PatchActorResponse { updated: false }));
        };
        let updated = self
            .actor_gateway
            .patch_plugin(actor.id, &self.plugin_id, &patch, inner.expected_revision)
            .await
            .map_err(db_status)?;
        Ok(Response::new(PatchActorResponse { updated }))
    }

    /// JavDB 榜单番号（只读、**出网**）。上游 `context.build_javdb_provider`
    /// （`context.py:1401-1409`）的 gRPC 形态。
    ///
    /// # 为什么这一条在宿主而不是插件里
    ///
    /// 出网那三样（UA、`jdsignature`、代理策略）与登录态本来就归宿主一处管，
    /// 插件只提供**账号** —— 它自己配置里的那两个字段，每次调用透传进来，
    /// **宿主不保管**。这与「排行同步由宿主编排、插件只答 `FetchRanking`」是
    /// 同一条分工。
    ///
    /// # 空榜单是**成功**，不是错误
    ///
    /// 见 `javdb_rank_status`：`Ok` + 空数组表示「这个榜此刻没有条目」，
    /// 它必须与「请求失败」分开 —— 混在一起会让历史年份的 TOP250 每天重试。
    async fn get_javdb_rank_numbers(
        &self,
        request: Request<GetJavdbRankNumbersRequest>,
    ) -> Result<Response<GetJavdbRankNumbersResponse>, Status> {
        let inner = request.into_inner();
        // `oneof` 未设置是**调用方的 bug**：没有它连「打哪个端点」都不知道。
        let query = inner.query.ok_or_else(|| {
            Status::invalid_argument("query 必须指定 playback / video_type_rank / top 之一")
        })?;
        let provider = self
            .javdb
            .as_ref()
            .map_err(|error| Status::internal(format!("构造 JavDB 客户端失败：{error:?}")))?;
        // 账号按需构造：只有 TOP250 会读它，另两个榜拿到也不用。
        let account = JavdbAccount {
            username: inner.username.unwrap_or_default(),
            password: inner.password.unwrap_or_default(),
        };
        let numbers = match query {
            get_javdb_rank_numbers_request::Query::Playback(playback) => {
                provider
                    .playback_rank_numbers(&playback.filter_by, &playback.period)
                    .await
            }
            get_javdb_rank_numbers_request::Query::VideoTypeRank(rank) => {
                provider.rank_numbers(&rank.video_type, &rank.period).await
            }
            get_javdb_rank_numbers_request::Query::Top(top) => {
                provider
                    .top_numbers(&account, &top.top_type, &top.type_value, top.max_pages)
                    .await
            }
        }
        .map_err(javdb_rank_status)?;
        Ok(Response::new(GetJavdbRankNumbersResponse {
            movie_numbers: numbers,
        }))
    }

    /// ★ 同步**本插件声明的**全部排行源（上游
    /// `PluginContext.sync_ranking_sources`，`context.py:1476-1486`）。
    ///
    /// # 越界由宿主挡：插件只能同步自己的源
    ///
    /// 上游先按 `RANKING_SOURCE_OWNERS` 过滤出归属本插件的 `source_keys`，**一个
    /// 都没有就抛 `RuntimeError`**。这里照做并回 `FailedPrecondition`。
    /// 空集合**是错误**，不是「同步了 0 个」—— 后者会让「插件把排行源注册丢了」
    /// 表现成「同步成功、0 个目标」，那是最难查的一种故障。
    ///
    /// # 单个目标失败不中断整批
    ///
    /// 一个榜挂了（JavDB 抽风之类）后面那些照跑，`failed_targets` 记账 ——
    /// 这是上游的 per-target `try/except`。
    async fn sync_ranking_sources(
        &self,
        _request: Request<SyncRankingSourcesRequest>,
    ) -> Result<Response<SyncRankingSourcesResponse>, Status> {
        let service = self.rankings()?;
        let owned = service.sources().source_keys_owned_by(&self.plugin_id);
        if owned.is_empty() {
            return Err(Status::failed_precondition(
                "本插件没有声明任何排行源：capability discovery.ranking_source \
                 未注册，或注册载荷已被拒",
            ));
        }

        let stats = service
            .sync_all_rankings(Some(&owned))
            .await
            .map_err(service_status)?;

        // 五个 `i64` 计数在 ABI 上是 `int32`：夹到 `i32::MAX` 而不是回绕。
        // 100 个榜 × 每榜几百条够不到这个量级，但静默回绕会给出**负数**计数。
        let clamp = |value: i64| i32::try_from(value).unwrap_or(i32::MAX);
        Ok(Response::new(SyncRankingSourcesResponse {
            synced_count: clamp(stats.success_targets),
            total_targets: clamp(stats.total_targets),
            failed_targets: clamp(stats.failed_targets),
            fetched_numbers: clamp(stats.fetched_numbers),
            imported_movies: clamp(stats.imported_movies),
            local_hit_movies: clamp(stats.local_hit_movies),
            skipped_movies: clamp(stats.skipped_movies),
            stored_items: clamp(stats.stored_items),
        }))
    }

    /// ★ 手动同步单个榜单（上游 `PluginContext.sync_ranking_board`，
    /// `context.py:1488-1503`）。
    ///
    /// `source_key` **必须是本插件自己的** —— 上游对别人的源直接 `ValueError`。
    /// 这里回 `PermissionDenied`：插件改自己的映射能修好它，重试一万次也不会好。
    async fn sync_ranking_board(
        &self,
        request: Request<SyncRankingBoardRequest>,
    ) -> Result<Response<SyncRankingBoardResponse>, Status> {
        let request = request.into_inner();
        let service = self.rankings()?;
        // 归属判定与 `sync_ranking_sources` 同一把尺子（都问目录要 owner）。
        let owner = service
            .sources()
            .require_definition(&request.source_key)
            .map_err(service_status)?
            .owner_plugin_id
            .clone();
        if owner != self.plugin_id {
            return Err(Status::permission_denied(format!(
                "排行源 {} 属于插件 {owner}，不属于 {}",
                request.source_key, self.plugin_id
            )));
        }

        // 周期给空串时**不是**回退到 `default_period`：由
        // `RankingCatalogService::resolve_period` 按上游语义裁决（有周期集合的
        // 榜单要求显式给，单期榜只接受空串）。
        let board = service
            .sync_board_period(&request.source_key, &request.board_key, &request.period)
            .await
            .map_err(service_status)?;

        let clamp = |value: i64| i32::try_from(value).unwrap_or(i32::MAX);
        Ok(Response::new(SyncRankingBoardResponse {
            source_key: board.source_key,
            board_key: board.board_key,
            period: board.period,
            fetched_numbers: clamp(board.fetched_numbers),
            imported_movies: clamp(board.imported_movies),
            local_hit_movies: clamp(board.local_hit_movies),
            skipped_movies: clamp(board.skipped_movies),
            stored_items: clamp(board.stored_items),
        }))
    }

    unwired! {
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
    }
}

/// 私有辅助（不属于 `PluginHost` 契约）。
impl PluginHostService {
    /// 一组影片各自的演员快照，同一部影片内按 `actor_id` 升序。
    ///
    /// **两次批量查询**（关联表 → 演员表），不是每部一次 —— 一页可能上千部影片。
    /// 上游 `MovieApi._to_snapshots` 也是这个形状（`context.py:112-153`）：先按
    /// `movie_id` 批量取关联，再一次 join 出演员，最后逐部归组。
    ///
    /// 关联表那步的排序是 `(movie_id, actor_id)`，所以归组后的顺序天然是 actor_id
    /// 升序 —— 与上游 `.order_by(movie, actor)` 一致。
    async fn actors_by_movie(
        &self,
        movie_ids: &[i32],
    ) -> Result<HashMap<i32, Vec<ActorSnapshot>>, Status> {
        let links = self
            .movie_actors
            .actor_ids_for_movies(movie_ids)
            .await
            .map_err(db_status)?;
        let actor_ids: Vec<i32> = links.iter().map(|(_, actor_id)| *actor_id).collect();
        let by_id: HashMap<i32, ActorSnapshot> = self
            .actors
            .find_by_ids(&actor_ids)
            .await
            .map_err(db_status)?
            .iter()
            .map(|actor| (actor.id, actor_snapshot(actor)))
            .collect();

        let mut grouped: HashMap<i32, Vec<ActorSnapshot>> = HashMap::new();
        for (movie_id, actor_id) in links {
            if let Some(snapshot) = by_id.get(&actor_id) {
                grouped.entry(movie_id).or_default().push(snapshot.clone());
            }
        }
        Ok(grouped)
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
///
/// # `rankings` 是**后填的槽**
///
/// 这个函数在装配步骤 4a 跑，而排行源目录要等 4b 加载完插件才有 —— 见
/// [`RankingSyncSlot`]。没填之前那两个 rpc 回 `Unavailable`，不会假成功。
pub async fn serve_for(
    db: &Db,
    config: &ConfigService,
    plugin_id: &str,
    rankings: RankingSyncSlot,
) -> Result<String, std::io::Error> {
    serve_for_with(db, config, plugin_id, rankings, None).await
}

/// 同 [`serve_for`]，外加一个**打桩用的** JavDB 基地址。
///
/// `javdb_base` 只给测试用：指到回环上的假 JavDB 上，免得集成测试去真连
/// javdb.com（`JavdbProvider::with_base_url` 的存在理由与它相同）。生产传
/// `None`，走编译期常量 `JAVDB_HOST`。
///
/// 端到端冒烟需要这条缝：那条链路上**插件进程内部**才拿得到 JavDB 客户端
/// （插件 → 宿主 `GetJavdbRankNumbers`），所以打桩点只能在宿主这边。
pub async fn serve_for_with(
    db: &Db,
    config: &ConfigService,
    plugin_id: &str,
    rankings: RankingSyncSlot,
    javdb_base: Option<&str>,
) -> Result<String, std::io::Error> {
    let addr: SocketAddr = sm_plugins::supervisor::reserve_addr()?;
    let service = match javdb_base {
        Some(base) => PluginHostService::new(db, config, plugin_id, rankings).with_javdb_base(base),
        None => PluginHostService::new(db, config, plugin_id, rankings),
    };
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
///
/// `actors` 由调用方**批量**取好传进来（[`PluginHostService::actors_by_movie`]）——
/// 一页可能有上千部影片，在映射闭包里逐部查库就是 N+1。
fn movie_snapshot(movie: &sm_db::Movie, actors: Vec<ActorSnapshot>) -> MovieSnapshot {
    MovieSnapshot {
        movie_id: i64::from(movie.id),
        revision: movie.mutation_revision,
        values: movie_values(movie),
        owners: owners_of(&movie.field_owners),
        actors,
        // `tags` 仍要 join `movie_tag`，还没接 —— **空数组**表示「这次没带」，
        // 不是「这部片子没有标签」。
        tags: Vec::new(),
    }
}

/// 演员快照。三个入口共用：`GetActor` / `ListActors` / 影片快照里的演员。
fn actor_snapshot(actor: &sm_db::Actor) -> ActorSnapshot {
    ActorSnapshot {
        actor_id: i64::from(actor.id),
        revision: actor.mutation_revision,
        values: actor_values(actor),
        owners: owners_of(&actor.field_owners),
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
    // ── 身份（只读）──
    put_str(&mut values, "name", &actor.name);
    put_str(&mut values, "alias_name", &actor.alias_name);
    put_str(&mut values, "javdb_id", &actor.javdb_id);
    put_number(&mut values, "javdb_type", f64::from(actor.javdb_type));
    // ── 订阅位（人工决策，不可写；遍历类插件要按它筛）──
    put_flag(&mut values, "is_subscribed", actor.is_subscribed);
    // ── 九个可写资料字段：**可写就得可读** ──
    // 上游 `actor_metadata` 插件先读现值算「缺哪些」，再决定写什么；读不到它只能
    // 盲写 —— 那会平白推进 `mutation_revision`。可选字段为空时**不进快照**
    // （`put_opt` / `put_opt_int` 的既定语义：缺失就是空）。
    put_number(&mut values, "gender", f64::from(actor.gender));
    put_opt(
        &mut values,
        "birthday",
        actor.birthday.map(|date| date.to_string()).as_deref(),
    );
    put_opt_int(&mut values, "height_cm", actor.height_cm);
    put_opt_int(&mut values, "bust_cm", actor.bust_cm);
    put_opt_int(&mut values, "waist_cm", actor.waist_cm);
    put_opt_int(&mut values, "hips_cm", actor.hips_cm);
    put_opt(&mut values, "cup", actor.cup.as_deref());
    put_opt(&mut values, "birthplace", actor.birthplace.as_deref());
    put_opt(&mut values, "blood_type", actor.blood_type.as_deref());
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

/// `ListActors.limit` 的上限。proto 对演员这组没写注释，这里取与影片侧相同的 1000
/// —— 上游 `ActorApi.list_page` 也是这个上界（`context.py:67-84`）。
const MAX_LIST_ACTORS_LIMIT: i32 = 1000;

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

/// proto 的 `fields` → [`FieldPatch`]（**演员版**）。
///
/// 与影片版同一个套路（字段名逐个 `match`、白名单外报错），差别是这九个字段分三类：
///
/// | 类 | 字段 | 收什么 |
/// |---|---|---|
/// | 整数 | `gender` / `height_cm` / `bust_cm` / `waist_cm` / `hips_cm` | `NumberValue`，**必须是整数** |
/// | 日期 | `birthday` | `StringValue`，必须严格 `YYYY-MM-DD` |
/// | 文本 | `cup` / `birthplace` / `blood_type` | `StringValue`；`NullValue` = 清空 |
///
/// 白名单外**报错**（fail-closed）。网关对演员侧也是直接报错（影片侧会静默跳过），
/// 所以两边一致 —— 演员字段少，传错更可能是代码写错。
fn actor_field_patch(fields: &HashMap<String, PbValue>) -> Result<FieldPatch, Status> {
    if fields.is_empty() {
        return Err(Status::invalid_argument("fields 不能为空"));
    }

    let mut patch = FieldPatch::new();
    for (name, value) in fields {
        match name.as_str() {
            "gender" => patch.int("gender", int_of(name, value)?),
            "height_cm" => patch.int("height_cm", int_of(name, value)?),
            "bust_cm" => patch.int("bust_cm", int_of(name, value)?),
            "waist_cm" => patch.int("waist_cm", int_of(name, value)?),
            "hips_cm" => patch.int("hips_cm", int_of(name, value)?),
            "birthday" => patch.date("birthday", date_of(name, value)?),
            "cup" => patch.text("cup", text_of(name, value)?),
            "birthplace" => patch.text("birthplace", text_of(name, value)?),
            "blood_type" => patch.text("blood_type", text_of(name, value)?),
            other => {
                return Err(Status::invalid_argument(format!(
                    "字段 {other} 不是演员资料字段（可写：gender / birthday / height_cm / \
                     bust_cm / waist_cm / hips_cm / cup / birthplace / blood_type）"
                )))
            }
        };
    }
    Ok(patch)
}

/// 取整数字段。`NullValue` = 「显式清空」（`gender` 的清空会被网关拒 —— 它没有
/// 「未知」态）。
///
/// 小数一律拒：`160.5` 厘米不是「四舍五入一下就行」，是调用方把字段搞错了。
fn int_of(name: &str, value: &PbValue) -> Result<Option<i32>, Status> {
    match value.kind.as_ref() {
        Some(Kind::NumberValue(number)) if number.is_finite() && number.fract() == 0.0 => {
            i32::try_from(*number as i64)
                .map(Some)
                .map_err(|_| Status::invalid_argument(format!("字段 {name} 超出 i32 范围")))
        }
        Some(Kind::NullValue(_)) => Ok(None),
        _ => Err(Status::invalid_argument(format!("字段 {name} 期望整数"))),
    }
}

/// 取日期字段。**严格** `YYYY-MM-DD` —— `2020-1-1` 会被拒（网关还会再拒一次，
/// 这里拒是为了让错误消息带上字段名）。
fn date_of(name: &str, value: &PbValue) -> Result<Option<chrono::NaiveDate>, Status> {
    match value.kind.as_ref() {
        Some(Kind::StringValue(text)) => parse_iso_date_exact(text)
            .map(Some)
            .map_err(|error| Status::invalid_argument(format!("字段 {name}：{error}"))),
        Some(Kind::NullValue(_)) => Ok(None),
        _ => Err(Status::invalid_argument(format!(
            "字段 {name} 期望 YYYY-MM-DD 字符串"
        ))),
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

/// `JavdbRankError` → `Status`。
///
/// 四类分开不是为了好看 —— 插件要据此判**能不能重试**：
///
/// | 类别 | 码 | 插件该怎么办 |
/// |---|---|---|
/// | 参数不在白名单 | `InvalidArgument` | 改自己的映射（重试一万次也不会好）|
/// | 需要登录而没带账号 | `FailedPrecondition` | 先配账号 / 跳过这个榜 |
/// | 账号带了但被拒 | `Unauthenticated` | 账号错了，别重试 |
/// | 网络 / 非 2xx / `success != 1` | `Unavailable` | 留到下一轮 |
///
/// ★ 注意这张表里**没有**「榜单是空的」—— 那是 `Ok` + 空数组。把空榜单也归到
/// `Unavailable` 会让历史年份的 TOP250 永远在重试。
fn javdb_rank_status(error: JavdbRankError) -> Status {
    match error {
        JavdbRankError::Unsupported(detail) => Status::invalid_argument(detail),
        JavdbRankError::AccountRequired => Status::failed_precondition(
            "该榜单需要登录，而请求里没有带 JavDB 账号（username/password）",
        ),
        JavdbRankError::Auth(detail) => Status::unauthenticated(detail),
        JavdbRankError::Request(detail) => Status::unavailable(detail),
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

/// 可选的整数字段：`None` 就不进快照（与 [`put_opt`] 同一语义）。
///
/// 演员的身高/三围是 `Option<i32>`，而 [`put_number`] 收 `f64` —— 用
/// `unwrap_or(0.0)` 会把「没填」变成 `0`，而上游插件把 `0` 当**有效值**看
/// （`is_empty_field_value` 只对 `gender` 把 0 当空）。
fn put_opt_int(values: &mut HashMap<String, prost_types::Value>, key: &str, value: Option<i32>) {
    if let Some(number) = value {
        put_number(values, key, f64::from(number));
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

    // ---------------------------------------------------------- 演员 patch 的翻译

    fn number_value(number: f64) -> PbValue {
        PbValue {
            kind: Some(Kind::NumberValue(number)),
        }
    }

    /// ★ 九个可写字段全部翻得出来，三类类型都对上。
    ///
    /// 少一个的后果与影片侧一样：那个字段会落进 `other` 被当「未知字段」拒，于是
    /// 插件写一个**本来合法**的字段却拿到 `InvalidArgument`。
    #[test]
    fn every_actor_profile_field_is_mapable() {
        let fields = HashMap::from([
            ("gender".to_owned(), number_value(1.0)),
            ("height_cm".to_owned(), number_value(160.0)),
            ("bust_cm".to_owned(), number_value(88.0)),
            ("waist_cm".to_owned(), number_value(58.0)),
            ("hips_cm".to_owned(), number_value(86.0)),
            ("birthday".to_owned(), text_value("1996-03-14")),
            ("cup".to_owned(), text_value("D")),
            ("birthplace".to_owned(), text_value("东京")),
            ("blood_type".to_owned(), null_value()),
        ]);

        let patch = actor_field_patch(&fields).expect("九个字段都该收");
        assert_eq!(patch.len(), 9);
        assert!(matches!(
            patch.get("gender"),
            Some(FieldValue::Int(Some(1)))
        ));
        assert!(matches!(
            patch.get("birthday"),
            Some(FieldValue::Date(Some(_)))
        ));
        // `NullValue` = 显式清空（`gender` 的清空会被网关拒，不在这里）
        assert!(matches!(
            patch.get("blood_type"),
            Some(FieldValue::Text(None))
        ));
    }

    /// ★ 身份与订阅字段**不可写** —— 报错，不是静默跳过。
    ///
    /// 放开 `javdb_id` 等于能把演员挂到别的 JavDB 条目上；`is_subscribed` 是人工
    /// 决策。两者都在网关的白名单外，这里提前拦是为了让错误消息带去字段名。
    #[test]
    fn identity_and_subscription_fields_are_rejected() {
        for name in [
            "name",
            "alias_name",
            "javdb_id",
            "javdb_type",
            "is_subscribed",
            "profile_image_id",
        ] {
            let value = if name == "is_subscribed" {
                bool_value(true)
            } else {
                text_value("x")
            };
            let status = actor_field_patch(&HashMap::from([(name.to_owned(), value)]))
                .expect_err("身份与订阅字段该被拒");
            assert_eq!(status.code(), tonic::Code::InvalidArgument, "{name}");
            assert!(status.message().contains(name), "{}", status.message());
        }
    }

    /// 小数、非字符串日期、松动日期都拒，且**指名是哪个字段**。
    #[test]
    fn fractional_numbers_and_loose_dates_are_rejected() {
        for (name, value) in [
            ("height_cm", number_value(160.5)),
            ("gender", text_value("1")),
            ("birthday", text_value("2020-1-1")),
            ("birthday", number_value(20_200_101.0)),
            ("cup", number_value(4.0)),
        ] {
            let status =
                actor_field_patch(&HashMap::from([(name.to_owned(), value)])).expect_err("该拒");
            assert_eq!(status.code(), tonic::Code::InvalidArgument, "{name}");
            assert!(status.message().contains(name), "{}", status.message());
        }
    }

    /// `gender` 清空在**网关**被拒（它没有「未知」态）—— 本层只负责翻译成 `None`。
    #[test]
    fn a_null_gender_is_translated_for_the_gateway_to_reject() {
        let patch = actor_field_patch(&HashMap::from([("gender".to_owned(), null_value())]))
            .expect("翻译层收下");
        assert!(matches!(patch.get("gender"), Some(FieldValue::Int(None))));
    }

    /// 空 `fields` 一律拒 —— 它只会白跑一条 UPDATE。
    #[test]
    fn empty_actor_fields_are_rejected() {
        let status = actor_field_patch(&HashMap::new()).expect_err("空 patch 该拒");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
    }
}
