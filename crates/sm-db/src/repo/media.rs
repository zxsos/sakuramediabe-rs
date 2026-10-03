//! `media` 表仓储。
//!
//! # XOR 归属不变量由**仓储层**强制，不靠数据库
//!
//! 上游是在 Python 的 `save()` 里 `raise ValueError`：
//!
//! ```python
//! if (self.movie_number is None) == (self.video_item_id is None):
//!     raise ValueError("Media must belong to exactly one of movie / video_item")
//! ```
//!
//! **DDL 里没有对应的 CHECK。** 完全可以加
//! `CHECK ((movie_number IS NULL) <> (video_item_id IS NULL))`，
//! 但本项目选择不加 —— schema 与上游同构优先于本地优化，校验放在
//! 仓储层已经足够。
//!
//! 这样做还有个好处：报错是 [`DbError::Business`]（422）而不是
//! [`DbError::ConstraintViolation`]（409），因为「这条 Media 没归属」
//! 是业务错误，不是状态冲突。

use chrono::NaiveDateTime;
use sqlx::PgPool;

use crate::common::update::UpdateSet;
use crate::error::DbError;
use crate::playback::media::{thumbnail_state, Media};

use super::movie::{bind_value_exec, safe_sql};

/// 实体名，用于错误分类。
const ENTITY: &str = "Media";

/// 插入一条媒体。
#[derive(Debug, Clone)]
pub struct NewMedia {
    pub library_id: i32,
    pub file_name: String,
    pub file_size_bytes: i64,
    /// 指向 `Movie.movie_number`（**字符串**），与 `video_item_id` 恰好其一非空。
    pub movie_number: Option<String>,
    /// 指向 `VideoItem.id`。DDL 是 `integer`，所以是 `i32` 而非 `i64`。
    pub video_item_id: Option<i32>,
    /// 不透明存储引用，结构由 provider 定义。
    pub storage_ref: Option<String>,
    pub resolution: Option<String>,
    /// `media-file-hash-v1:<40 hex>`。
    pub file_hash: Option<String>,
    pub import_source_identity: Option<String>,
    /// DDL 里是 `integer`，模型是 `i32` —— 与 `Media` 结构体保持一致。
    pub duration_seconds: Option<i32>,
    /// `JsonTextField`，写入时序列化为文本。
    pub video_info: Option<serde_json::Value>,
}

impl NewMedia {
    /// 写入前的全部业务校验。
    ///
    /// 两项都归到这里而不是散在 `insert` 里：原先空文件名检查内联在
    /// `insert` 中，导致单元测试只能测到 `str::trim`，实际拒绝逻辑
    /// 一行都没被执行过。合成一个入口后，测试可以直接断言错误类型。
    fn validate(&self) -> Result<(), DbError> {
        self.check_owner()?;

        if self.file_name.trim().is_empty() {
            return Err(DbError::business(ENTITY, "file_name 不能为空"));
        }
        Ok(())
    }

    /// 校验 XOR 归属，返回业务错误。
    fn check_owner(&self) -> Result<(), DbError> {
        if self.movie_number.is_some() == self.video_item_id.is_some() {
            return Err(DbError::business(
                ENTITY,
                "Media 必须恰好归属 movie（JAV）或 video_item（非 JAV）之一：\
                 两者都空或都非空都被拒绝",
            ));
        }
        Ok(())
    }
}

/// `media` 表仓储。
#[derive(Debug, Clone)]
pub struct MediaRepository {
    pool: PgPool,
}

