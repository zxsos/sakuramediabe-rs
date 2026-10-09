//! `download_submission_record` 表仓储：**提交历史**。
//!
//! # 这张表记的是「尝试过」，不是「下载中」
//!
//! 一次提交分两步：向索引器/下载器发出请求（`submitting`），拿到
//! 远端任务 id（`submitted`，带上 `remote_id`）或失败（带 `error_code`）。
//! 实际的下载进度由 [`DownloadTask`](crate::transfers::downloads::DownloadTask)
//! 跟踪，两者用 `remote_id` 关联。
//!
//! # `client_id` / `task_id` 是裸整数，没有外键 —— 这是刻意的
//!
//! 上游注释写得很直白：「保留提交历史，不随下载任务或下载器删除」。
//! 提交历史必须独立于它引用的东西存活，否则清理下载器时历史会连带消失，
//! 而审计与排查恰恰依赖这些记录。
//!
//! 代价是**引用完整性由应用负责**，数据库不兜底：任务被删后 `task_id`
//! 变成悬空整数（模型上的 `is_orphaned()` 就是这个意思），`client_id`
//! 也可以指向一个不存在的下载器。所以 `record_submission` 要显式传
//! `client_id`，由调用方保证它有意义。
//!
//! # 幂等提交靠的是 `download_task` 的唯一索引，不是这张表
//!
//! `download_task` 上有 `UNIQUE (client_id, remote_id)`。本表**没有**
//! `(client_id, remote_id)` 唯一索引，只有 `task_id` 的普通索引 ——
//! 所以同一个 remote_id 提交两次会留下两行。
//!
//! 这是有意的：它记的是提交**尝试**的次数，而第一次尝试可能失败、
//! 第二次成功，两条记录都是真实发生的事。
//! [`DownloadSubmissionRepository::find_successful_by_remote`] 给出的
//! 是「最终成功的那次」，与「一共有几次尝试」是两个问题。

use sqlx::PgPool;

use crate::common::page::{Page, PageRequest};
use crate::error::DbError;
use crate::paged_list;
use crate::transfers::downloads::DownloadSubmissionRecord;

use super::ctx::Ctx;

const ENTITY: &str = "DownloadSubmissionRecord";

/// 提交状态字面量。
///
/// 与 `download_task` 的状态机分开定义：那边是「下载生命周期」，这边是
/// 「一次提交尝试的生命周期」，取值不同、迁移规则也不同。
pub mod submission_state {
    /// 已发出请求，等待下载器返回。**列的 DEFAULT。**
    pub const SUBMITTING: &str = "submitting";
    /// 拿到 `remote_id`，可以交给 `download_task` 跟踪。
    pub const SUBMITTED: &str = "submitted";
    /// 提交被拒或出错，`error_code` 说明原因。
    pub const FAILED: &str = "failed";

    /// 全部合法取值。
    pub const ALL: [&str; 3] = [SUBMITTING, SUBMITTED, FAILED];

    /// 是否为合法状态字面量。
    pub fn is_valid(state: &str) -> bool {
        ALL.contains(&state)
    }
}
/// 一次提交尝试的入参。
#[derive(Debug, Clone)]
pub struct NewSubmissionRecord {
    /// 下载器 id。**裸整数，无外键** —— 调用方保证它指向存在的下载器。
    pub client_id: i32,
    /// 关联的下载任务。`None` 表示这次提交还没落成任务。
    pub task_id: Option<i32>,
    pub movie_number: String,
    /// 哪个索引器/下载站。用于排查「哪个站最常失败」。
    pub indexer_name: String,
    pub title: String,
    /// 磁力链接或种子文件地址。
    pub source_uri: String,
    /// 40 位 v1 info hash。
    pub info_hash: String,
}

impl NewSubmissionRecord {
    pub(crate) fn validate(&self) -> Result<(), DbError> {
        for (name, value) in [
            ("movie_number", &self.movie_number),
            ("indexer_name", &self.indexer_name),
            ("title", &self.title),
            ("source_uri", &self.source_uri),
        ] {
            if value.trim().is_empty() {
                return Err(DbError::business(
                    ENTITY,
                    format!("{name} 不能为空：提交历史要能独立说明「提交了什么」"),
                ));
            }
        }
        // 40 位 hex 是与黑名单表共用的约定：写成别的长度就永远匹配不上。
        if !crate::transfers::downloads::is_valid_info_hash(&self.info_hash) {
            return Err(DbError::business(
                ENTITY,
                "info_hash 必须是 40 位 v1 hash（十六进制），否则无法与资源黑名单比对",
            ));
        }
        Ok(())
    }
}

