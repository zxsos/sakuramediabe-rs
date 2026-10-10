//! 目录导入（上游 `catalog/catalog_import_service.py`，965 行，本域最大）。
//!
//! # 它是「元数据 → 本地记录」的唯一入口
//!
//! 三个上游来源（JavDB / 插件 metadata_source / 目录扫描）都汇到这里，
//! 由它 upsert 成 `movie` / `actor` / `image` 记录。分成三条路会各自漂移
//! —— 同一个番号从不同路进来会得到不同的字段集。
//!
//! # ★ 所有写入必须经 [`sm_db::repo::MovieOwnershipGateway`]
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
//! # ★ 已存在时**不写字段**（骨架期这里写错了）
//!
//! 上游 `import_movie_if_missing`（`:115-293`）的分支是：
//!
//! | 情况 | 行为 |
//! |---|---|
//! | 已存在 **且** 无 `javdb_id` **且** 有 `metadata_source` | 转 [`CatalogImportService::backfill_plugin_movie`]（插件影片原位接入 JavDB）|
//! | 已存在（按番号或 `javdb_id` 命中）| **直接返回，一个字段都不写** |
//! | 不存在 | 才建记录 |
//!
//! 骨架期的文档写的是「已存在时只补空字段」—— 上游**没有**这个策略。
//! 「补空字段」的语义在上游是靠**主权网关**实现的（受保护字段只在无人接管
//! 时才被写），而不是靠这里判断「目标列空不空」。
//!
//! # 图片：先下载到临时文件，全部成功才落盘
//!
//! 流程是「下载所有 → 全部成功才写库」。部分成功**不回滚**（已下载的不浪费），
//! 但**不会**出现「记录指向一个没下载成功的文件」—— 那个状态会让播放器
//! 显示裂图。
//!
//! ⚠️ **本仓还没有 image store**：封面 / 剧照的落盘与关联**尚未接线**
//! （见 [`CatalogImportService::import_movie_if_missing`] 的文档）。在那之前**不写**
//! `cover_image_id` —— 宁可没有封面，也不能指向一个不存在的文件。
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
//!
//! # ⚠️ 尚未接线的两处（如实登记）
//!
//! 1. **`force_subscribed`**：上游建记录时写 `is_subscribed` /
//!    `subscribed_at`，而本仓的 `NewMovie` 是「列的固定子集」，没有这两列；
//!    补写需要一条窄更新（还没有）。现在传 `true` **不会**写订阅状态。
//! 2. **图片落盘与演员 / 标签 / 剧照关联**：要 image store 与三个关联表的
//!    写入方法，都还没有。

use std::sync::Arc;

use sm_db::repo::{
    ActorOwnershipGateway, ActorRepository, FieldPatch, MovieOwnershipGateway, MovieRepository,
};
use sm_db::Db;

use crate::error::ServiceError;
use crate::movie_numbers::normalize_movie_number;

/// 导入后再次检查 JavDB 的间隔（天）。上游
/// `CatalogImportService.JAVDB_CHECK_INTERVAL`（`catalog_import_service.py:71`）。
///
/// ⚠️ 这个常量**只应存在一份**：`movie_javdb_backfill` 那边引用这一份。
pub const JAVDB_CHECK_INTERVAL_DAYS: i64 = 7;

/// 上游 `_MOVIE_FIELD_UPDATE_MAP`（`:470-480`）的键 —— **正好 9 个**。
///
/// `update_movie_fields` 只认这 9 个字段，其余一律拒绝
/// （上游 `ValueError("不支持的字段: ...")`）。
pub const UPDATABLE_MOVIE_FIELDS: [&str; 9] = [
    "score",
    "score_number",
    "watched_count",
    "want_watch_count",
    "comment_count",
    "title",
    "summary",
    "maker_name",
    "director_name",
];

