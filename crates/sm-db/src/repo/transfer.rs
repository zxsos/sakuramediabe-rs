//! 传输链的三张表：`download_client` / `indexer` / `indexer_download_client`，
//! 外加独立的 `download_resource_blacklist`。
//!
//! # 为什么它们必须一起做
//!
//! ```text
//! media_library
//!    └─ download_client ── download_task
//!          ↑
//!    indexer ── indexer_download_client
//! ```
//!
//! `download_client.library_id` 指向 `media_library`，而 `download_task.client_id`
//! 指向 `download_client` —— 写下载任务前必须先有库与下载器。所以
//! `download_client` 单独一个仓储不解决问题，它是三张表里最先要落地的那一环。
//!
//! # 三张表都是 CASCADE 的一端
//!
//! | 删谁 | 连带 |
//! |---|---|
//! | `media_library` | 该库的 `download_client` → 它们的 `download_task` |
//! | `download_client` | 它的 `download_task` 与 `indexer_download_client` |
//! | `indexer` | 它的 `indexer_download_client` 绑定 |
//!
//! 删下载器会连带删掉下载历史，这是 schema 的决定，仓储层照实返回
//! `ConstraintViolation` 或成功，不做静默跳过 —— 「删掉它会连带删掉 12 条任务」
//! 是调用方该先知道的。
//!
//! # 绑定表的语义
//!
//! `indexer_download_client` 的唯一索引 `(indexer_id, download_client_id)`
//! 决定 [`IndexerDownloadClientRepository::bind`] 的行为是**幂等**而不是
//! 「重复插入报错」：同一个组合绑两次没有意义，调用方（配置同步、UI 保存）
//! 天然会重复提交。

use sqlx::PgPool;

use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;
use crate::transfers::downloads::{DownloadClient, DownloadResourceBlacklist, Indexer};

use sqlx::PgConnection;

use super::ctx::Ctx;

const CLIENT_ENTITY: &str = "DownloadClient";
const INDEXER_ENTITY: &str = "Indexer";
const BINDING_ENTITY: &str = "IndexerDownloadClient";
const BLACKLIST_ENTITY: &str = "DownloadResourceBlacklist";

/// `provider_config` 的空值形态，与列的 `DEFAULT '{}'` 一致。
const EMPTY_JSON_OBJECT: &str = "{}";
/// 新建一个下载器。
#[derive(Debug, Clone)]
pub struct NewDownloadClient {
    /// 全局唯一。
    pub name: String,
    /// 不透明 JSON 文本。`None` 落库为 `'{}'`。
    ///
    /// 列是 `text NOT NULL DEFAULT '{}'`，而模型是 `Option<String>` ——
    /// 对拍对 `text/json` 列的可空性放行，理由与 `compare_schema.py` 里的
    /// 类型映射一致。所以「缺失」必须由仓储兜成 `'{}'`，绑 `None` 会违反
    /// NOT NULL。
    pub provider_config: Option<String>,
    /// 归属库。**有外键**，指向 `media_library.id`。
    pub library_id: i32,
}

impl NewDownloadClient {
    fn validate(&self) -> Result<(), DbError> {
        if self.name.trim().is_empty() {
            return Err(DbError::business(CLIENT_ENTITY, "name 不能为空"));
        }
        Ok(())
    }

    fn normalized(&self) -> Result<(&str, &str, i32), DbError> {
        self.validate()?;
        let config = self
            .provider_config
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(EMPTY_JSON_OBJECT);
        Ok((self.name.trim(), config, self.library_id))
    }
}

/// `download_client` 表仓储。
#[derive(Debug, Clone)]
pub struct DownloadClientRepository {
    pool: PgPool,
}

