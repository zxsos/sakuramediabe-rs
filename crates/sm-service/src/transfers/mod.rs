//! `transfers` 子域（上游 23 文件 / 4,248 行）。
//!
//! # 三个能力簇，与上游目录结构一致
//!
//! ```text
//!   downloads/    搜索种子（Torznab）· 提交下载 · 客户端配置 · 任务台账 · 状态同步
//!   imports/      provider 侧扫描 → 暂存 → 宿主写入 → 定稿
//!   shared/       导入与转存两个「TaskRun 边界」+ 互斥键 + 通知
//! ```
//!
//! 本仓**平铺**在 `transfers/` 下而不是照抄目录层级：`downloads` 与
//! `shared` 里的文件互相引用极多（`common.rs` 被 5 个文件用、`import_task`
//! 被 `download_task` 用），而本仓其它域（`catalog` / `playback`）也都是平铺，
//! 引入子目录只会让 `use` 路径变长。
//!
//! # 映射表（上游 17 个有实质内容的文件 → 全部已铺）
//!
//! | 本模块 | 上游 | 依赖 |
//! |---|---|---|
//! | [`torznab`] | `downloads/clients/torznab.py` | 纯本地（HTTP 客户端） |
//! | [`download_search`] | `downloads/search_service.py` | 纯本地 |
//! | [`download_common`] | `downloads/common.py` | provider + DB |
//! | [`download_resource_hash`] | `downloads/resource_hash.py` | 出网 + libtorrent |
//! | [`download_request`] | `downloads/request_service.py` | `download_client` |
//! | [`download_client`] | `downloads/client_config_service.py` | `download_client` |
//! | [`download_task`] | `downloads/task_service.py` | `download_client` + 导入 |
//! | [`download_sync`] | `downloads/sync_service.py` | `download_client` |
//! | [`auto_download`] | `downloads/auto_subscribed/auto_download_service.py` | 索引器 + `download_client` |
//! | [`import_service`] | `imports/import_service.py`（949 行，最大） | `media.provider` + `metadata_source` |
//! | [`provider_browse`] | `imports/provider_browse_service.py` | `media.provider` |
//! | [`import_task`] | `shared/import_task_service.py`（713 行） | `metadata_source` + `download_client` |
//! | [`media_transfer_task`] | `shared/media_transfer_task_service.py`（576 行） | `media.provider` |
//! | [`transfer_shared`] | `shared/common.py` | 纯本地（peewee EXISTS 表达式） |
//! | [`import_write_mutex`] | `shared/write_mutex.py` | 纯本地 |
//! | [`import_notifications`] | `shared/import_notifications.py` | 纯本地 |
//!
//! 上游 6 个空 `__init__.py` 不对应文件。
//!
//! # 本域**没有**一处需要 Qdrant 或推理服务
//!
//! 与 `discovery` 域相反 —— 时刻推荐的图片字节来自 DB 里的路径，不是向量。
//! 所以本域的阻塞全部集中在**插件 ABI** 与**本地文件系统**两处。
//!
//! # 两个 TaskRun 边界：`import_task` 与 `media_transfer_task`
//!
//! 它们是本域唯二「**只**通过 `BackgroundTaskRun` 驱动」的服务 —— HTTP
//! 端点只负责入队并立刻返回 202，执行体跑在 worker 里。移植时要保住这条
//! 边界：把执行体搬进 HTTP 处理器会让长任务占住连接，而上游是刻意不这么做的。

pub mod auto_download;
pub mod download_client;
pub mod download_common;
pub mod download_request;
pub mod download_resource_hash;
pub mod download_search;
pub mod download_sync;
pub mod download_task;
pub mod import_notifications;
pub mod import_service;
pub mod import_task;
pub mod import_write_mutex;
pub mod media_transfer_task;
pub mod provider_browse;
pub mod torznab;
pub mod transfer_shared;
