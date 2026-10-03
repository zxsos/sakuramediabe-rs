//! `download_task` 表仓储。
//!
//! # 两个状态机用**分离的 setter** 强制，不提供统一入口
//!
//! 上游注释写得很直接：
//!
//! > 导入是宿主自己的业务流程，不能与 provider 的远端状态混用。
//!
//! | 列 | 归属 | 默认值 |
//! |---|---|---|
//! | `state` | provider 的远端下载状态 | `queued` |
//! | `import_status` | 宿主自己的导入流程 | `pending` |
//!
//! 合并成一个 `status` 列会丢掉「下载完了但导入失败」这个**真实存在**
//! 的状态组合 —— 那恰好是最常见的「卡住」形态，需要单独的告警口径。
//!
//! 所以本仓储**刻意不提供** `set_status(fields, values)` 这种通用入口。
//! 只有两个专名方法，想把两个状态混着传在类型层面就不可能：
//!
//! ```text
//! set_state(state, progress, source_ref)   // 只碰 state / progress / completed_source_ref
//! set_import_status(status, task_run_id)    // 只碰 import_status / import_task_run_id
//! ```

use sqlx::PgPool;

use crate::common::page::{Page, PageRequest};
use crate::common::update::UpdateSet;
use crate::error::DbError;
use crate::paged_list;
use crate::transfers::downloads::{download_state, import_status, DownloadTask};

use super::movie::{bind_value_exec, safe_sql};

/// 实体名，用于错误分类。
const ENTITY: &str = "DownloadTask";

/// 插入一条下载任务。
#[derive(Debug, Clone)]
pub struct NewDownloadTask {
    pub client_id: i32,
    /// 下载器侧的远端任务 id。
    pub remote_id: String,
    pub name: String,
    /// 影片番号。**允许为空** —— 允许任务早于影片入库，
    /// 所以它不是指向 `Movie` 的外键。
    pub movie_number: Option<String>,
}

impl NewDownloadTask {
    fn validate(&self) -> Result<(), DbError> {
        if self.remote_id.trim().is_empty() {
            return Err(DbError::business(ENTITY, "remote_id 不能为空"));
        }
        Ok(())
    }
}

/// `download_task` 表仓储。
#[derive(Debug, Clone)]
pub struct DownloadTaskRepository {
    pool: PgPool,
}

impl DownloadTaskRepository {
    /// 构造仓储。
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// 底层连接池。
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// 按主键查询。
    pub async fn find_by_id(&self, id: i32) -> Result<Option<DownloadTask>, DbError> {
        Ok(
            sqlx::query_as::<_, DownloadTask>("SELECT * FROM download_task WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await?,
        )
    }

    /// 按 `(client, remote_id)` 查询。
    ///
    /// 唯一索引 `(client, remote_id)` 让重复提交命中约束而非产生第二条
    /// —— 这是幂等提交的基础，所以**先查后插**在这里是安全的。
    /// 按主键查询，未命中返回 [`DbError::NotFound`]。
    pub async fn require_by_id(&self, id: i32) -> Result<DownloadTask, DbError> {
        self.find_by_id(id)
            .await?
            .ok_or_else(|| DbError::not_found(ENTITY, id))
    }

    pub async fn find_by_remote(
        &self,
        client_id: i32,
        remote_id: &str,
    ) -> Result<Option<DownloadTask>, DbError> {
        Ok(sqlx::query_as::<_, DownloadTask>(
            "SELECT * FROM download_task WHERE client_id = $1 AND remote_id = $2",
        )
        .bind(client_id)
        .bind(remote_id)
        .fetch_optional(&self.pool)
        .await?)
    }

    /// 插入。
    ///
    /// `state` 与 `import_status` 都不在这里显式赋值 —— 走数据库
    /// DEFAULT（`queued` / `pending`）。让默认值住在 schema 里而不是代码里，
    /// 这样裸 SQL 插入与仓储插入的初始状态一定一致。
    pub async fn insert(&self, new: &NewDownloadTask) -> Result<DownloadTask, DbError> {
        new.validate()?;

        let row = sqlx::query_as::<_, DownloadTask>(
            "INSERT INTO download_task (client_id, remote_id, name, movie_number, created_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $5) RETURNING *",
        )
        .bind(new.client_id)
        .bind(new.remote_id.trim())
        .bind(new.name.trim())
        .bind(new.movie_number.as_deref().map(str::trim))
        .bind(crate::common::time::now_utc())
        .fetch_one(&self.pool)
        .await
        .map_err(|e| DbError::from(e).with_entity(ENTITY))?;

        Ok(row)
    }

    /// 更新**远端下载状态**。只碰 `state` / `progress` / `completed_source_ref`。
    ///
    /// 刻意不接受 `import_status` —— 两个状态机在 API 形状上分离。
    pub async fn set_state(
        &self,
        id: i32,
        state: &str,
        progress: Option<f64>,
        completed_source_ref: Option<&str>,
    ) -> Result<DownloadTask, DbError> {
        if !matches!(
            state,
            download_state::QUEUED
                | download_state::SUBMITTED
                | download_state::DOWNLOADING
                | download_state::COMPLETED
                | download_state::FAILED
        ) {
            return Err(DbError::business(
                ENTITY,
                format!("未知的远端下载状态 {state}"),
            ));
        }

        let mut set = UpdateSet::new();
        set.set("state", state);
        if let Some(progress) = progress {
            // provider 汇报 0.0–1.0，越界值直接拒而不是让数据库存进去。
            if !(0.0..=1.0).contains(&progress) {
                return Err(DbError::business(
                    ENTITY,
                    format!("progress 必须在 0.0–1.0 之间，收到 {progress}"),
                ));
            }
            set.set("progress", progress);
        }
        if state == download_state::COMPLETED {
            // 完成态必须有产物引用，否则导入侧无从下手。
            let Some(source_ref) = completed_source_ref.filter(|s| !s.trim().is_empty()) else {
                return Err(DbError::business(
                    ENTITY,
                    "state=completed 必须提供 completed_source_ref",
                ));
            };
            set.set("completed_source_ref", source_ref.trim());
        }

        self.persist(id, set).await
    }

    /// 更新**宿主导入状态**。只碰 `import_status` / `import_task_run_id`。
    ///
    /// 刻意不接受 `state` —— 见模块文档。
    pub async fn set_import_status(
        &self,
        id: i32,
        status: &str,
        import_task_run_id: Option<i64>,
    ) -> Result<DownloadTask, DbError> {
        if !matches!(
            status,
            import_status::PENDING
                | import_status::RUNNING
                | import_status::DONE
                | import_status::FAILED
        ) {
            return Err(DbError::business(
                ENTITY,
                format!("未知的导入状态 {status}"),
            ));
        }

        let mut set = UpdateSet::new();
        set.set("import_status", status);
        if let Some(run_id) = import_task_run_id {
            set.set("import_task_run_id", run_id);
        }

        self.persist(id, set).await
    }

    /// 领取一条处于 `queued` 的任务。
    ///
    /// 单语句 `UPDATE ... WHERE state = 'queued' ... RETURNING` 靠行锁排他，
    /// 多个 worker 并发调用时只有一方能拿到这一行；「先 SELECT 再 UPDATE」
    /// 则会重复领取。
    pub async fn claim_queued(&self) -> Result<Option<DownloadTask>, DbError> {
        let now = crate::common::time::now_utc();

        // 三个占位符**必须分开**。曾经把子查询的过滤条件也写成 $1，
        // 而 $1 绑的是 'submitted'（写入目标），于是子查询在找「已提交」
        // 的任务而不是排队中的 —— 永远返回 None，不报任何错。
        // 队列非空却领不到任务，排查起来极其困难。
        let row = sqlx::query_as::<_, DownloadTask>(
            "UPDATE download_task SET state = $1, updated_at = $2 \
             WHERE id = ( \
                SELECT id FROM download_task WHERE state = $3 \
                ORDER BY id FOR UPDATE SKIP LOCKED LIMIT 1 \
             ) RETURNING *",
        )
        .bind(download_state::SUBMITTED)
        .bind(now)
        .bind(download_state::QUEUED)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row)
    }