/// 一部影片的导入结果。
// ⚠️ **不能** derive `Copy`：`updated_fields: Vec<String>` 带堆分配。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CatalogImportResult {
    pub movie_id: i32,
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
/// # 为什么方法是 `async`
///
/// 写入要查库（判番号是否已存在），而本仓的 `sm-db` 是异步的。同步签名会被
/// 逼成 `block_on`，那会在异步上下文里阻塞线程池线程。
#[tonic::async_trait]
pub trait CatalogImport {
    /// 按番号导入（**不存在才建**；已存在时按上游的分支处置，见模块文档）。
    ///
    /// 返回 `(movie_id, 是否新建)`。
    async fn import_movie_if_missing(
        &self,
        movie_number: &str,
        detail: &serde_json::Value,
    ) -> Result<(i32, bool), ServiceError>;

    /// 按番号查**已有**记录的 id。上游 `find_movie_by_number`。
    ///
    /// # 为什么窄接口要有它
    ///
    /// [`super::metadata_source::MetadataSourceService::import_by_number`] 在
    /// **问外部来源之前**先查一次库：已有的番号不该再打一次 JavDB 或插件
    /// （上游那一节的第一行就是这个短路）。
    ///
    /// 少了它，调用方只能自己拿一个 `MovieRepository` 来查 —— 那就把「窄接口」
    /// 漏成了「窄接口 + 一个仓储」。
    async fn find_movie_id(&self, movie_number: &str) -> Result<Option<i32>, ServiceError>;

    /// 从**插件交付**导入。上游
    /// `import_plugin_movie(detail, source, provider, *, force_subscribed)`。
    ///
    /// # 与 [`Self::import_movie_if_missing`] 的差别在**建记录**那一步
    ///
    /// 那一支建的是 **JavDB 来源**的记录（带 `javdb_id`）；这一支建的是
    /// **插件来源**的记录 —— `javdb_id` 为 `None`、`metadata_source` 写插件身份、
    /// 并把 `javdb_next_check_at` 预定到 7 天后（过一阵子再问 JavDB 收没收）。
    ///
    /// 两支分开而不是加一个来源参数：上游就是两个方法，而「哪一支」决定了
    /// 建记录时写哪些列。
    async fn import_plugin_movie(
        &self,
        detail: &serde_json::Value,
        source: &serde_json::Value,
        force_subscribed: bool,
    ) -> Result<(i32, bool), ServiceError>;

    /// 从 JavDB 资源 upsert 一位演员，返回演员 id。
    async fn upsert_actor(&self, actor_resource: &serde_json::Value) -> Result<i32, ServiceError>;
}

/// 图片下载器（出网）。**可注入**，测试用替身。
///
/// 抽成别名：`Box<dyn Fn(&str, &Path) -> Result<(), ServiceError>>` 这个形状
/// 在字段与构造参数上各写一遍，`clippy::type_complexity` 也会在这里报警。
pub type ImageDownloader =
    Box<dyn Fn(&str, &std::path::Path) -> Result<(), ServiceError> + Send + Sync>;

/// 目录导入服务。**元数据落地的唯一入口**。
pub struct CatalogImportService {
    db: Db,
    // 两个图片依赖**尚未被读取**：落盘要 image store（本仓还没有那一层），
    // 那一刻 [`CatalogImportService::import_movie_if_missing`] 才会用它们。图片支线落地时
    // 删掉这行 allow —— 见模块文档那两处「尚未接线」。
    #[allow(dead_code)]
    image_service: Box<dyn super::movie_image::ImageTasksBuilder + Send + Sync>,
    /// 图片下载器（出网）。**可注入**，测试用替身。同上：图片支线落地才读。
    #[allow(dead_code)]
    image_downloader: ImageDownloader,
    /// 持久化锁。**同一影片的并发导入要串行** ——
    /// 两个来源同时补录同一部片会互相覆盖，且最后写入的可能更旧。
    persist_lock: Arc<tokio::sync::Mutex<()>>,
}

