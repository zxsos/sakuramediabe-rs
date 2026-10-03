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
//! # 覆盖范围：40 张表里的 11 张
//!
//! 下面这些是**已知缺口**，不是待办清单里的小事。按「不解锁别的就写不了」
//! 的顺序排列：
//!
//! | 优先级 | 缺口 | 阻塞了什么 |
//! |---|---|---|
//! | P1 | [`Actor`] 无仓储 | 与 `Movie` 完全对称的主数据（9 个受保护字段 + 合并链 + 字段主权），却连 `find_by_javdb_id` 都没有 |
//! | P1 | `Image` / `Tag` / `MovieActor` / `MovieTag` / `Subtitle` 无仓储 | `asset.rs` 自己指出影片资产要按 `origin` 前缀查（有 `text_pattern_ops` 索引）—— **索引是为某个查询建的，而该查询不存在** |
//! | P1 | [`MediaLibrary`] 无仓储 | `media.library_id` 指向它，但库管理端点（增删改查 provider 配置）无落点。写 `media` 前必须先有库 |
//! | P2 | `Movie.subscription_search_*` 9 列无方法 | 这是**第二个重试状态机**（与 `download_task` 的双状态机同构），但既没有「列出到期任务」也没有「记录一次尝试」。注意 [`movie::MovieRepository::list_by_subscription_state`] 过滤的是 `is_subscribed`，与这 9 列无关 |
//! | P2 | `DownloadSubmissionRecord` 无仓储 | `download.rs` 的注释把幂等提交建立在 `(client, remote_id)` 唯一索引上，但「先查后插」要查的正是这张表 |
//! | P2 | 合集族 6 张表 / 其余 5 张传输表 | `PluginOwned` trait 与 `playback_order_key()` 已为仓储预留形状，一个方法都没有 |
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
//! # 两条结构性缺失
//!
//! **零 `delete`**：只有 `media_point` 与 `media_progress` 两处局部例外
//! （`delete` / `clear`），其余表的删除路径只存在于注释里。
//!
//! **组合写入已有出口**：[`UnitOfWork`] 按**动词**暴露用例，每个方法内部
//! 编排多个仓储的 `_in` 变体并共享一个事务。已落地的用例：
//! `generate_thumbnail`（写 `media_thumbnail` + 推进 `media` 状态机）。
//! 仍缺的是跨**更多**表的用例，例如「插 Movie + 3 条 MovieActor +
//! upsert Tag」—— 那需要先有 `MovieActor` / `Tag` 的仓储。
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

pub mod ctx;
pub mod download;
pub mod gateway;
pub mod media;
pub mod movie;
pub mod playback;
pub mod task;
pub mod user;

pub use ctx::{Ctx, CtxConnection, GeneratedThumbnail, UnitOfWork};
pub use download::{DownloadTaskRepository, NewDownloadTask};
pub use gateway::{FieldCodec, FieldPatch, FieldValue, MovieOwnershipGateway};
pub use media::{MediaRepository, NewMedia};
pub use movie::{MovieRepository, MovieSeriesRepository, NewMovie, SubscriptionState};
pub use playback::{
    MediaClipRepository, MediaPointRepository, MediaProgressRepository, MediaThumbnailRepository,
    NewMediaClip,
};
pub use task::{BackgroundTaskRunRepository, ClaimedTask, NewTaskRun, TaskOutcome, TaskProgress};
pub use user::{NewRefreshToken, NewUser, Rotation, UserRefreshTokenRepository, UserRepository};