impl MediaRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<Media>, DbError> {
        Ok(
            sqlx::query_as::<_, Media>("SELECT * FROM media WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按主键查询，未命中返回 [`DbError::NotFound`]。
    pub async fn require_by_id(&self, id: i32) -> Result<Media, DbError> {
        self.find_by_id(id)
            .await?
            .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    /// 插入。**写入前校验 XOR 归属与文件名。**
    pub async fn insert(&self, new: &NewMedia) -> Result<Media, DbError> {
        new.validate()?;

        let sql = "\
            INSERT INTO media (
                movie_number, video_item_id, library_id, storage_ref, file_name,
                resolution, file_size_bytes, file_hash, import_source_identity,
                duration_seconds, video_info, valid,
                thumbnail_generation_state, created_at, updated_at
            ) VALUES (
                $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $14
            ) RETURNING *";

        let row = sqlx::query_as::<_, Media>(sql)
            .bind(new.movie_number.as_deref().map(str::trim))
            .bind(new.video_item_id)
            .bind(new.library_id)
            .bind(new.storage_ref.as_deref())
            .bind(new.file_name.trim())
            .bind(new.resolution.as_deref())
            .bind(new.file_size_bytes)
            .bind(new.file_hash.as_deref())
            .bind(new.import_source_identity.as_deref())
            .bind(new.duration_seconds)
            // JsonTextField 是 TEXT 列：序列化成字符串，空串不写。
            .bind(new.video_info.as_ref().map(|v| v.to_string()))
            // `valid` 的 DB 默认值是 true，这里不重复表达。
            .bind(true)
            .bind(thumbnail_state::PENDING)
            .bind(crate::common::time::now_utc())
            .fetch_one(&self.pool)
            .await
            .map_err(|e| DbError::from(e).with_entity(ENTITY))?;

        debug_assert!(
            row.satisfies_owner_constraint(),
            "插入后 XOR 不变量必须成立，否则说明 check_owner 漏了"
        );

        Ok(row)
    }

    /// 更新。
    ///
    /// 若本次写入会碰到归属列，先做 XOR 预判再落库 —— 否则错误会以
    /// 外键冲突（409）的形式漏出来，而不是业务错误（422）。
    pub async fn update(&self, id: i32, mut set: UpdateSet<'_>) -> Result<Media, DbError> {
        let touches_owner = set
            .fields()
            .iter()
            .any(|(name, _)| *name == "movie_number" || *name == "video_item_id");

        if touches_owner {
            let current = self.require_by_id(id).await?;
            let mut movie_number = current.movie_number;
            let mut video_item_id = current.video_item_id;

            for (name, value) in set.fields() {
                match *name {
                    "movie_number" => movie_number = as_opt_text(value),
                    "video_item_id" => video_item_id = as_opt_int(value),
                    _ => {}
                }
            }

            // 与插入路径同一套判定，复用同一条消息。
            let probe = NewMedia {
                library_id: 0,
                file_name: String::new(),
                file_size_bytes: 0,
                movie_number,
                video_item_id,
                storage_ref: None,
                resolution: None,
                file_hash: None,
                import_source_identity: None,
                duration_seconds: None,
                video_info: None,
            };
            probe.check_owner()?;
        }

        set.touch();
        // 字段从 $1 起、id 放最后 —— 与 SET/WHERE 的书写顺序一致，
        // 读者不需要在脑子里做逆序映射。
        let assignments = set.assignments(1);
        let fields = set.finish(ENTITY)?;
        let sql = format!(
            "UPDATE media SET {assignments} WHERE id = ${}",
            fields.len() + 1
        );

        let query = fields
            .iter()
            .fold(sqlx::query(safe_sql(sql)), |query, (_, value)| {
                bind_value_exec(query, value)
            });
        let result = query.bind(id).execute(&self.pool).await?;

        if result.rows_affected() == 0 {
            return Err(DbError::not_found(ENTITY, id));
        }
        self.require_by_id(id).await
    }

    /// 列出待生成缩略图的媒体。
    ///
    /// 索引是 `(thumbnail_generation_state, thumbnail_next_retry_at)`，
    /// 所以只有 `retry_wait` 且已到期的行会被这个查询命中 —— 与
    /// [`thumbnail_state::is_retryable`] 的口径一致。
    pub async fn list_pending_thumbnails(&self, limit: i64) -> Result<Vec<Media>, DbError> {
        let now = crate::common::time::now_utc();
        let rows = sqlx::query_as::<_, Media>(
            "SELECT * FROM media \
             WHERE thumbnail_generation_state = $1 \
               AND (thumbnail_next_retry_at IS NULL OR thumbnail_next_retry_at <= $2) \
             ORDER BY thumbnail_next_retry_at NULLS FIRST, id \
             LIMIT $3",
        )
        .bind(thumbnail_state::RETRY_WAIT)
        .bind(now)
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }

    /// 记录一次缩略图生成失败。
    ///
    /// 失败后退避等待重试（`retry_wait`），而不是直接进终态 ——
    /// 网络抖动、存储暂时不可用都属于可恢复情形。
    pub async fn record_thumbnail_failure(
        &self,
        id: i32,
        error_code: &str,
        next_retry_at: NaiveDateTime,
    ) -> Result<Media, DbError> {
        let row = sqlx::query_as::<_, Media>(
            "UPDATE media SET \
                thumbnail_generation_state = $2, \
                thumbnail_attempt_count = thumbnail_attempt_count + 1, \
                thumbnail_last_error_code = $3, \
                thumbnail_last_error = $3, \
                thumbnail_next_retry_at = $4, \
                updated_at = $5 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(thumbnail_state::RETRY_WAIT)
        .bind(error_code)
        .bind(next_retry_at)
        .bind(crate::common::time::now_utc())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))?;

        Ok(row)
    }

    /// 标记缩略图生成成功（进入终态）。
    pub async fn record_thumbnail_success(&self, id: i32) -> Result<Media, DbError> {
        let row = sqlx::query_as::<_, Media>(
            "UPDATE media SET \
                thumbnail_generation_state = $2, \
                thumbnail_last_error_code = NULL, \
                thumbnail_last_error = NULL, \
                thumbnail_next_retry_at = NULL, \
                thumbnail_terminal_at = $3, \
                updated_at = $3 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(thumbnail_state::SUCCEEDED)
        .bind(crate::common::time::now_utc())
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))?;

        Ok(row)
    }
}