impl CatalogImportService {
    /// 构造。
    pub fn new(
        db: &Db,
        image_service: Box<dyn super::movie_image::ImageTasksBuilder + Send + Sync>,
        image_downloader: ImageDownloader,
    ) -> Self {
        Self {
            db: db.clone(),
            image_service,
            image_downloader,
            persist_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// ★ 导入一部 JavDB 影片（**不存在才建**）。
    ///
    /// 上游 `import_movie_if_missing(detail, force_subscribed=False) -> (Movie, bool)`。
    ///
    /// # 已存在时的两个分支（见模块文档）
    ///
    /// 1. 无 `javdb_id` 且有 `metadata_source` → 转 [`CatalogImportService::backfill_plugin_movie`]；
    /// 2. 否则**直接返回**，一个字段都不写。
    ///
    /// # 建记录的流程
    ///
    /// 按上游顺序：拿锁 → 事务内**二次确认**（并发）→ 建 `movie` →
    /// （演员 / 标签 / 剧照 / 图片 —— 见下）。
    ///
    /// ⚠️ **图片与关联尚未接线**：本仓没有 image store，`movie_actor` /
    /// `movie_tag` / `movie_plot_image` 也还没有写入方法。所以现在这条路径
    /// 只建 `movie` 本身，**不写** `cover_image_id`（宁可没封面，也不能指向
    /// 一个不存在的文件），演员 / 标签 / 剧照留待那两层落地。
    ///
    /// ⚠️ `force_subscribed` 同样未接线（见模块文档）。
    pub async fn import_movie_if_missing(
        &self,
        detail: &serde_json::Value,
        force_subscribed: bool,
    ) -> Result<CatalogImportResult, ServiceError> {
        let repo = MovieRepository::new(self.db.clone());
        let number = self.movie_number_of(detail)?;
        if let Some(movie) = repo.find_by_number(&number).await? {
            // 快路径：插件来源的影片（有 metadata_source、还没有 javdb_id）
            // 现在 JavDB 收录了 —— 原位接入，不算新建。
            if movie.javdb_id.as_deref().unwrap_or_default().is_empty()
                && movie.metadata_source.is_some()
            {
                let result = self.backfill_plugin_movie(movie.id, detail).await?;
                return Ok(CatalogImportResult {
                    movie_id: result.movie_id,
                    created: false,
                    updated_fields: result.updated_fields,
                });
            }
            // 上游：已有 JavDB 影片跳过 —— **不写任何字段**。
            return Ok(CatalogImportResult {
                movie_id: movie.id,
                created: false,
                updated_fields: Vec::new(),
            });
        }
        self.create_movie(detail, None, force_subscribed).await
    }

    /// 从插件来源导入。上游 `import_plugin_movie(detail, source, provider, *, force_subscribed)`。
    ///
    /// 与 [`CatalogImportService::import_movie_if_missing`] 的差别在**建记录**这一步：
    /// `javdb_id` 为 `None`（插件来源没有 JavDB 身份）、`metadata_source`
    /// 透传、并把 `javdb_next_check_at` 设为 `now + 7 天`（过一阵子再问
    /// JavDB 收没收 —— 见 [`super::movie_javdb_backfill`]）。
    pub async fn import_plugin_movie(
        &self,
        detail: &serde_json::Value,
        source: &serde_json::Value,
        force_subscribed: bool,
    ) -> Result<CatalogImportResult, ServiceError> {
        let result = self
            .create_movie(detail, Some(source), force_subscribed)
            .await?;
        if result.created {
            let next_check =
                sm_db::common::time::now_utc() + chrono::Duration::days(JAVDB_CHECK_INTERVAL_DAYS);
            MovieRepository::new(self.db.clone())
                .set_plugin_metadata_source(result.movie_id, source, next_check)
                .await?;
        }
        Ok(result)
    }

    /// 回填已有影片的 JavDB 字段。上游 `backfill_plugin_movie(movie, detail) -> Movie`。
    ///
    /// 由 [`super::movie_javdb_backfill`] 的 cron 调用：插件来源的影片过一阵子
    /// 再问一次 JavDB，收录了就把身份补上。
    ///
    /// # 两处上游的**显式校验**
    ///
    /// - 番号一致性（`normalize` 后比较，`:366-372`）—— 番号对不上说明抓错了片；
    /// - `javdb_id` 冲突（`:374-378`）—— 已有身份就**不能再补**。
    ///
    /// # 写完清空 `javdb_next_check_at`
    ///
    /// 「不用再问了」。这也是 [`super::movie_javdb_backfill`] 那条候选 SQL
    /// 不再命中它的原因。
    pub async fn backfill_plugin_movie(
        &self,
        movie_id: i32,
        detail: &serde_json::Value,
    ) -> Result<CatalogImportResult, ServiceError> {
        let repo = MovieRepository::new(self.db.clone());
        let movie = repo.find_by_id(movie_id).await?.ok_or_else(|| {
            ServiceError::not_found("movie_not_found", "影片不存在", "movie_id", movie_id)
        })?;
        let detail_number = self.movie_number_of(detail)?;
        if normalize_movie_number(&detail_number) != normalize_movie_number(&movie.movie_number) {
            return Err(ServiceError::validation(
                "movie_number_mismatch",
                format!(
                    "番号不一致：影片 {} 的元数据是 {}",
                    movie.movie_number, detail_number
                ),
            ));
        }
        let javdb_id = text_of(detail, "javdb_id").unwrap_or_default();
        if !movie.javdb_id.as_deref().unwrap_or_default().is_empty() {
            return Err(ServiceError::validation(
                "javdb_id_conflict",
                format!(
                    "影片 {} 已有 JavDB 编号 {}，不能再补录",
                    movie.movie_number,
                    movie.javdb_id.as_deref().unwrap_or_default()
                ),
            ));
        }

        // ① 非受保护的那一份（身份 / 发行 / 时长 + 清空下次检查时间）。
        repo.apply_javdb_backfill(
            movie_id,
            (!javdb_id.is_empty()).then_some(javdb_id.as_str()),
            date_of(detail, "release_date"),
            int_of(detail, "duration_minutes"),
            None,
        )
        .await?;

        // ② 受保护字段（title / summary / maker_name / director_name）经网关：
        //    只在无人接管时才写。返回值不区分「被拒绝」与「值没变」，所以
        //    `updated_fields` 靠**重读**来算（与 `update_movie_fields` 同一套路）。
        let mut patch = FieldPatch::new();
        for field in ["title", "summary", "maker_name", "director_name"] {
            let value = text_of(detail, field).filter(|text| !text.is_empty());
            if let Some(value) = value {
                patch.text(field, Some(value.as_str()));
            }
        }
        let mut updated = Vec::new();
        let wrote = MovieOwnershipGateway::new(self.db.clone())
            .update_host_unowned(movie_id, &patch)
            .await?;
        if wrote > 0 {
            updated.extend(self.changed_since(&movie, movie_id).await?);
        }
        Ok(CatalogImportResult {
            movie_id,
            created: false,
            updated_fields: updated,
        })
    }

    /// ★ 更新指定字段。上游 `update_movie_fields(detail, fields) -> (Movie, bool, tuple[str, ...])`。
    ///
    /// # `fields` 的校验（照抄上游 `:484-493`）
    ///
    /// 不能为空、去重、并且必须落在 [`UPDATABLE_MOVIE_FIELDS`] 这 9 个之内 ——
    /// 否则是**调用方的 bug**（上游 `ValueError`），不是「跳过不支持的」。
    ///
    /// # 返回的是**实际写入**的字段名
    ///
    /// 两道减法：① 目标值与现值**相等**的字段跳过；② 受保护字段被主权规则
    /// 拒绝的不算（写完后**重读该行**确认，不虚报 —— 上游 `:536-542` 同样如此）。
    pub async fn update_movie_fields(
        &self,
        movie_id: i32,
        detail: &serde_json::Value,
        fields: &[&str],
    ) -> Result<CatalogImportResult, ServiceError> {
        if fields.is_empty() {
            return Err(ServiceError::validation("fields_empty", "fields 不能为空"));
        }
        let mut wanted: Vec<&str> = Vec::new();
        for field in fields {
            if !UPDATABLE_MOVIE_FIELDS.contains(field) {
                return Err(ServiceError::validation(
                    "unsupported_field",
                    format!("不支持的字段：{field}"),
                ));
            }
            if !wanted.contains(field) {
                wanted.push(field);
            }
        }
        let movie = MovieRepository::new(self.db.clone())
            .find_by_id(movie_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found("movie_not_found", "影片不存在", "movie_id", movie_id)
            })?;

        // ① 非受保护的五个计数列：只有**值变了**才写。
        let mut counts = (None, None, None, None, None);
        let mut updated: Vec<String> = Vec::new();
        macro_rules! count {
            ($slot:expr, $field:literal, $read:expr) => {
                if wanted.contains(&$field) {
                    if let Some(value) = $read {
                        if value.to_string() != movie_field(&movie, $field) {
                            $slot = Some(value);
                            updated.push($field.to_owned());
                        }
                    }
                }
            };
        }
        count!(counts.0, "score", float_of(detail, "score"));
        count!(counts.1, "score_number", int_of(detail, "score_number"));
        count!(counts.2, "watched_count", int_of(detail, "watched_count"));
        count!(
            counts.3,
            "want_watch_count",
            int_of(detail, "want_watch_count")
        );
        count!(counts.4, "comment_count", int_of(detail, "comment_count"));
        if counts.0.is_some()
            || counts.1.is_some()
            || counts.2.is_some()
            || counts.3.is_some()
            || counts.4.is_some()
        {
            MovieRepository::new(self.db.clone())
                .update_interaction_counts(
                    movie_id, counts.0, counts.1, counts.2, counts.3, counts.4,
                )
                .await?;
        }

        // ② 受保护的四列走网关（只在无人接管时写）。
        let mut patch = FieldPatch::new();
        for field in ["title", "summary", "maker_name", "director_name"] {
            if !wanted.contains(&field) {
                continue;
            }
            let Some(value) = text_of(detail, field).filter(|text| !text.is_empty()) else {
                continue;
            };
            if value == movie_field(&movie, field) {
                continue;
            }
            patch.text(field, Some(value.as_str()));
        }
        if !patch.is_empty() {
            MovieOwnershipGateway::new(self.db.clone())
                .update_host_unowned(movie_id, &patch)
                .await?;
            // 重读：被主权规则拒掉的字段不能算进 `updated_fields`。
            updated.retain(|field| {
                !["title", "summary", "maker_name", "director_name"].contains(&field.as_str())
            });
            updated.extend(self.changed_since(&movie, movie_id).await?);
        }
        Ok(CatalogImportResult {
            movie_id,
            created: false,
            updated_fields: updated,
        })
    }

    /// 严格刷新元数据。上游 `refresh_movie_metadata_strict(movie, detail) -> Movie`。
    ///
    /// 「严格」= **值不同就覆盖**，不像
    /// [`CatalogImportService::import_movie_if_missing`] 那样「已存在直接返回」。它复用的正是
    /// [`CatalogImportService::update_movie_fields`] 那套变更检测 —— 那套检测只在「值相等」时
    /// 跳过，所以本身就是覆盖式的。
    pub async fn refresh_movie_metadata_strict(
        &self,
        movie_id: i32,
        detail: &serde_json::Value,
    ) -> Result<CatalogImportResult, ServiceError> {
        self.update_movie_fields(movie_id, detail, &UPDATABLE_MOVIE_FIELDS)
            .await
    }

    /// 为已有影片补算竖封面。上游 `backfill_movie_thin_cover(movie) -> bool`。
    ///
    /// 流程：查影片（不存在 → 404）→ 已有竖封面直接返回 `false`（**不覆盖**，
    /// 上游 `backfill_missing_thin_cover_images` 语义）→ 解析薄封面 →
    /// 落盘登记 → 更新 `movie.thin_cover_image_id`。
    ///
    /// 解析失败（无封面/切不出书脊）→ `Ok(false)` **降级**，不抛错 —— 上游
    /// `cv2` 缺失时也是降级返回 `false`。
    pub async fn backfill_movie_thin_cover(&self, movie_id: i32) -> Result<bool, ServiceError> {
        let movie = MovieRepository::new(self.db.clone())
            .find_by_id(movie_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found("movie_not_found", "影片不存在", "movie_id", movie_id)
            })?;
        // 已有竖封面的不覆盖（回填语义：只补没有的）。
        if movie.thin_cover_image_id.is_some() {
            return Ok(false);
        }
        let resolution = match self
            .image_service
            .resolve_thin_cover_from_existing_movie(movie_id)
            .await
        {
            Ok(resolution) => resolution,
            Err(_) => return Ok(false),
        };
        let Some(image_id) = self
            .image_service
            .persist_thin_cover(&movie.movie_number, resolution)
            .await?
        else {
            return Ok(false);
        };
        MovieRepository::new(self.db.clone())
            .set_thin_cover_image_id(movie_id, Some(image_id))
            .await?;
        Ok(true)
    }