impl DownloadClientRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<DownloadClient>, DbError> {
        Ok(
            sqlx::query_as::<_, DownloadClient>("SELECT * FROM download_client WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按库名查询。走 `download_client_name_idx`。
    pub async fn find_by_name(&self, name: &str) -> Result<Option<DownloadClient>, DbError> {
        Ok(
            sqlx::query_as::<_, DownloadClient>("SELECT * FROM download_client WHERE name = $1")
                .bind(name.trim())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 插入。
    pub async fn insert(&self, new: &NewDownloadClient) -> Result<DownloadClient, DbError> {
        let (name, config, library_id) = new.normalized()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, DownloadClient>(
            "INSERT INTO download_client ( \
                 name, provider_config, library_id, created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $4) RETURNING *",
        )
        .bind(name)
        .bind(config)
        .bind(library_id)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(CLIENT_ENTITY))
    }

    /// 事务内变体，供 [`Ctx`] 编排多表写入时使用。
    pub async fn insert_in(
        &self,
        ctx: &mut Ctx<'_>,
        new: &NewDownloadClient,
    ) -> Result<DownloadClient, DbError> {
        let (name, config, library_id) = new.normalized()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, DownloadClient>(
            "INSERT INTO download_client ( \
                 name, provider_config, library_id, created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $4) RETURNING *",
        )
        .bind(name)
        .bind(config)
        .bind(library_id)
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(CLIENT_ENTITY))
    }

    /// 改 `provider_config`。**只改这一列，且不接受 `None`。**
    ///
    /// 单独成方法而不是通用 `update`：该列是 `NOT NULL`，通用路径能绑
    /// `None` 进去。空白串按「没有配置」处理，落 `'{}'`。
    pub async fn set_provider_config(
        &self,
        id: i32,
        config: &str,
    ) -> Result<DownloadClient, DbError> {
        let trimmed = config.trim();
        let value = if trimmed.is_empty() {
            EMPTY_JSON_OBJECT
        } else {
            trimmed
        };
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, DownloadClient>(
            "UPDATE download_client SET provider_config = $2, updated_at = $3 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(value)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(CLIENT_ENTITY, id))
    }

    /// 换绑到另一个库。
    ///
    /// **级联后果**：目标库被删除时这个下载器会跟着消失，而它名下所有
    /// `download_task` 也会跟着消失（两跳 CASCADE）。所以这是个需要调用方
    /// 明确决定的操作，单独成方法。
    pub async fn move_to_library(
        &self,
        id: i32,
        library_id: i32,
    ) -> Result<DownloadClient, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, DownloadClient>(
            "UPDATE download_client SET library_id = $2, updated_at = $3 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(library_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(CLIENT_ENTITY, id))
    }

    paged_list! {
        /// 列出全部下载器。**分页。**
        ///
        /// 按 `name` 排序，与唯一索引前缀一致，翻页稳定。
        pub async fn list(&self) -> Result<Page<DownloadClient>, DbError> {
            count = "SELECT COUNT(*) FROM download_client",
            items = "SELECT * FROM download_client ORDER BY name LIMIT $1 OFFSET $2",
        }
    }

    paged_list! {
        /// 按库过滤。**分页。**
        ///
        /// 「这个库下有哪些下载器」—— 库详情页。`library_id` 上**没有**索引
        /// （只有 `name` 有），走全表扫；下载器数量是两位数量级，可接受。
        pub async fn list_by_library(&self, library_id: i32) -> Result<Page<DownloadClient>, DbError> {
            count = "SELECT COUNT(*) FROM download_client WHERE library_id = $1",
            items = "SELECT * FROM download_client WHERE library_id = $1 \
                     ORDER BY name LIMIT $2 OFFSET $3",
        }
    }

    /// 列出全部下载器客户端，**不分页**，按 `created_at DESC, id DESC`。
    ///
    /// 上游 `DownloadClientService.list_clients`（`client_config_service.py:257-265`）
    /// 就是 `DownloadClient.select().order_by(created_at.desc(), id.desc())` ——
    /// **不分页，且最新在前**。骨架期那条注释写的是「按 id 升序」，与上游相反。
    ///
    /// 与 [`Self::list`] 的区别：那个分页，这个给 `GET /download-clients`
    /// （上游那个端点返回**裸数组**，没有分页信封）。
    pub async fn list_ordered(&self) -> Result<Vec<DownloadClient>, DbError> {
        Ok(sqlx::query_as::<_, DownloadClient>(
            "SELECT * FROM download_client ORDER BY created_at DESC, id DESC",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// 删下载器。返回是否真的删掉了一行。
    ///
    /// **连带删除**它名下的全部 `download_task` 与 `indexer_download_client`
    /// 绑定（schema 的 `ON DELETE CASCADE`）。仓储不阻止这件事 ——
    /// 下载历史随下载器一起消失是既定设计，调用方要删之前自己先查。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM download_client WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}
/// 新建一个 Torznab 索引器。
#[derive(Debug, Clone)]
pub struct NewIndexer {
    /// 全局唯一。
    pub name: String,
    /// Torznab 搜索接口地址。
    pub url: String,
    /// `pt` / `bt`。**数据库无 CHECK 约束**，刻意不校验 ——
    /// 上游没约束，仓储层加校验会在上游扩展时把写入挡掉。
    pub kind: String,
    /// 每个索引器独立的鉴权 key。`None` 表示搜索请求不带 `apikey`
    /// 参数 —— 那不是缺失值，而是 Torznab 协议允许的形态。
    pub api_key: Option<String>,
}

impl NewIndexer {
    fn validate(&self) -> Result<(), DbError> {
        for (name, value) in [
            ("name", &self.name),
            ("url", &self.url),
            ("kind", &self.kind),
        ] {
            if value.trim().is_empty() {
                return Err(DbError::business(
                    INDEXER_ENTITY,
                    format!("{name} 不能为空"),
                ));
            }
        }
        Ok(())
    }

    fn normalized(&self) -> Result<(&str, &str, &str, Option<&str>), DbError> {
        self.validate()?;
        Ok((
            self.name.trim(),
            self.url.trim(),
            self.kind.trim(),
            // 空白 api_key 归一成 None：Torznab 协议里「不带参数」与
            // 「带一个空参数」不等价，前者才是「这个索引器不要鉴权」。
            self.api_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        ))
    }
}

/// `indexer` 表仓储。
#[derive(Debug, Clone)]
pub struct IndexerRepository {
    pool: PgPool,
}

/// 在任意连接（池或事务）上插入一个索引器，返回新建的行。
///
/// # 为什么要抽出来
///
/// 三个调用点需要同一段 INSERT，连接来源却不同：`insert` 走池、`insert_in`
/// 走调用方给的事务、`replace_all` 走自己开的事务（`Ctx::in_tx` 的入口
/// `UnitOfWork::ctx()` 是私有的，所以那条路拿不到 `Ctx`）。
///
/// 抽出来后，「整表替换」与「单条插入」用的是**同一段 SQL 与同一套校验** ——
/// 而整表替换**不能**有一套更宽松的规则（那会让它能写出 `insert` 拒绝的数据）。
async fn insert_on(conn: &mut PgConnection, new: &NewIndexer) -> Result<Indexer, DbError> {
    let (name, url, kind, api_key) = new.normalized()?;
    let now = crate::common::time::now_utc();
    sqlx::query_as::<_, Indexer>(
        "INSERT INTO indexer (name, url, kind, api_key, created_at, updated_at) \
         VALUES ($1, $2, $3, $4, $5, $5) RETURNING *",
    )
    .bind(name)
    .bind(url)
    .bind(kind)
    .bind(api_key)
    .bind(now)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| DbError::from(e).with_entity(INDEXER_ENTITY))
}
impl IndexerRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 列出**全部**索引器，按 `id` 升序。**刻意不分页。**
    ///
    /// 配置页要的是整张表 —— 分页会让它只看到前 20 个，而
    /// `PATCH /indexer-settings` 是**整表替换**语义：客户端基于这个列表
    /// 提交全文，被截断的列表会导致保存一次就丢掉其余索引器。
    ///
    /// `ORDER BY id` 与上游 `Indexer.select().order_by(Indexer.id.asc())` 一致，
    /// 所以「保存再读回」的顺序不变 —— 客户端的乐观更新（按位置比对）才成立。
    pub async fn list_all(&self) -> Result<Vec<Indexer>, DbError> {
        Ok(
            sqlx::query_as::<_, Indexer>("SELECT * FROM indexer ORDER BY id")
                .fetch_all(&self.pool)
                .await?,
        )
    }

    /// 读出 `name → api_key` 的映射。**给「省略 api_key 时沿用旧值」用。**
    ///
    /// 一次查询而不是逐个 `find_by_name` —— 整表替换时每个索引器都要查一次，
    /// 那是 N+1。
    ///
    /// 键是**原始 `name`**（未 casefold）：调用方按自己刚归一过的名字查，
    /// 而写入时的 `name` 就是那个归一值。
    pub async fn name_to_api_key(&self) -> Result<Vec<(String, Option<String>)>, DbError> {
        Ok(sqlx::query_as::<_, (String, Option<String>)>(
            "SELECT name, api_key FROM indexer ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// **原子地**把索引器整表替换为给定的一组（含绑定关系）。
    ///
    /// 语义就是上游 `_replace_indexers`：中间表与索引器全删，再逐条重建。
    /// 这是配置页「保存」的动作 —— 页面上删掉一个索引器，保存后它就该消失。
    ///
    /// # 为什么整表替换而不是增量 diff
    ///
    /// 因为请求体是**整张表**（`indexers: [...]` 是完整列表）。做增量需要
    /// 先在库里比对出「哪些是新增、哪些要改、哪些要删」，而那些判断依据
    /// （name 是不是身份？id 变了算不算同一个？）上游并没有定义 ——
    /// 它选的语义就是「你给什么就是什么」。跟着上游走，别自己发明 diff。
    ///
    /// # 一个事务，不是一串操作
    ///
    /// 全删再全插若不包在事务里，中途失败会留下**空表或半张表**，而用户
    /// 只是点了一次保存。`indexer_download_client` 挂 CASCADE，所以删
    /// indexer 会连带清空绑定 —— 上游因此显式先删中间表，这里照做：
    /// 让「先删绑定」成为可读的一步，而不是依赖级联的副作用。
    ///
    /// 返回新建的索引器行（按 `items` 的顺序）。
    pub async fn replace_all(
        &self,
        items: &[(NewIndexer, Vec<i32>)],
    ) -> Result<Vec<Indexer>, DbError> {
        // 自己开事务而不是走 `Ctx`：`Ctx::over_pool` 不开事务（语句自动提交），
        // 而拿到事务内 `Ctx` 的入口 `UnitOfWork::ctx()` 是私有的。
        // 整表替换必须是原子的，所以这里直接持有 `Transaction`。
        let mut tx = self.pool.begin().await?;

        // 先删中间表再删索引器：前者挂 CASCADE，后者会连带清空绑定。
        // 显式先删是为了让这一步可读，而不是依赖级联的副作用。
        sqlx::query("DELETE FROM indexer_download_client")
            .execute(&mut *tx)
            .await
            .map_err(|e| DbError::from(e).with_entity(INDEXER_ENTITY))?;
        sqlx::query("DELETE FROM indexer")
            .execute(&mut *tx)
            .await
            .map_err(|e| DbError::from(e).with_entity(INDEXER_ENTITY))?;

        let mut created = Vec::with_capacity(items.len());
        for (new, client_ids) in items {
            // 与 `insert` / `insert_in` 同一个归一与校验 —— 整表替换不该有
            // 一套更宽松的规则。
            let indexer = insert_on(&mut tx, new).await?;
            for client_id in client_ids {
                sqlx::query(
                    "INSERT INTO indexer_download_client (indexer_id, download_client_id) \
                     VALUES ($1, $2)",
                )
                .bind(indexer.id)
                .bind(client_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| DbError::from(e).with_entity(INDEXER_ENTITY))?;
            }
            created.push(indexer);
        }

        // 提交失败会在这里返回 Err，调用方拿不到半张表。事务在 `tx` 被 drop
        // 时由 sqlx 自动回滚，所以中途任何 `?` 提前返回都是安全的。
        tx.commit().await?;
        Ok(created)
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<Indexer>, DbError> {
        Ok(
            sqlx::query_as::<_, Indexer>("SELECT * FROM indexer WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按名查询。走 `indexer_name_idx`。
    pub async fn find_by_name(&self, name: &str) -> Result<Option<Indexer>, DbError> {
        Ok(
            sqlx::query_as::<_, Indexer>("SELECT * FROM indexer WHERE name = $1")
                .bind(name.trim())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    pub async fn insert(&self, new: &NewIndexer) -> Result<Indexer, DbError> {
        let mut conn = self.pool.acquire().await?;
        insert_on(&mut conn, new).await
    }

    pub async fn insert_in(&self, ctx: &mut Ctx<'_>, new: &NewIndexer) -> Result<Indexer, DbError> {
        let mut conn = ctx.conn().await?;
        insert_on(conn.as_conn(), new).await
    }

    /// 换接口地址。
    pub async fn set_url(&self, id: i32, url: &str) -> Result<Indexer, DbError> {
        let url = url.trim();
        if url.is_empty() {
            return Err(DbError::business(INDEXER_ENTITY, "url 不能为空"));
        }
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, Indexer>(
            "UPDATE indexer SET url = $2, updated_at = $3 WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(url)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(INDEXER_ENTITY, id))
    }

    /// 换/清鉴权 key。传空串或 `None` 表示清除（该列可空）。
    pub async fn set_api_key(&self, id: i32, api_key: Option<&str>) -> Result<Indexer, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, Indexer>(
            "UPDATE indexer SET api_key = $2, updated_at = $3 WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(api_key.map(str::trim).filter(|s| !s.is_empty()))
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(INDEXER_ENTITY, id))
    }

    paged_list! {
        /// 列出全部索引器。**分页。** 按 `name` 排序，与索引前缀一致。
        pub async fn list(&self) -> Result<Page<Indexer>, DbError> {
            count = "SELECT COUNT(*) FROM indexer",
            items = "SELECT * FROM indexer ORDER BY name LIMIT $1 OFFSET $2",
        }
    }

    paged_list! {
        /// 按类型过滤（`pt` / `bt`）。**分页。**
        ///
        /// 「只列出种子索引器」这类筛选。`kind` 上无索引，全表扫。
        pub async fn list_by_kind(&self, kind: &str) -> Result<Page<Indexer>, DbError> {
            count = "SELECT COUNT(*) FROM indexer WHERE kind = $1",
            items = "SELECT * FROM indexer WHERE kind = $1 \
                     ORDER BY name LIMIT $2 OFFSET $3",
        }
    }

    /// 删索引器。连带删除它的全部 `indexer_download_client` 绑定。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM indexer WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}
/// `indexer_download_client` 表仓储：索引器与下载器的多对多绑定。
#[derive(Debug, Clone)]
pub struct IndexerDownloadClientRepository {
    pool: PgPool,
}

impl IndexerDownloadClientRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 一趟 JOIN 取回**全部**绑定关系：`(indexer_id, download_client_id, name)`。
    ///
    /// 按绑定行 `id` 升序 —— 与上游
    /// `IndexerDownloadClient.select(...).order_by(IndexerDownloadClient.id.asc())`
    /// 一致。顺序有实际意义：提交下载时「同 kind 内按绑定顺序挑选」，
    /// 所以这个顺序是**行为**而不是排版。
    ///
    /// 返回三元组而不是 `DownloadClient`：配置页只需要 `id` 与 `name`，
    /// 而 `provider_config`（可能含 cookie）不该为了取个名字被带出来。
    ///
    /// **刻意不分页**：调用方要按 `indexer_id` 分组后填进每一个索引器，
    /// 分页会让某些索引器的绑定列表被截断。
    pub async fn list_all_with_clients(&self) -> Result<Vec<(i32, i32, String)>, DbError> {
        Ok(sqlx::query_as::<_, (i32, i32, String)>(
            "SELECT b.indexer_id, b.download_client_id, c.name \
             FROM indexer_download_client b \
             JOIN download_client c ON c.id = b.download_client_id \
             ORDER BY b.id",
        )
        .fetch_all(&self.pool)
        .await?)
    }

    /// 该索引器绑定的下载器客户端**完整行**，按关联行 `id` 升序。
    ///
    /// 与 [`Self::list_all_with_clients`] 的区别是**取什么**：那个只回
    /// `(id, name)`（配置页列表用，不把可能含 cookie 的 `provider_config`
    /// 带出来），这里回完整行 —— 提交下载时要构造插件句柄，而句柄必须带
    /// `provider_config` 与 `library_id`。
    ///
    /// 上游 `list_indexer_clients`（`downloads/common.py:147-156`）取的是
    /// **`link.download_client`**，也就是完整客户端行，同样按关联行 id 升序。
    pub async fn list_clients_by_indexer(
        &self,
        indexer_id: i32,
    ) -> Result<Vec<DownloadClient>, DbError> {
        Ok(sqlx::query_as::<_, DownloadClient>(
            "SELECT c.* FROM indexer_download_client b \
             JOIN download_client c ON c.id = b.download_client_id \
             WHERE b.indexer_id = $1 \
             ORDER BY b.id",
        )
        .bind(indexer_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 该下载器客户端是否还被任何索引器绑定。
    ///
    /// 上游 `delete_client` 的第二道 409（`client_config_service.py:342-350`）：
    /// `IndexerDownloadClient.select().where(download_client == id).exists()`。
    pub async fn exists_for_client(&self, download_client_id: i32) -> Result<bool, DbError> {
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM indexer_download_client WHERE download_client_id = $1)",
        )
        .bind(download_client_id)
        .fetch_one(&self.pool)
        .await?;
        Ok(exists)
    }

    /// 绑定索引器与下载器。**幂等。**
    ///
    /// 唯一索引 `(indexer_id, download_client_id)` 让重复绑定可以写成
    /// `ON CONFLICT DO NOTHING`，所以调用方（配置同步、UI 保存）天然会
    /// 重复提交时**不会**拿到约束违例。
    ///
    /// 返回值是「这次是否真的新建了一行」—— `false` 表示已经绑过。
    /// 调用方需要区分「绑好了」与「本来就有」时用得上。
    pub async fn bind(&self, indexer_id: i32, download_client_id: i32) -> Result<bool, DbError> {
        let now = crate::common::time::now_utc();
        let result = sqlx::query(
            "INSERT INTO indexer_download_client ( \
                 indexer_id, download_client_id, created_at, updated_at \
             ) VALUES ($1, $2, $3, $3) ON CONFLICT (indexer_id, download_client_id) \
             DO NOTHING",
        )
        .bind(indexer_id)
        .bind(download_client_id)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(BINDING_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 解绑。返回是否真的删掉了一行。
    pub async fn unbind(&self, indexer_id: i32, download_client_id: i32) -> Result<bool, DbError> {
        let result = sqlx::query(
            "DELETE FROM indexer_download_client \
             WHERE indexer_id = $1 AND download_client_id = $2",
        )
        .bind(indexer_id)
        .bind(download_client_id)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() > 0)
    }
    paged_list! {
        /// 列出某个索引器绑定的全部下载器。**分页。**
        ///
        /// 「这个索引器搜到的种子交给哪个下载器」—— 提交前的必经查询。
        /// 走唯一索引 `(indexer_id, download_client_id)` 的前缀。
        ///
        /// `SELECT c.*` 里 `c` 是别名，但 PostgreSQL 返回的列名仍是表列名，
        /// 所以 `DownloadClient` 的 `FromRow` 照常映射。
        pub async fn list_clients_of_indexer(
            &self,
            indexer_id: i32,
        ) -> Result<Page<DownloadClient>, DbError> {
            count = "SELECT COUNT(*) FROM download_client c \
                     JOIN indexer_download_client b ON b.download_client_id = c.id \
                     WHERE b.indexer_id = $1",
            items = "SELECT c.* FROM download_client c \
                     JOIN indexer_download_client b ON b.download_client_id = c.id \
                     WHERE b.indexer_id = $1 ORDER BY c.name LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 列出某个下载器接受的全部索引器。**分页。**
        ///
        /// 「这个下载器能从哪里接单」—— 提交时的候选集。
        pub async fn list_indexers_of_client(
            &self,
            download_client_id: i32,
        ) -> Result<Page<Indexer>, DbError> {
            count = "SELECT COUNT(*) FROM indexer i \
                     JOIN indexer_download_client b ON b.indexer_id = i.id \
                     WHERE b.download_client_id = $1",
            items = "SELECT i.* FROM indexer i \
                     JOIN indexer_download_client b ON b.indexer_id = i.id \
                     WHERE b.download_client_id = $1 ORDER BY i.name LIMIT $2 OFFSET $3",
        }
    }

    /// 把某个索引器的绑定**替换**为给定的一组下载器。
    ///
    /// 语义是「这个索引器绑定的下载器就是这几个」—— 适合配置编辑页的
    /// 「保存」：页面上删掉一个勾选，保存后就该真的解绑。
    ///
    /// 与 `MovieTagRepository` 的 `clear_movie` + `link` 组合同构。
    /// 刻意**不**引用一个叫 `replace_all` 的方法：本仓库里没有那个方法，
    /// 写上去会是一条解析不了的链接 —— 而链接点不开正是读者最需要它的时候。
    ///
    /// 先读出现有绑定再逐个解绑 + 逐个绑定，**不**用一条
    /// `DELETE ... WHERE id NOT IN (...)`：后者在列表为空时会退化成
    /// 「解绑这个索引器的全部下载器」——而那恰好是调用方想要的结果，
    /// 可一旦 SQL 写错（比如漏了 WHERE 条件）就是全表删除。逐个走
    /// 的好处是每一步的 rows_affected 都可见。
    pub async fn replace_clients(
        &self,
        indexer_id: i32,
        download_client_ids: &[i32],
    ) -> Result<usize, DbError> {
        let current = sqlx::query_scalar::<_, i32>(
            "SELECT download_client_id FROM indexer_download_client WHERE indexer_id = $1",
        )
        .bind(indexer_id)
        .fetch_all(&self.pool)
        .await?;

        let mut removed = 0;
        for client_id in &current {
            if !download_client_ids.contains(client_id)
                && self.unbind(indexer_id, *client_id).await?
            {
                removed += 1;
            }
        }
        for client_id in download_client_ids {
            self.bind(indexer_id, *client_id).await?;
        }
        Ok(removed)
    }
}
/// `download_resource_blacklist` 表仓储。
///
/// 整张表只有一列有业务含义：`info_hash`（`varchar(40) NOT NULL UNIQUE`）。
/// 规则是「这个资源不��再下」，没有状态、没有来源、没有备注 —— 上游就是
/// 一张纯集合表。
#[derive(Debug, Clone)]
pub struct DownloadResourceBlacklistRepository {
    pool: PgPool,
}

impl DownloadResourceBlacklistRepository {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 该 info hash 是否被拉黑。**走唯一索引。**
    ///
    /// 这是提交前的**第一道**检查：命中就应该换源，而不是提交出去再等
    /// 下载器拒绝。
    pub async fn contains(&self, info_hash: &str) -> Result<bool, DbError> {
        let hit = sqlx::query_scalar::<_, i32>(
            "SELECT 1 FROM download_resource_blacklist WHERE info_hash = $1",
        )
        .bind(info_hash.trim())
        .fetch_optional(&self.pool)
        .await?;
        Ok(hit.is_some())
    }

    /// 按 info hash 查询整行。
    pub async fn find_by_hash(
        &self,
        info_hash: &str,
    ) -> Result<Option<DownloadResourceBlacklist>, DbError> {
        Ok(sqlx::query_as::<_, DownloadResourceBlacklist>(
            "SELECT * FROM download_resource_blacklist WHERE info_hash = $1",
        )
        .bind(info_hash.trim())
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 加入黑名单。
    ///
    /// **返回是否真的新增** —— `ON CONFLICT DO NOTHING` 让重复拉黑是
    /// 幂等的。调用方在批量拉黑时能靠它知道哪几条本来就在黑名单里。
    ///
    /// 拒绝非法 hash（不是 40 位 hex）而不是让它进库：这一列要与
    /// `download_submission_record.info_hash` 比对，长度或字符集不对就
    /// 永远匹配不上，等于写了一条没用的规则。
    pub async fn add(&self, info_hash: &str) -> Result<bool, DbError> {
        let hash = info_hash.trim();
        if !crate::transfers::downloads::is_valid_info_hash(hash) {
            return Err(DbError::business(
                BLACKLIST_ENTITY,
                format!("info_hash 不是合法的 40 位 v1 hash: {hash:?}"),
            ));
        }
        let now = crate::common::time::now_utc();
        let result = sqlx::query(
            "INSERT INTO download_resource_blacklist (info_hash, created_at, updated_at) \
             VALUES ($1, $2, $2) ON CONFLICT (info_hash) DO NOTHING",
        )
        .bind(hash)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(BLACKLIST_ENTITY))?;
        Ok(result.rows_affected() > 0)
    }

    /// 移出黑名单。返回是否真的删掉了一行。
    pub async fn remove(&self, info_hash: &str) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM download_resource_blacklist WHERE info_hash = $1")
            .bind(info_hash.trim())
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    paged_list! {
        /// 列出黑名单，最新在前。**分页。**
        ///
        /// 按 `created_at DESC` 而不是 `info_hash` —— 运维看的是
        /// 「最近拉黑了什么」，不是字典序。
        pub async fn list(&self) -> Result<Page<DownloadResourceBlacklist>, DbError> {
            count = "SELECT COUNT(*) FROM download_resource_blacklist",
            items = "SELECT * FROM download_resource_blacklist \
                     ORDER BY created_at DESC, id DESC LIMIT $1 OFFSET $2",
        }
    }

    /// 批量判断哪些已被拉黑。返回**命中**的那部分。
    ///
    /// 比逐个 [`Self::contains`] 好：一次往返而不是 N 次。
    /// 返回命中而不是「每个输入对应一个布尔」—— 调用方要的就是
    /// 「这批里哪些要跳过」。
    ///
    /// 空列表直接返回空 Vec，不发查询。
    pub async fn filter_blacklisted(&self, info_hashes: &[String]) -> Result<Vec<String>, DbError> {
        if info_hashes.is_empty() {
            return Ok(Vec::new());
        }
        let rows = sqlx::query_scalar::<_, String>(
            "SELECT info_hash FROM download_resource_blacklist \
             WHERE info_hash = ANY($1)",
        )
        .bind(info_hashes)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn new_client(name: &str) -> NewDownloadClient {
        NewDownloadClient {
            name: name.to_owned(),
            provider_config: None,
            library_id: 1,
        }
    }

    fn new_indexer(name: &str) -> NewIndexer {
        NewIndexer {
            name: name.to_owned(),
            url: "http://indexer.example/api".to_owned(),
            kind: "pt".to_owned(),
            api_key: None,
        }
    }

    #[test]
    fn blank_required_fields_are_rejected() {
        assert!(new_client("客户端").normalized().is_ok());
        let mut blank = new_client("c");
        blank.name = "   ".to_owned();
        assert!(blank.normalized().is_err(), "空名");

        assert!(new_indexer("idx").normalized().is_ok());
        for blank in ["", "  ", "\t"] {
            let mut i = new_indexer("idx");
            i.name = blank.to_owned();
            assert!(i.normalized().is_err(), "空名");

            let mut i = new_indexer("idx");
            i.url = blank.to_owned();
            assert!(i.normalized().is_err(), "空 url");

            let mut i = new_indexer("idx");
            i.kind = blank.to_owned();
            assert!(i.normalized().is_err(), "空 kind");
        }
    }

    #[test]
    fn indexer_kind_is_not_validated_against_a_closed_set() {
        // 上游没有 CHECK 约束，kind 只是 varchar(32)。刻意不校验取值：
        // 上游加一种索引器类型时，旧版本的仓储不该把写入挡掉。
        let mut future = new_indexer("idx");
        future.kind = "future-kind-2099".to_owned();
        assert_eq!(future.normalized().unwrap().2, "future-kind-2099");
    }

    #[test]
    fn missing_provider_config_becomes_an_empty_object() {
        // 列是 `text NOT NULL DEFAULT '{}'`，绑 None 会违反约束。
        let plain = new_client("c");
        let (_, config, _) = plain.normalized().unwrap();
        assert_eq!(config, "{}", "缺失应落空对象");

        let mut blank = new_client("c");
        blank.provider_config = Some("   ".to_owned());
        assert_eq!(blank.normalized().unwrap().1, "{}", "空白串按无配置处理");
    }

    #[test]
    fn provider_config_is_preserved_verbatim() {
        // 宿主只保存与回传，解释权在 provider，所以仓储不校验 JSON ——
        // 校验会挡住 provider 尚未实现的字段。
        let mut c = new_client("c");
        c.provider_config = Some("  {\"cookie\":\"abc\"}  ".to_owned());
        assert_eq!(c.normalized().unwrap().1, "{\"cookie\":\"abc\"}");

        c.provider_config = Some("not json".to_owned());
        assert_eq!(c.normalized().unwrap().1, "not json");
    }

    #[test]
    fn blank_api_key_becomes_none_because_torznab_distinguishes_them() {
        // Torznab 协议里「不带 apikey 参数」与「带一个空参数」不等价，
        // 前者才是「这个索引器不要鉴权」。所以空白归一成 None。
        let plain = new_indexer("idx");
        let (_, _, _, api_key) = plain.normalized().unwrap();
        assert_eq!(api_key, None);

        let mut blank = new_indexer("idx");
        blank.api_key = Some("  ".to_owned());
        assert_eq!(blank.normalized().unwrap().3, None);

        let mut real = new_indexer("idx");
        real.api_key = Some("  key123  ".to_owned());
        assert_eq!(
            real.normalized().unwrap().3,
            Some("key123"),
            "非空白值被 trim 后保留"
        );
    }
}
