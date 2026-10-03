//! `media_library` 表仓储。
//!
//! # 为什么现在才有
//!
//! `media.library_id` 指向本表，所以**没有它就不能写 `media`** ——
//! 这不是「锦上添花」的缺口，而是阻塞整条播放链路的地基。
//! 之前 `media_family_integration.rs` 的 24 个测试里有一半直接失败在
//! `media_library_id_fk` 上，那些测试自己写裸 SQL 造父行，注释还写着
//! 「`media_library` 还没有仓储，这正是缺口的样子」。
//!
//! # `provider_config` 不可空这件事
//!
//! 列是 `provider_config text NOT NULL DEFAULT '{}'`（上游
//! `JsonTextField(default=dict)`，没有 `null=True`），但模型声明的是
//! `Option<String>` —— 对拍对 `text/json` 列的**可空性**放行，理由与
//! `compare_schema.py` 里的类型映射一致：那列落的是 TEXT，只是内容是
//! JSON 文本，`Option` 表达「调用方没提供」。
//!
//! 代价是调用方可能真的绑 `None` 进去，而那会违反 NOT NULL。所以本仓储
//! 的每个写入口都**兜成 `'{}'`**，与 `task.rs` 对 `result_summary`、
//! `media.rs` 对 `storage_ref` 的处理一致。`Option` 在这里只表达意图，
//! 不表达「允许存 NULL」。
//!
//! 上游自己也有同样的陷阱：`JsonTextField.db_value` 遇到 `None` 会
//! 返回 `None`，而列是 NOT NULL。

use sqlx::PgPool;

use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;
use crate::playback::media::MediaLibrary;

use super::ctx::Ctx;

const ENTITY: &str = "MediaLibrary";

/// `provider_config` 的空值形态，与列的 `DEFAULT '{}'` 一致。
const EMPTY_JSON_OBJECT: &str = "{}";

/// 新建一个媒体库。
#[derive(Debug, Clone)]
pub struct NewMediaLibrary {
    /// 库名。**全局唯一**（`unique + index`）。
    pub name: String,
    /// 决定用哪个 provider 实现解释 `provider_config`。
    ///
    /// 刻意**不**校验取值：库里没有 CHECK 约束，上游也没有枚举，
    /// 新 provider 落地时不该被旧版本的仓储挡住。
    pub provider_key: String,
    /// 不透明 JSON 文本。`None` 落库为 `'{}'`。
    pub provider_config: Option<String>,
    /// 多账号存储（115 等）用它区分 cookie 归属。
    pub account_key: Option<String>,
}

impl NewMediaLibrary {
    fn validate(&self) -> Result<(), DbError> {
        if self.name.trim().is_empty() {
            return Err(DbError::business(ENTITY, "name 不能为空"));
        }
        if self.provider_key.trim().is_empty() {
            return Err(DbError::business(
                ENTITY,
                "provider_key 不能为空：它决定由哪个 provider 解释 provider_config",
            ));
        }
        Ok(())
    }

    /// 归一后的插入参数。`provider_config` 的空白与 `None` 都落成 `'{}'`。
    fn normalized(&self) -> Result<(&str, &str, &str, Option<&str>), DbError> {
        self.validate()?;
        let config = self
            .provider_config
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(EMPTY_JSON_OBJECT);
        Ok((
            self.name.trim(),
            self.provider_key.trim(),
            config,
            self.account_key
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty()),
        ))
    }
}

/// `media_library` 表仓储。
#[derive(Debug, Clone)]
pub struct MediaLibraryRepository {
    pool: PgPool,
}

impl MediaLibraryRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<MediaLibrary>, DbError> {
        Ok(
            sqlx::query_as::<_, MediaLibrary>("SELECT * FROM media_library WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按库名查询。`name` 是 `unique + index`，走索引。
    pub async fn find_by_name(&self, name: &str) -> Result<Option<MediaLibrary>, DbError> {
        Ok(
            sqlx::query_as::<_, MediaLibrary>("SELECT * FROM media_library WHERE name = $1")
                .bind(name.trim())
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 插入。
    pub async fn insert(&self, new: &NewMediaLibrary) -> Result<MediaLibrary, DbError> {
        let (name, provider_key, config, account_key) = new.normalized()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MediaLibrary>(
            "INSERT INTO media_library ( \
                 name, provider_key, provider_config, account_key, created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $5) RETURNING *",
        )
        .bind(name)
        .bind(provider_key)
        .bind(config)
        .bind(account_key)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))
    }

    /// 事务内插入。供 [`Ctx`] 编排多表写入时使用。
    pub async fn insert_in(
        &self,
        ctx: &mut Ctx<'_>,
        new: &NewMediaLibrary,
    ) -> Result<MediaLibrary, DbError> {
        let (name, provider_key, config, account_key) = new.normalized()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MediaLibrary>(
            "INSERT INTO media_library ( \
                 name, provider_key, provider_config, account_key, created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $5) RETURNING *",
        )
        .bind(name)
        .bind(provider_key)
        .bind(config)
        .bind(account_key)
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))
    }