    /// 从 JavDB 资源 upsert 一位演员。
    ///
    /// 上游 `upsert_actor_from_javdb_resource(actor_resource, profile_image_task=None, *, update_gender=False)`
    /// （`:880-954`）。
    ///
    /// ⚠️ `update_gender` 默认 **false**：不要用外部数据覆盖用户改过的性别。
    /// 性别是**受保护字段**，写它要经
    /// [`ActorOwnershipGateway::update_host_source`]
    /// 并带 `host:javdb` 这个 owner（网关拒绝人工 owner 与非 `host:` 前缀）。
    ///
    /// ⚠️ 头像：上游会在拿到头像任务时落盘并写 `profile_image`；本仓没有
    /// image store，所以这里**不写**头像（不是「写成空」）—— 那条路留待
    /// image store 落地时补。
    pub async fn upsert_actor_from_javdb_resource(
        &self,
        actor_resource: &serde_json::Value,
        update_gender: bool,
    ) -> Result<i32, ServiceError> {
        let javdb_id = text_of(actor_resource, "javdb_id")
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .ok_or_else(|| {
                ServiceError::validation("actor_javdb_id_missing", "演员资源缺少 javdb_id")
            })?;
        let name = text_of(actor_resource, "name")
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .ok_or_else(|| ServiceError::validation("actor_name_missing", "演员资源缺少 name"))?;
        let repo = ActorRepository::new(self.db.clone());
        let actor = match repo.find_by_javdb_id(&javdb_id).await? {
            Some(actor) => actor,
            None => {
                repo.insert(&sm_db::repo::NewActor {
                    javdb_id: javdb_id.clone(),
                    name: name.clone(),
                })
                .await?
            }
        };
        repo.update_javdb_profile(
            actor.id,
            Some(name.as_str()),
            None,
            int_of(actor_resource, "javdb_type"),
            None,
        )
        .await?;
        if update_gender {
            if let Some(gender) = int_of(actor_resource, "gender").filter(|g| *g == 1 || *g == 2) {
                let mut patch = FieldPatch::new();
                patch.int("gender", Some(gender));
                ActorOwnershipGateway::new(self.db.clone())
                    .update_host_source(actor.id, &patch, "host:javdb")
                    .await?;
            }
        }
        Ok(actor.id)
    }

