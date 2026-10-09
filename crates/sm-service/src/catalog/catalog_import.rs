//! 目录导入（上游 `catalog/catalog_import_service.py`，965 行，本域最大）。
//!
//! # 它是「元数据 → 本地记录」的唯一入口
//!
//! 三个上游来源（JavDB / 插件 metadata_source / 目录扫描）都汇到这里，
//! 由它 upsert 成 `movie` / `actor` / `image` 记录。分成三条路会各自漂移
//! —— 同一个番号从不同路进来会得到不同的字段集。
//!
//! # ★ 所有写入必须经 [`MovieOwnershipGateway`](sm_db::repo::MovieOwnershipGateway)
//!
//! 本文件是**插件写入的主要发起方**，所以它最需要守那条规则：插件能写的
//! 只有它声明的字段，身份（`javdb_id`）、头像、订阅不在其中。
//!
//! 直接 `UPDATE movie SET ...` 会让插件的补录覆盖掉用户手动设的黑名单。
//!
//! # 「JavDB 补录」要等 7 天
//!
//! [`JAVDB_CHECK_INTERVAL_DAYS`] = 7 天。导入时 JavDB 可能还没收录这部片，
//! 立刻重试只会白等（见 [`super::movie_javdb_backfill`]：那是 cron 任务的事）。
//!
//! # 图片：先下载到临时文件，全部成功才落盘
//!
//! 流程是「下载所有 → 全部成功才写库」。部分成功**不回滚**（已下载的不浪费），
//! 但**不会**出现「记录指向一个没下载成功的文件」—— 那个状态会让播放器
//! 显示裂图。
//!
//! # 薄封面（thin cover）从剧情图里切
//!
//! [`super::movie_image`] 的活。⚠️ `cv2` 缺失时**降级跳过**，不报错 ——
//! 少一张竖封面不影响影片可用性。
//!
//! # 清 Qdrant 剧情图向量是**惰性**的
//!
//! 删图片记录时要清对应的向量，但**只在图搜启用时**做（`image_search_enabled()`）。
//! 没启用就没建过索引，清它是白费一次网络往返。

use std::sync::Arc;

use crate::error::ServiceError;

/// 导入后再次检查 JavDB 的间隔（天）。
pub const JAVDB_CHECK_INTERVAL_DAYS: i64 = 7;

/// 一部影片的导入结果。
// ⚠️ **不能** derive `Copy`：`updated_fields: Vec<String>` 带堆分配。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogImportResult {
    pub movie_id: i64,
    /// ★ 本次**是否新建**。`false` = 已存在并被更新。
    pub created: bool,
    /// 实际写入的字段名。调用方用它判断「这次补了什么」。
    pub updated_fields: Vec<String>,
}

/// ★ 目录写入的**窄接口**。由 [`CatalogImportService`] 实现。
///
/// # 为什么要抽出 trait
///
/// [`super::metadata_source`] 只**需要**「把元数据写进去」这一件事。若它直接
/// 依赖 `CatalogImportService`（具体 struct），就把图片下载、封面切割、
/// 资产包重建全拖进了依赖图 —— 而那些与「哪个来源提供了元数据」毫无关系。
///
/// 窄接口让两个模块能各自独立测试。
pub trait CatalogImport {
    /// 按番号导入（**不存在才建**，已有则只补空字段）。
    ///
    /// 返回 `(movie_id, 是否新建)`。
    fn import_movie_if_missing(
        &mut self,
        movie_number: &str,
        detail: &serde_json::Value,
    ) -> Result<(i64, bool), ServiceError>;

    /// 从 JavDB 资源 upsert 一位演员，返回演员 id。
    fn upsert_actor(&mut self, actor_resource: &serde_json::Value) -> Result<i64, ServiceError>;
}

/// 图片下载器（出网）。**可注入**，测试用替身。
///
/// 抽成别名：`Box<dyn Fn(&str, &Path) -> Result<(), ServiceError>>` 这个形状
/// 在字段与构造参数上各写一遍，`clippy::type_complexity` 也会在这里报警。
pub type ImageDownloader = Box<dyn Fn(&str, &std::path::Path) -> Result<(), ServiceError>>;

/// 目录导入服务。**元数据落地的唯一入口**。
// 三个字段都是构造时注入、**尚未被方法体引用**的依赖（那些方法还是
// `todo!()`）。落地时删掉这行 allow —— 它不该长期存在。
#[allow(dead_code)]
pub struct CatalogImportService {
    image_service: Option<Box<dyn super::movie_image::ImageTasksBuilder>>,
    /// 图片下载器（出网）。**可注入**，测试用替身。
    image_downloader: Option<ImageDownloader>,
    /// 持久化锁。**同一影片的并发导入要串行** ——
    /// 两个来源同时补录同一部片会互相覆盖，且最后写入的可能更旧。
    persist_lock: Option<Arc<tokio::sync::Mutex<()>>>,
}