    /// 改 `provider_config`。**只改这一列，且不接受 `None`。**
    ///
    /// 单独成方法而不是走通用 update：该列是 `NOT NULL`，通用路径能绑
    /// `None` 进去。空白串按「没有配置」处理，落 `'{}'`。
    pub async fn set_provider_config(
        &self,
        id: i32,
        config: &str,
    ) -> Result<MediaLibrary, DbError> {
        let trimmed = config.trim();
        let value = if trimmed.is_empty() {
            EMPTY_JSON_OBJECT
        } else {
            trimmed
        };
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MediaLibrary>(
            "UPDATE media_library SET provider_config = $2, updated_at = $3 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(value)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    /// 改 `account_key`。传空串或 `None` 表示清除（该列可空）。
    pub async fn set_account_key(
        &self,
        id: i32,
        account_key: Option<&str>,
    ) -> Result<MediaLibrary, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, MediaLibrary>(
            "UPDATE media_library SET account_key = $2, updated_at = $3 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(account_key.map(str::trim).filter(|s| !s.is_empty()))
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    paged_list! {
        /// 列出全部库。**分页。**
        ///
        /// 按 `name` 排序，与 `unique` 索引的前缀一致，翻页稳定。
        pub async fn list(&self) -> Result<Page<MediaLibrary>, DbError> {
            count = "SELECT COUNT(*) FROM media_library",
            items = "SELECT * FROM media_library ORDER BY name LIMIT $1 OFFSET $2",
        }
    }

    paged_list! {
        /// 按 `provider_key` 过滤。**分页。**
        ///
        /// 用于「这个 provider 名下有哪几个库」—— 多账号场景下按
        /// provider 归组展示。`provider_key` 上没有索引，走全表扫；
        /// 库数量是两位数量级，可接受。
        pub async fn list_by_provider(&self, provider_key: &str) -> Result<Page<MediaLibrary>, DbError> {
            count = "SELECT COUNT(*) FROM media_library WHERE provider_key = $1",
            items = "SELECT * FROM media_library WHERE provider_key = $1 \
                     ORDER BY name LIMIT $2 OFFSET $3",
        }
    }

    /// 删库。返回是否真的删掉了一行。
    ///
    /// 已被 `media` 引用的库会怎样由 schema 的 `on_delete` 决定；这里
    /// 不做静默跳过 —— 「这个库还有 N 条媒体」是调用方删之前该知道的，
    /// 库的删除本身是低频操作。
    pub async fn delete(&self, id: i32) -> Result<bool, DbError> {
        let result = sqlx::query("DELETE FROM media_library WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_library(name: &str) -> NewMediaLibrary {
        NewMediaLibrary {
            name: name.to_owned(),
            provider_key: "local".to_owned(),
            provider_config: None,
            account_key: None,
        }
    }

    #[test]
    fn blank_name_or_provider_key_is_rejected() {
        assert!(new_library("主库").normalized().is_ok());

        let mut blank_name = new_library("库");
        blank_name.name = "   ".to_owned();
        assert!(blank_name.normalized().is_err(), "空库名");

        let mut blank_provider = new_library("库");
        blank_provider.provider_key = " \t".to_owned();
        assert!(
            blank_provider.normalized().is_err(),
            "空 provider_key：它决定由谁解释 provider_config"
        );
    }

    #[test]
    fn missing_provider_config_becomes_an_empty_object_not_null() {
        // 该列是 `text NOT NULL DEFAULT '{}'`。绑 None 会违反约束，
        // 所以归一化成 '{}' —— 与 task.rs / media.rs 的处理一致。
        // `normalized()` 返回的引用活不过临时值，所以先绑定。
        let plain = new_library("库");
        let (_, _, config, _) = plain.normalized().unwrap();
        assert_eq!(config, "{}", "缺失应落空对象，而不是 NULL");

        let mut blank = new_library("库");
        blank.provider_config = Some("   ".to_owned());
        let (_, _, config, _) = blank.normalized().unwrap();
        assert_eq!(config, "{}", "空白串按没有配置处理");
    }

    #[test]
    fn provider_config_is_trimmed_but_otherwise_preserved() {
        // 宿主只保存与回传，不解释内容，所以不能在这里做 JSON 校验 ——
        // 校验会挡住 provider 尚未实现的字段。
        let mut lib = new_library("库");
        lib.provider_config = Some("  {\"a\":1}  ".to_owned());
        let (_, _, config, _) = lib.normalized().unwrap();
        assert_eq!(config, "{\"a\":1}");

        lib.provider_config = Some("not json at all".to_owned());
        let (_, _, config, _) = lib.normalized().unwrap();
        assert_eq!(
            config, "not json at all",
            "仓储不校验 JSON：宿主只保存与回传，解释权在 provider"
        );
    }

    #[test]
    fn blank_account_key_normalises_to_none() {
        // `account_key` 可空，空白串归一成 NULL 而不是存一个空值 ——
        // 「没有账号」与「账号是空串」在多账号存储里是不同的事。
        let plain = new_library("库");
        let (_, _, _, account) = plain.normalized().unwrap();
        assert_eq!(account, None);

        let mut blank = new_library("库");
        blank.account_key = Some("  ".to_owned());
        assert_eq!(blank.normalized().unwrap().3, None);

        blank.account_key = Some(" user@example.com ".to_owned());
        assert_eq!(blank.normalized().unwrap().3, Some("user@example.com"));
    }
}