    /// 建一部影片。**持锁 + 二次确认**（上游 `:200-210` 的并发处理）。
    async fn create_movie(
        &self,
        detail: &serde_json::Value,
        source: Option<&serde_json::Value>,
        _force_subscribed: bool,
    ) -> Result<CatalogImportResult, ServiceError> {
        let _guard = self.persist_lock.lock().await;
        let repo = MovieRepository::new(self.db.clone());
        let number = self.movie_number_of(detail)?;
        // 二次确认：拿锁之前可能已经有别人建了同一部。
        if let Some(movie) = repo.find_by_number(&number).await? {
            return Ok(CatalogImportResult {
                movie_id: movie.id,
                created: false,
                updated_fields: Vec::new(),
            });
        }
        let movie = repo
            .insert(&sm_db::repo::NewMovie {
                movie_number: number,
                title: text_of(detail, "title").unwrap_or_default(),
                // 插件来源**没有** JavDB 身份（`import_plugin_movie` 走这一支）。
                javdb_id: source
                    .map(|_| None)
                    .unwrap_or_else(|| text_of(detail, "javdb_id")),
                summary: text_of(detail, "summary").unwrap_or_default(),
                maker_name: text_of(detail, "maker_name"),
                director_name: text_of(detail, "director_name"),
                release_date: date_of(detail, "release_date"),
                duration_minutes: int_of(detail, "duration_minutes").unwrap_or(0),
                score: float_of(detail, "score").unwrap_or(0.0),
                score_number: int_of(detail, "score_number").unwrap_or(0),
                // 系列与封面都要 join / 落盘，本轮都没接（见模块文档）。
                series_id: None,
                cover_image_id: None,
                ..Default::default()
            })
            .await?;
        Ok(CatalogImportResult {
            movie_id: movie.id,
            created: true,
            updated_fields: Vec::new(),
        })
    }

