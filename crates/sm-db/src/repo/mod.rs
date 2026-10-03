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
//!
//! # 覆盖范围：40 张表里的 4 张
//!
//! 下面这些是**已知缺口**，不是待办清单里的小事。按「不解锁别的就写不了」
//! 的顺序排列：
//!
//! | 优先级 | 缺口 | 阻塞了什么 |
//! |---|---|---|
//! | P0 | [`User`] / [`UserRefreshToken`] 无仓储 | 认证链路的**唯一**持久化状态。`sm-core` 已有 `password.rs` / `jwt.rs` / `refresh_token.rs`，但没有仓储就连登录端点都跑不起来 |
//! | P0 | [`BackgroundTaskRun`] 无仓储 | 后台队列。模型注释写明「pending 行即队列元素」，`is_claimable` 已经是可直接翻译成 SQL 的谓词。缩略图重试、下载导入、订阅搜索三个流程都要过它 |
//! | P0 | 任何表都没有分页与计数 | `sm-core::pagination::Paginated` 需要 `offset` 与 `total`，而所有 list 方法只有 `limit`。**这是横切缺口**，不是单表缺口 |
//! | P1 | [`MediaThumbnail`] 无仓储 | `media` 上已有的缩略图状态机（[`media::MediaRepository::record_thumbnail_success`]）**产出的行无处可存** —— 现有代码内部就已经断裂 |
//! | P1 | `Media` 缺业务键查询 | 只有 `find_by_id`。`file_hash` 的模型注释明说它是「跨存储识别重复文件的依据」，却没有对应的去重查询 |
//! | P1 | [`MediaProgress`] / [`MediaPoint`] / [`MediaClip`] 无仓储 | 播放进度是典型 upsert（`media` 上有唯一索引），最需要 `upsert_progress` |
//! | P1 | [`Actor`] 无仓储 | 与 `Movie` 完全对称的主数据（9 个受保护字段 + 合并链 + 字段主权），却连 `find_by_javdb_id` 都没有 |
//! | P1 | `Image` / `Tag` / `MovieActor` / `MovieTag` / `Subtitle` 无仓储 | `asset.rs` 自己指出影片资产要按 `origin` 前缀查（有 `text_pattern_ops` 索引）—— **索引是为某个查询建的，而该查询不存在** |
//! | P2 | `Movie.subscription_search_*` 9 列无方法 | 这是**第二个重试状态机**（与 `download_task` 的双状态机同构），但既没有「列出到期任务」也没有「记录一次尝试」。注意 [`movie::MovieRepository::list_by_subscription_state`] 过滤的是 `is_subscribed`，与这 9 列无关 |
//! | P2 | `DownloadSubmissionRecord` 无仓储 | `download.rs` 的注释把幂等提交建立在 `(client, remote_id)` 唯一索引上，但「先查后插」要查的正是这张表 |
//! | P2 | 合集族 6 张表 / 其余 5 张传输表 | `PluginOwned` trait 与 `playback_order_key()` 已为仓储预留形状，一个方法都没有 |
//!
//! 还有两处**跨仓储的结构性缺失**：零 `delete`（`media_point` 的
//! RESTRICT / SET NULL 语义只存在于注释里），以及零事务
//! （`pool()` 的文档说「事务场景需要它」，但没有任何方法接受
//! `&mut Transaction`，多表写入无处表达原子性）。
//!
//! [`User`]: crate::system::user::User
//! [`UserRefreshToken`]: crate::system::user::UserRefreshToken
//! [`BackgroundTaskRun`]: crate::system::activity::BackgroundTaskRun
//! [`MediaThumbnail`]: crate::playback::media::MediaThumbnail
//! [`MediaProgress`]: crate::playback::media::MediaProgress
//! [`MediaPoint`]: crate::playback::media::MediaPoint
//! [`MediaClip`]: crate::playback::media::MediaClip
//! [`Actor`]: crate::catalog::actor::Actor
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

pub mod download;
pub mod gateway;
pub mod media;
pub mod movie;
pub mod task;
pub mod user;

pub use download::{DownloadTaskRepository, NewDownloadTask};
pub use gateway::{FieldCodec, FieldPatch, FieldValue, MovieOwnershipGateway};
pub use media::{MediaRepository, NewMedia};
pub use movie::{MovieRepository, MovieSeriesRepository, NewMovie, SubscriptionState};
pub use task::{BackgroundTaskRunRepository, ClaimedTask, NewTaskRun, TaskOutcome, TaskProgress};
pub use user::{NewRefreshToken, NewUser, Rotation, UserRefreshTokenRepository, UserRepository};
