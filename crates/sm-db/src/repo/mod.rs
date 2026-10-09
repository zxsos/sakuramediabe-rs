//! 数据访问层。
//!
//! # 为什么不用 `sqlx::query!` 宏
//!
//! `sqlx` 开了 `macros` feature，但 `query!` 需要**编译期**数据库连接。
//! 本机有 PostgreSQL、CI 没有 —— 用宏会让 CI 直接编译失败。
//!
//! 所以全部走 `sqlx::query_as()` + `#[derive(sqlx::FromRow)]`（运行时解析）。
//! 代价是失去编译期列名校验，补偿是两道验证：
//!
//! - **L1** `parity/compare_schema.py` —— Rust 结构体与 Peewee 模型逐列比对
//! - **L2** 集成测试在真实 PG 上读写 —— 任何不匹配在运行时会暴露
//!
//! 这个选择还有一个副作用：动态 SET 子句必须在运行时按类型分派 bind，
//! 见 [`movie`] 里的 `bind_value`。
//!
//! # 三张样板表各自代表一类难例
//!
//! | 仓储 | 难例 | 强制手段 |
//! |---|---|---|
//! | [`movie`] | CHECK 约束 + 字段主权 | 预判 + 护栏 |
//! | [`media`] | XOR 归属不变量 | 仓储层拦截（schema 里没有这个 CHECK） |
//! | [`download`] | 两个独立状态机 | 分离的 setter，形状上无法混传 |
//! | [`user`] | 令牌轮换的原子性 | 事务 + `FOR UPDATE`（本 crate 第一个事务） |
//! | [`task`] | 队列互斥 + 租约 | `SKIP LOCKED` 领取，终态释放 `mutex_key` |
//! | [`playback`] | 三个不同形状的唯一索引 | 各表分别 upsert / 允许重复 |
//!
//! # 覆盖范围：40 张表**全部**有仓储
//!
//! 剩下的是两个**不属于表清单**的缺口。
//!
//! | 缺口 | 阻塞了什么 |
//! |---|---|
//! | [`crate::catalog::actor::Actor`] 的**字段主权网关**缺失 | [`actor::ActorRepository`] 已能读写，但 9 个受保护字段没有 `MovieOwnershipGateway` 那样的受控入口 —— 插件能绕过归属直接写。`UnitOfWork::merge_actors` 也等它 |
//! | `Movie.subscription_search_*` 9 列无方法 | 这是**第二个重试状态机**（与 `download_task` 的双状态机同构），但既没有「列出到期任务」也没有「记录一次尝试」。注意 [`movie::MovieRepository::list_by_subscription_state`] 过滤的是 `is_subscribed`，与这 9 列无关 |
//!
//! # 全表覆盖之后，接下来不是加表
//!
//! 剩下的工作不在 `sm-db` 的**表清单**里，而在两处：一是上面那两个缺口，
//! 二是上层 —— [`sm-service`](../../sm_service/index.html) 对应上游 114 个
//! service 文件（25,174 行，占上游代码量的 57%），目前是 1 行占位。
//!
//! 值得记的是：这批仓储**每一张都配了集成测试**，而每一批测试都挖出了
//! 缺陷（占位符错位、依赖不存在的 DEFAULT、`count` 与 `items` 过滤不一致、
//! 上游 `primary_key` kwarg 被解析器丢掉导致 DDL 少了主键）。「有仓储」与
//! 「仓储正确」是两件事，前者不带来后者。
//!
//! # 三条链已打通
//!
//! ```text
//! 传输链
//! media_library ── download_client ── download_task
//!                        ↑
//!                  indexer_download_client ── indexer
//!
//! 合集链（JAV 三条 + 非 JAV 两条）
//! playlist ── playlist_movie ── movie
//! moment_collection ── moment_collection_item ── media_point
//! clip_collection ── clip_collection_item ── media_clip
//! video_collection ── video_collection_item ── video_item ── media
//!
//! 发现链
//! ranking_item ── movie                  （榜单，rank 在**榜单内**唯一）
//! daily_recommendation_item ── movie      （推荐，rank **全表**唯一）
//! moment_recommendation ── media_thumbnail
//! image_search_session ── image_search_index_state（单例，判断兼容）
//!
//! 资产
//! image ──（封面、时刻配图、缩略图都指向它）
//! ```
//!
//! [`transfer`] 四张表、[`submission`]（提交历史）、[`collection`] 六张、
//! [`video_collection`] 两张、[`recommendation`] 两张、[`discovery`]
//! 三张、[`image`] 与 [`video_item`] 都已落地，且**都有集成测试**。
//! 剩下的 4 张里没有这几条链了。
//!
//! # 分页：11 个 list 方法已覆盖，3 个刻意不分页
//!
//! | 方法 | 为什么不分页 |
//! |---|---|
//! | [`media::MediaRepository::find_by_file_hash`] | 去重检查，调用方要完整答案；分页会让它拿到不完整结论而误判「没有重复」 |
//! | [`media::MediaRepository::list_pending_thumbnails`] | worker 循环的队列扫描，语义是「给我 N 条待办」 |
//! | [`task::BackgroundTaskRunRepository::list_claimable`] | 同上 |
//!
//! 区分标准是**调用方是人还是 worker**。人看列表需要翻页与总数，
//! worker 循环需要「下一批待办」—— 给它 `page=1` 只会让它反复取第一页。
//!
//! # 删除策略：已从上游确认，无需再问
//!
//! 上游 `src/model/` 里**没有软删除**：无 `deleted` 字段，
//! `TimestampedMixin` 也不含删除状态。全部是硬删。
//!
//! 46 个外键**全部显式声明** `on_delete`，零个依赖数据库默认：
//!
//! | 行为 | 数量 | 典型 |
//! |---|---|---|
//! | `CASCADE` | 30 | `movie_actor`、`movie_tag`、`movie_plot_image`、`subtitle` 随 `movie` 级联 |
//! | `SET NULL` | 15 | `movie.cover_image_id` / `series_id`；`media_point.media_id` 置空但快照列保留 |
//! | `RESTRICT` | 1 | 仅 `media_point.image_id` —— 有引用时禁止删图 |
//!
//! # `Movie` 与 `Actor` 不可删
//!
//! 全仓库搜不到 `Movie.delete` 或 `movie.delete_instance` 的调用点。
//! 业务上的「移除一部影片」实际是**删关联行**
//! （`catalog_import_service.py` 删 `MovieActor` / `MovieTag` / `MoviePlotImage`），
//! `movie` 行本身留着 —— 因为大量列记录的是**采集到的事实**而非用户意图。
//!
//! 所以仓储层**刻意不提供** `MovieRepository::delete`。将来若出现真实
//! 删除需求，那是行为变更，应先确认上游是否同步改。
//!
//! # 组合写入：已有出口
//!
//! [`UnitOfWork`] 按**动词**暴露用例，每个方法内部编排多个仓储的 `_in`
//! 变体并共享一个事务。已落地三个：
//!
//! | 用例 | 跨越的表 |
//! |---|---|
//! | `generate_thumbnail` | 写 `media_thumbnail` + 推进 `media` 状态机 |
//! | `import_movie` | 影片行 + 标签 upsert + 演员关联 + 标签关联 |
//!
//! 参与者用 `Ctx` 接入事务；方法成对提供（`insert` / `insert_in`），
//! 共用一个私有实现，所以事务内外**不可能出现两套逻辑**。
//!
//! 仍缺的是演员合并 —— 它要搬运影片关联、合并别名、合并订阅、
//! 填空受保护字段、搬运头像、打墓碑并压平链，共 6 步，
//! 且需要 `Actor` 的字段主权网关先就位。
//!
//! [`Actor`]: crate::catalog::actor::Actor
//! [`MediaLibrary`]: crate::playback::media::MediaLibrary
//!
//! # 两个尚未确认的问题
//!
//! 落地对应仓储前必须先查清，否则第二个批次一定会撞上：
//!
//! 1. `daily_recommendation_item` / `moment_recommendation` 的 `rank`
//!    是**全表唯一**。每次生成前必须清空重写，否则第二批撞唯一约束。
//!    `discovery/rankings.rs` 的模块注释自己写着「如果实际行为是累积保留，
//!    那 schema 与 service 就不一致，需要先查清」—— 至今没查。
//! 2. `schema_migration` 由谁写入。`lib.rs` 声明本 crate 不引入迁移框架，
//!    但模型注释描述了「启动时把已应用记录与代码里的迁移列表比对」的流程。
//!    两者只能有一个成立。
//!
//! 上游 Python 侧不在本仓库内，CI 里 clone。这些问题需要查上游才能定论。