    /// 取番号（去空白）。**没有番号是调用方的 bug**，不是「跳过」。
    fn movie_number_of(&self, detail: &serde_json::Value) -> Result<String, ServiceError> {
        text_of(detail, "movie_number")
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty())
            .ok_or_else(|| {
                ServiceError::validation("movie_number_missing", "元数据缺少 movie_number")
            })
    }

    /// 重读该行，返回**真正发生变化**的那几个受保护字段。
    /// 上游在网关写入后就是这样回流的（`:536-542`）—— 不虚报。
    async fn changed_since(
        &self,
        before: &sm_db::Movie,
        movie_id: i32,
    ) -> Result<Vec<String>, ServiceError> {
        let after = MovieRepository::new(self.db.clone())
            .find_by_id(movie_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found("movie_not_found", "影片不存在", "movie_id", movie_id)
            })?;
        let mut changed = Vec::new();
        for field in ["title", "summary", "maker_name", "director_name"] {
            if movie_field(before, field) != movie_field(&after, field) {
                changed.push(field.to_owned());
            }
        }
        Ok(changed)
    }
}

#[tonic::async_trait]
impl CatalogImport for CatalogImportService {
    async fn import_movie_if_missing(
        &self,
        movie_number: &str,
        detail: &serde_json::Value,
    ) -> Result<(i32, bool), ServiceError> {
        // 插件可能不返回 movie_number（如 JavDB 搜索），此时用传入的番号补上
        let mut detail = detail.clone();
        if text_of(&detail, "movie_number")
            .map(|t| t.trim().is_empty())
            .unwrap_or(true)
        {
            if let Some(obj) = detail.as_object_mut() {
                obj.insert(
                    "movie_number".to_owned(),
                    serde_json::Value::String(movie_number.to_owned()),
                );
            }
        }
        // `Self::` 前缀解析到**固有方法**，不是要递归调 trait 方法。
        let result = Self::import_movie_if_missing(self, &detail, false).await?;
        Ok((result.movie_id, result.created))
    }