/// `download_submission_record` 表仓储。
#[derive(Debug, Clone)]
pub struct DownloadSubmissionRepository {
    pool: PgPool,
}

impl DownloadSubmissionRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// info_hash 去首尾空白。
    ///
    /// 上游这列不是 `CaseSensitiveCharField`（对比 `actor.javdb_id`），
    /// 所以**不**折叠大小写 —— 改写大小写会破坏与黑名单表的比对。
    fn trimmed_hash(hash: &str) -> String {
        hash.trim().to_owned()
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<DownloadSubmissionRecord>, DbError> {
        Ok(sqlx::query_as::<_, DownloadSubmissionRecord>(
            "SELECT * FROM download_submission_record WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 按 `task_id` 查询。**走 `download_submission_record_task_id_idx`。**
    ///
    /// `ORDER BY created_at DESC, id DESC`：一个任务可能有多次提交尝试
    /// （第一次失败、第二次成功），最新的一次排在前面。
    pub async fn list_by_task(
        &self,
        task_id: i32,
    ) -> Result<Vec<DownloadSubmissionRecord>, DbError> {
        Ok(sqlx::query_as::<_, DownloadSubmissionRecord>(
            "SELECT * FROM download_submission_record WHERE task_id = $1 \
             ORDER BY created_at DESC, id DESC",
        )
        .bind(task_id)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 找某个 `remote_id` **最终成功**的那次提交。
    ///
    /// 「成功」用模型上的 `DownloadSubmissionRecord::succeeded()` 判定：
    /// 有 `remote_id` 且没有 `error_code`。
    ///
    /// # 为什么按时间倒序再取一条
    ///
    /// 本表没有 `(client_id, remote_id)` 唯一索引，重复提交会留下多行。
    /// 同一个 remote_id 可能先失败后成功，所以要的是**成功**的那次，
    /// 而它在时间上不一定是最后一条写入 —— 只能靠排序后取第一条来定。
    ///
    /// `client_id` 上无索引，走全表扫。提交历史低频写入低频读取，
    /// 行数按提交次数增长，可接受。
    pub async fn find_successful_by_remote(
        &self,
        client_id: i32,
        remote_id: &str,
    ) -> Result<Option<DownloadSubmissionRecord>, DbError> {
        Ok(sqlx::query_as::<_, DownloadSubmissionRecord>(
            "SELECT * FROM download_submission_record \
             WHERE client_id = $1 AND remote_id = $2 AND error_code IS NULL \
             ORDER BY created_at DESC, id DESC LIMIT 1",
        )
        .bind(client_id)
        .bind(remote_id.trim())
        .fetch_optional(&self.pool)
        .await?)
    }
    /// 记录一次提交尝试。
    ///
    /// `state` **显式**写 `submitting`。列上确实有 `DEFAULT 'submitting'`，
    /// 但那是字面量才被 `gen_ddl.py` 保留下来 —— 对比
    /// `user_refresh_tokens.status` 的 `default=SomeEnum.ACTIVE` 是属性
    /// 引用，解析不出来，那个 DEFAULT 被丢了。显式写入让插入不依赖
    /// 任何 schema 假设。
    pub async fn record_submission(
        &self,
        new: &NewSubmissionRecord,
    ) -> Result<DownloadSubmissionRecord, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, DownloadSubmissionRecord>(
            "INSERT INTO download_submission_record ( \
                 client_id, task_id, movie_number, indexer_name, title, \
                 source_uri, info_hash, state, created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9) RETURNING *",
        )
        .bind(new.client_id)
        .bind(new.task_id)
        .bind(new.movie_number.trim())
        .bind(new.indexer_name.trim())
        .bind(new.title.trim())
        .bind(new.source_uri.trim())
        .bind(Self::trimmed_hash(&new.info_hash))
        .bind(submission_state::SUBMITTING)
        .bind(now)
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))
    }

    /// 事务内变体，供 [`Ctx`] 编排多表写入时使用。
    pub async fn record_submission_in(
        &self,
        ctx: &mut Ctx<'_>,
        new: &NewSubmissionRecord,
    ) -> Result<DownloadSubmissionRecord, DbError> {
        new.validate()?;
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, DownloadSubmissionRecord>(
            "INSERT INTO download_submission_record ( \
                 client_id, task_id, movie_number, indexer_name, title, \
                 source_uri, info_hash, state, created_at, updated_at \
             ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9) RETURNING *",
        )
        .bind(new.client_id)
        .bind(new.task_id)
        .bind(new.movie_number.trim())
        .bind(new.indexer_name.trim())
        .bind(new.title.trim())
        .bind(new.source_uri.trim())
        .bind(Self::trimmed_hash(&new.info_hash))
        .bind(submission_state::SUBMITTING)
        .bind(now)
        .fetch_one(ctx.conn().await?.as_conn())
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))
    }

    /// 标记提交成功，写入 `remote_id`。
    ///
    /// 只有 `submitting` / `failed` 能走到成功 —— 已成功的行再改一次
    /// 说明调用方逻辑有问题，**让它失败**而不是悄悄覆盖 `remote_id`：
    /// 覆盖会丢掉「最早是哪次提交成功的」这个信息。
    pub async fn mark_submitted(
        &self,
        id: i32,
        remote_id: &str,
    ) -> Result<DownloadSubmissionRecord, DbError> {
        let remote_id = remote_id.trim();
        if remote_id.is_empty() {
            return Err(DbError::business(
                ENTITY,
                "remote_id 不能为空：成功提交必须有远端任务 id，\
                 否则 `download_task` 那边无从挂上",
            ));
        }
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, DownloadSubmissionRecord>(
            "UPDATE download_submission_record \
             SET state = $2, remote_id = $3, error_code = NULL, updated_at = $4 \
             WHERE id = $1 AND state IN ($5, $6) RETURNING *",
        )
        .bind(id)
        .bind(submission_state::SUBMITTED)
        .bind(remote_id)
        .bind(now)
        .bind(submission_state::SUBMITTING)
        .bind(submission_state::FAILED)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            DbError::business(
                ENTITY,
                format!("提交记录 {id} 不在 submitting/failed 状态，不能标记为已提交"),
            )
        })
    }

    /// 标记提交失败，写入 `error_code`。
    ///
    /// 同样只允许从 `submitting` / `failed` 迁移 —— 终态不可覆盖，
    /// 理由同 [`Self::mark_submitted`]。
    pub async fn mark_failed(
        &self,
        id: i32,
        error_code: &str,
    ) -> Result<DownloadSubmissionRecord, DbError> {
        let error_code = error_code.trim();
        if error_code.is_empty() {
            return Err(DbError::business(ENTITY, "error_code 不能为空"));
        }
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, DownloadSubmissionRecord>(
            "UPDATE download_submission_record \
             SET state = $2, error_code = $3, updated_at = $4 \
             WHERE id = $1 AND state IN ($5, $6) RETURNING *",
        )
        .bind(id)
        .bind(submission_state::FAILED)
        .bind(error_code)
        .bind(now)
        .bind(submission_state::SUBMITTING)
        .bind(submission_state::FAILED)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| {
            DbError::business(
                ENTITY,
                format!("提交记录 {id} 不在 submitting/failed 状态，不能标记为失败"),
            )
        })
    }

    /// 关联下载任务。
    ///
    /// 单独成方法而不是通用 `update`：`task_id` 的语义是「这次提交落成了
    /// 哪个任务」，而且**没有外键** —— 写进去的 id 不保证指向存在的行。
    pub async fn attach_task(
        &self,
        id: i32,
        task_id: Option<i32>,
    ) -> Result<DownloadSubmissionRecord, DbError> {
        let now = crate::common::time::now_utc();
        sqlx::query_as::<_, DownloadSubmissionRecord>(
            "UPDATE download_submission_record SET task_id = $2, updated_at = $3 \
             WHERE id = $1 RETURNING *",
        )
        .bind(id)
        .bind(task_id)
        .bind(now)
        .fetch_optional(&self.pool)
        .await?
        .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    paged_list! {
        /// 按状态过滤。**分页。**
        ///
        /// 「有哪些提交卡在 `submitting` 一直没回来」是排查索引器问题的
        /// 第一个问题。`state` 上无索引，走全表扫；这张表按提交次数增长，
        /// 读取低频。
        pub async fn list_by_state(&self, state: &str) -> Result<Page<DownloadSubmissionRecord>, DbError> {
            count = "SELECT COUNT(*) FROM download_submission_record WHERE state = $1",
            items = "SELECT * FROM download_submission_record WHERE state = $1 \
                     ORDER BY created_at DESC, id DESC LIMIT $2 OFFSET $3",
        }
    }

    paged_list! {
        /// 按下载器过滤，最新在前。**分页。**
        ///
        /// 「这个下载器最近提交了什么」—— 排查某个下载器专有问题时用。
        /// `client_id` 无索引，全表扫。
        pub async fn list_by_client(&self, client_id: i32) -> Result<Page<DownloadSubmissionRecord>, DbError> {
            count = "SELECT COUNT(*) FROM download_submission_record WHERE client_id = $1",
            items = "SELECT * FROM download_submission_record WHERE client_id = $1 \
                     ORDER BY created_at DESC, id DESC LIMIT $2 OFFSET $3",
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn new_record() -> NewSubmissionRecord {
        NewSubmissionRecord {
            client_id: 1,
            task_id: None,
            movie_number: "ABC-001".to_owned(),
            indexer_name: "example-indexer".to_owned(),
            title: "Example Title".to_owned(),
            source_uri: "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567".to_owned(),
            info_hash: "0123456789abcdef0123456789abcdef01234567".to_owned(),
        }
    }

    #[test]
    fn blank_descriptive_fields_are_rejected() {
        assert!(new_record().validate().is_ok());

        // 提交历史要能独立说明「提交了什么」，所以这几个字段一个都不能空。
        // 尤其 `indexer_name` 与 `title` 不是查询键 —— 缺了的话事后看
        // 「哪个站、哪个片」就没了，而那正是这张表存在的理由。
        for blank in ["", "   ", "\t\n"] {
            let mut r = new_record();
            r.movie_number = blank.to_owned();
            assert!(r.validate().is_err(), "空 movie_number");

            let mut r = new_record();
            r.indexer_name = blank.to_owned();
            assert!(r.validate().is_err(), "空 indexer_name");

            let mut r = new_record();
            r.title = blank.to_owned();
            assert!(r.validate().is_err(), "空 title");

            let mut r = new_record();
            r.source_uri = blank.to_owned();
            assert!(r.validate().is_err(), "空 source_uri");
        }
    }

    #[test]
    fn info_hash_must_be_forty_hex_chars() {
        // 与资源黑名单表同一套规则。写成别的长度就永远匹配不上黑名单，
        // 所以宁可在入口拒绝。
        for bad in [
            "".to_owned(),
            "abc".to_owned(),
            "g".repeat(40),
            "0".repeat(39),
            "0".repeat(41),
        ] {
            let mut r = new_record();
            r.info_hash = bad.clone();
            assert!(r.validate().is_err(), "非法 info_hash 应被拒: {bad:?}");
        }

        // 大写 hex 也是合法 v1 hash —— 上游这列不是 CaseSensitiveCharField，
        // 所以校验不能拒大写，否则与黑名单表的比对会少一半。
        let mut ok = new_record();
        ok.info_hash = "ABCDEF0123456789ABCDEF0123456789ABCDEF01".to_owned();
        assert!(ok.validate().is_ok(), "大写 hex 合法");
    }

    #[test]
    fn info_hash_is_trimmed_but_case_is_preserved() {
        // 只去空白，不折叠大小写：改写大小写会破坏与黑名单表的比对，
        // 而黑名单那侧是普通 CharField。
        assert_eq!(
            DownloadSubmissionRepository::trimmed_hash("  ABCdef01  "),
            "ABCdef01"
        );
    }

    #[test]
    fn state_literals_match_the_column_default() {
        // `state` 的 DEFAULT 是字面量 'submitting'，所以 gen_ddl 保留了它。
        // 仓储显式写入同一个值 —— 两者必须一致，否则「省略 state 的裸 SQL
        // 插入」和「仓储插入」会落在不同状态上。
        assert!(submission_state::is_valid(submission_state::SUBMITTING));
        assert!(submission_state::is_valid("submitted"));
        assert!(submission_state::is_valid("failed"));
        assert!(!submission_state::is_valid("pending"));
        assert!(!submission_state::is_valid(""));
        assert_eq!(submission_state::ALL.len(), 3);
    }
}