pub mod actor;
pub mod asset;
pub mod collection;
pub mod ctx;
pub mod discovery;
pub mod download;
pub mod gateway;
pub mod image;
pub mod library;
pub mod media;
pub mod movie;
pub mod playback;
pub mod recommendation;
pub mod submission;
pub mod subtitle;
pub mod task;
pub mod transfer;
pub mod user;
pub mod video_collection;
pub mod video_item;

pub use actor::{ActorRepository, NewActor, SyncState};
pub use asset::{MovieActorRepository, MovieTagRepository, TagRepository};
pub use collection::{
    ClipCollectionItemRepository, ClipCollectionRepository, MomentCollectionItemRepository,
    MomentCollectionRepository, NewCollection, PlaylistMovieRepository, PlaylistRepository,
};
pub use ctx::{Ctx, CtxConnection, GeneratedThumbnail, UnitOfWork};
pub use discovery::{
    ImageSearchIndexStateRepository, ImageSearchSessionRepository, NewImageSearchSession,
    NewRankingItem, RankingItemRepository,
};
pub use download::{DownloadTaskRepository, NewDownloadTask};
pub use gateway::{FieldCodec, FieldPatch, FieldValue, MovieOwnershipGateway};
pub use image::{ImageRepository, NewImage};
pub use library::{MediaLibraryRepository, NewMediaLibrary};
pub use media::{MediaRepository, NewMedia};
pub use movie::{MovieRepository, MovieSeriesRepository, NewMovie, SubscriptionState};
pub use playback::{
    MediaClipRepository, MediaPointRepository, MediaProgressRepository, MediaThumbnailRepository,
    NewMediaClip,
};
pub use recommendation::{
    DailyRecommendationItemRepository, MomentRecommendationRepository, NewDailyRecommendation,
    NewMomentRecommendation,
};
pub use submission::{DownloadSubmissionRepository, NewSubmissionRecord};
pub use subtitle::{
    MoviePlotImageRepository, NewNotification, NewSubtitle, SchemaMigrationRepository,
    SubtitleRepository, SystemNotificationRepository,
};
pub use task::{BackgroundTaskRunRepository, ClaimedTask, NewTaskRun, TaskOutcome, TaskProgress};
pub use transfer::{
    DownloadClientRepository, DownloadResourceBlacklistRepository, IndexerDownloadClientRepository,
    IndexerRepository, NewDownloadClient, NewIndexer,
};
pub use user::{NewRefreshToken, NewUser, Rotation, UserRefreshTokenRepository, UserRepository};
pub use video_collection::{
    NewVideoCollection, VideoCollectionItemRepository, VideoCollectionRepository,
};
pub use video_item::{NewVideoItem, VideoItemFields, VideoItemRepository};