    async fn find_movie_id(&self, movie_number: &str) -> Result<Option<i32>, ServiceError> {
        let number = movie_number.trim();
        if number.is_empty() {
            // 空番号是**调用方的 bug**，不是「查不到」—— 返回 `Ok(None)` 会让
            // 调用方接着去问外部来源，而那个查询注定也拿不到东西。
            return Err(ServiceError::validation(
                "movie_number_missing",
                "番号不能为空",
            ));
        }
        Ok(MovieRepository::new(self.db.clone())
            .find_by_number(number)
            .await?
            .map(|movie| movie.id))
    }

    async fn import_plugin_movie(
        &self,
        detail: &serde_json::Value,
        source: &serde_json::Value,
        force_subscribed: bool,
    ) -> Result<(i32, bool), ServiceError> {
        // 同上：`Self::` 是固有方法。
        let result = Self::import_plugin_movie(self, detail, source, force_subscribed).await?;
        Ok((result.movie_id, result.created))
    }

    async fn upsert_actor(&self, actor_resource: &serde_json::Value) -> Result<i32, ServiceError> {
        // 窄接口不暴露 `update_gender`：调用方（元数据来源）不该顺手改性别 ——
        // 上游默认 False 也是这个意思。
        self.upsert_actor_from_javdb_resource(actor_resource, false)
            .await
    }
}