/// 从 UpdateSet 的值里取可选文本。
fn as_opt_text(value: &crate::common::update::Value<'_>) -> Option<String> {
    match &*value.0 {
        crate::common::update::ValueInner::Text(v) => Some(v.clone()),
        _ => None,
    }
}

/// 从 UpdateSet 的值里取可选整数（`video_item_id` 是 `integer` 列）。
fn as_opt_int(value: &crate::common::update::Value<'_>) -> Option<i32> {
    match &*value.0 {
        crate::common::update::ValueInner::Int(v) => Some(*v as i32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_media(movie: Option<&str>, video: Option<i32>) -> NewMedia {
        NewMedia {
            library_id: 1,
            file_name: "a.mp4".to_owned(),
            file_size_bytes: 0,
            movie_number: movie.map(str::to_owned),
            video_item_id: video,
            storage_ref: None,
            resolution: None,
            file_hash: None,
            import_source_identity: None,
            duration_seconds: None,
            video_info: None,
        }
    }

    #[test]
    fn xor_rejects_both_empty_and_both_present() {
        assert!(new_media(Some("ABC-001"), None).check_owner().is_ok());
        assert!(new_media(None, Some(7)).check_owner().is_ok());

        let both = new_media(Some("ABC-001"), Some(7));
        let err = both.check_owner().unwrap_err();
        assert!(matches!(err, DbError::Business { .. }), "应为业务错误(422)");
        assert!(err.to_string().contains("恰好归属"));

        let neither = new_media(None, None);
        assert!(neither.check_owner().is_err());
    }

    #[test]
    fn empty_file_name_is_rejected_before_touching_db() {
        // 断言的是 `validate()` 的行为，不是 `str::trim` 的行为。
        // 原先这里只写了 `assert!(m.file_name.trim().is_empty())` ——
        // 一个恒真断言，`insert` 里的拒绝逻辑从未被执行过。
        let mut m = new_media(Some("ABC-001"), None);
        m.file_name = "   ".to_owned();
        let err = m.validate().expect_err("空白 file_name 应被拒绝");
        assert!(matches!(err, DbError::Business { .. }), "应为业务错误(422)");
        assert!(err.to_string().contains("file_name"), "{err}");

        // 归属错误优先于文件名错误：XOR 是更根本的不变量。
        let mut both = new_media(Some("ABC-001"), Some(7));
        both.file_name = "  ".to_owned();
        assert!(both
            .validate()
            .unwrap_err()
            .to_string()
            .contains("恰好归属"));

        // 正常输入通过
        assert!(new_media(Some("ABC-001"), None).validate().is_ok());
    }

    #[test]
    fn failure_keeps_media_retryable_while_success_is_terminal() {
        // 失败进 retry_wait（可重试），成功进 succeeded（终态）。
        assert!(thumbnail_state::is_retryable(thumbnail_state::RETRY_WAIT));
        assert!(!thumbnail_state::is_retryable(thumbnail_state::SUCCEEDED));
        assert!(thumbnail_state::is_valid(thumbnail_state::SUCCEEDED));
    }
}