    paged_list! {
        /// 列出「下载完成但导入失败」的任务。**分页。**
        ///
        /// 分页让告警能报出准确数量，而不是「至少 N 条」。
        pub async fn list_stuck_after_download(
            &self,
            state: &str,
            import_status_value: &str,
        ) -> Result<Page<DownloadTask>, DbError> {
            count = "SELECT COUNT(*) FROM download_task \
                     WHERE state = $1 AND import_status = $2",
            items = "SELECT * FROM download_task \
                     WHERE state = $1 AND import_status = $2 \
                     ORDER BY updated_at LIMIT $3 OFFSET $4",
        }
    }

    /// 通用更新。仅供本模块内部使用 —— 它不区分状态机，
    /// 所以**不导出**。
    async fn persist(&self, id: i32, mut set: UpdateSet<'_>) -> Result<DownloadTask, DbError> {
        set.touch();
        // 字段从 $1 起、id 放最后 —— 与 SET/WHERE 的书写顺序一致。
        let assignments = set.assignments(1);
        let fields = set.finish(ENTITY)?;
        let sql = format!(
            "UPDATE download_task SET {assignments} WHERE id = ${}",
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_task(remote_id: &str) -> NewDownloadTask {
        NewDownloadTask {
            client_id: 1,
            remote_id: remote_id.to_owned(),
            name: "n".to_owned(),
            movie_number: None,
        }
    }

    #[test]
    fn empty_remote_id_is_rejected() {
        assert!(new_task("r").validate().is_ok());
        assert!(new_task("   ").validate().is_err());
    }

    #[test]
    fn movie_number_may_be_absent_because_task_precedes_movie() {
        // 搜索结果先于刮削入库是正常流程，所以 movie_number 可空且非外键。
        let t = new_task("r");
        assert!(t.movie_number.is_none());
        assert!(t.validate().is_ok(), "番号为空不应阻止创建任务");
    }

    #[test]
    fn the_two_state_machines_stay_disjoint() {
        // setter 各自只认自己那组字面量。下面这个断言的意义是：
        // 就算将来有人想「统一」两个 setter，这些常量仍然是两套 ——
        // 合并它们会丢掉 state=completed & import_status=failed 这个组合。
        let download_terminal = [download_state::COMPLETED, download_state::FAILED];
        let import_terminal = [import_status::DONE, import_status::FAILED];

        for state in download_terminal {
            assert!(download_state::is_terminal(state));
        }
        for status in import_terminal {
            assert!(import_status::is_terminal(status));
        }

        // 关键交叉：远端失败不影响导入状态，反之亦然。
        assert!(!import_status::is_terminal(download_state::DOWNLOADING));
        assert!(!download_state::is_terminal(import_status::RUNNING));
    }
}