/// 读一个文本字段。空白串按「没给」处理（上游大量 `or None`）。
fn text_of(detail: &serde_json::Value, field: &str) -> Option<String> {
    detail
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn int_of(detail: &serde_json::Value, field: &str) -> Option<i32> {
    detail
        .get(field)
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
}

fn float_of(detail: &serde_json::Value, field: &str) -> Option<f64> {
    detail.get(field).and_then(serde_json::Value::as_f64)
}

fn date_of(detail: &serde_json::Value, field: &str) -> Option<chrono::NaiveDateTime> {
    let text = text_of(detail, field)?;
    chrono::NaiveDate::parse_from_str(&text, "%Y-%m-%d")
        .ok()
        .and_then(|date| date.and_hms_opt(0, 0, 0))
}

/// 取影片某一列的**字符串表示**，只为「变没变」的比较服务。
fn movie_field(movie: &sm_db::Movie, field: &str) -> String {
    match field {
        "title" => movie.title.clone(),
        "summary" => movie.summary.clone(),
        "maker_name" => movie.maker_name.clone().unwrap_or_default(),
        "director_name" => movie.director_name.clone().unwrap_or_default(),
        "score" => movie.score.to_string(),
        "score_number" => movie.score_number.to_string(),
        "watched_count" => movie.watched_count.to_string(),
        "want_watch_count" => movie.want_watch_count.to_string(),
        "comment_count" => movie.comment_count.to_string(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `UPDATABLE_MOVIE_FIELDS` 与上游 `_MOVIE_FIELD_UPDATE_MAP` **逐字一致**
    /// —— 多一个或少一个都会让「这个字段能不能被刷新」静默改变。
    #[test]
    fn the_updatable_fields_match_upstreams_map() {
        let mut sorted = UPDATABLE_MOVIE_FIELDS;
        sorted.sort();
        assert_eq!(
            sorted,
            [
                "comment_count",
                "director_name",
                "maker_name",
                "score",
                "score_number",
                "summary",
                "title",
                "want_watch_count",
                "watched_count",
            ]
        );
    }

    /// 空白串按「没给」处理 —— 上游大量的 `or None` 就是这个语义，
    /// 反过来会把空标题写进库里覆盖掉真实标题。
    #[test]
    fn blank_text_counts_as_absent() {
        let detail = serde_json::json!({"title": "   ", "score": 0});
        assert_eq!(text_of(&detail, "title"), None);
        assert_eq!(float_of(&detail, "score"), Some(0.0));
        assert_eq!(text_of(&detail, "missing"), None);
    }

    /// 番号缺失是**调用方的 bug**（上游要按番号定位），不能静默跳过。
    #[test]
    fn a_detail_without_a_movie_number_is_rejected() {
        assert!(text_of(&serde_json::json!({}), "movie_number").is_none());
        assert!(
            text_of(&serde_json::json!({"movie_number": "  "}), "movie_number").is_none(),
            "纯空白也算没有"
        );
        assert_eq!(
            text_of(
                &serde_json::json!({"movie_number": " ABC-123 "}),
                "movie_number"
            )
            .as_deref(),
            Some("ABC-123"),
            "只去首尾空白，不改大小写与分隔符"
        );
    }

    /// 日期只认严格的 `YYYY-MM-DD`（proto 写在字段上的原话）。
    #[test]
    fn only_a_strict_calendar_date_is_accepted() {
        let detail = serde_json::json!({"release_date": "2026-01-02", "bad": "2026/01/02"});
        assert!(date_of(&detail, "release_date").is_some());
        assert_eq!(date_of(&detail, "bad"), None);
    }
}