impl CatalogImportService {
    /// 构造。
    pub fn new(
        image_service: Box<dyn super::movie_image::ImageTasksBuilder>,
        image_downloader: ImageDownloader,
    ) -> Self {
        Self {
            image_service: Some(image_service),
            image_downloader: Some(image_downloader),
            persist_lock: Some(Arc::new(tokio::sync::Mutex::new(()))),
        }
    }

    /// ★ 导入一部 JavDB 影片（**不存在才建**）。
    ///
    /// 上游 `import_movie_if_missing(detail, force_subscribed=False) -> (Movie, bool)`。
    /// 返回 `(影片, 是否新建)`。
    ///
    /// 已存在时**只补空字段**，不覆盖已有值 —— 用户手改过的标题不该被一次
    /// 补录冲掉。`force_subscribed` 才允许写订阅状态。
    pub async fn import_movie_if_missing(
        &self,
        detail: &serde_json::Value,
        force_subscribed: bool,
    ) -> Result<CatalogImportResult, ServiceError> {
        let _ = (detail, force_subscribed);
        todo!(
            "骨架：查番号(规范化) -> 不存在则建 -> 存在则只补空字段；写经 movie_ownership_gateway"
        )
    }

    /// 从插件来源导入。上游 `import_plugin_movie(detail, source, provider, *, force_subscribed)`。
    ///
    /// `source` 落进 `movie.metadata_source`（**JSONB 透传**，不解释）。
    pub async fn import_plugin_movie(
        &self,
        detail: &serde_json::Value,
        source: &serde_json::Value,
        force_subscribed: bool,
    ) -> Result<CatalogImportResult, ServiceError> {
        let _ = (detail, source, force_subscribed);
        todo!("骨架：同 import_movie_if_missing，但额外写 metadata_source 记录来源")
    }

    /// 回填已有影片的 JavDB 字段。上游 `backfill_plugin_movie(movie, detail) -> Movie`。
    ///
    /// 由 [`super::movie_javdb_backfill`] 的 cron 调用。
    pub async fn backfill_plugin_movie(
        &self,
        movie_id: i64,
        detail: &serde_json::Value,
    ) -> Result<CatalogImportResult, ServiceError> {
        let _ = (movie_id, detail);
        todo!("骨架：只写插件/JavDB 拥有的字段；写完把 javdb_next_check_at 推后 7 天")
    }

    /// ★ 更新指定字段。上游 `update_movie_fields(detail, fields) -> (Movie, bool, tuple[str, ...])`。
    ///
    /// 返回**实际写入的字段名**（可能被主权规则拒绝一部分）。
    pub async fn update_movie_fields(
        &self,
        movie_id: i64,
        detail: &serde_json::Value,
        fields: &[&str],
    ) -> Result<CatalogImportResult, ServiceError> {
        let _ = (movie_id, detail, fields);
        todo!("骨架：按 fields 白名单写，逐条经主权网关；返回实际写入的字段名")
    }

    /// 严格刷新元数据。上游 `refresh_movie_metadata_strict(movie, detail) -> Movie`。
    ///
    /// 「严格」的含义：**即使字段已有值也覆盖**（与 `import_movie_if_missing`
    /// 的「只补空」相反）。用于用户主动点「刷新」—— 那时他就是要最新的。
    pub async fn refresh_movie_metadata_strict(
        &self,
        movie_id: i64,
        detail: &serde_json::Value,
    ) -> Result<CatalogImportResult, ServiceError> {
        let _ = (movie_id, detail);
        todo!("骨架：覆盖式写入（仍经主权网关，受保护字段仍不可写）")
    }

    /// 为已有影片补算竖封面。上游 `backfill_movie_thin_cover(movie) -> bool`。
    ///
    /// `cv2` 缺失时**降级返回 `false`**，不报错。
    pub async fn backfill_movie_thin_cover(&self, movie_id: i64) -> Result<bool, ServiceError> {
        let _ = movie_id;
        todo!("骨架：从已落盘的剧情图切竖封面；cv2 缺失则返回 false（降级不报错）")
    }

    /// 从 JavDB 资源 upsert 一位演员。
    ///
    /// 上游 `upsert_actor_from_javdb_resource(actor_resource, profile_image_task=None, *, update_gender=False)`。
    ///
    /// ⚠️ `update_gender` 默认 **false**：不要用外部数据覆盖用户改过的性别。
    /// 头像走 [`ActorOwnershipGateway`](sm_db::repo::ActorOwnershipGateway)
    /// —— 它禁止插件写，但 JavDB 是
    /// `host:javdb` owner，可以写。
    pub async fn upsert_actor_from_javdb_resource(
        &self,
        actor_resource: &serde_json::Value,
        update_gender: bool,
    ) -> Result<i64, ServiceError> {
        let _ = (actor_resource, update_gender);
        todo!("骨架：查 JavDB id -> 建或更新 -> 头像经 ActorOwnershipGateway(host:javdb owner)；性别默认不改")
    }
}
