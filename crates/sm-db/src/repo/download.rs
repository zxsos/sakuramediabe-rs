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

use sqlx::{PgPool, Postgres, QueryBuilder};

use crate::common::page::{in_snapshot_tx, verify_page_shape, Page, PageRequest};
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

/// 台账列表的过滤条件（上游 `list_tasks` 的三个可选筛选位，`task_service.py:43-72`）。
///
/// 三个字段都是「不传 = 不筛」：
///
/// | 字段 | 上游 | 语义 |
/// |---|---|---|
/// | `client_id` | `DownloadTask.client == client_id` | 只看某个下载器名下 |
/// | `movie_number` | `DownloadTask.movie == value` | **精确匹配**（不是 LIKE）|
/// | `states` | `DownloadTask.state.in_(...)` | 状态白名单 |
#[derive(Debug, Clone, Default)]
pub struct DownloadTaskFilter {
    /// 只看某个下载器名下的任务。`None` = 不筛。
    pub client_id: Option<i32>,
    /// 影片番号。**精确匹配** —— 上游 `build_task_movie_filter`
    /// （`downloads/common.py:224-225`）是 `DownloadTask.movie == value`，
    /// **不是 `LIKE`**。空白串按不筛处理。
    pub movie_number: Option<String>,
    /// 远端下载状态白名单（`state = ANY(...)`）。`None` = 不筛。
    ///
    /// 服务层保证不会传 `Some(空)`：上游对空集合归 `None`（不筛）。
    pub states: Option<Vec<String>>,
}

/// 台账排序键（闭集；上游 `TASK_SORT_FIELDS`，`downloads/common.py:37-44`）。
///
/// `ORDER BY` 片段**写死**在这里：字段名来自封闭枚举，类型上就排除了
/// 「用户输入拼进 SQL」。次级排序恒为 `id` **同向** —— 否则同分的任务
/// 翻页顺序不稳定，一条会重复出现在两页里（`progress DESC` 尤其常见）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DownloadTaskSort {
    /// 上游缺省键 `created_at:desc`。
    #[default]
    CreatedAtDesc,
    CreatedAtAsc,
    UpdatedAtDesc,
    UpdatedAtAsc,
    ProgressDesc,
    ProgressAsc,
}

impl DownloadTaskSort {
    /// 六个变体，供遍历与一致性测试。
    pub const ALL: [DownloadTaskSort; 6] = [
        Self::CreatedAtDesc,
        Self::CreatedAtAsc,
        Self::UpdatedAtDesc,
        Self::UpdatedAtAsc,
        Self::ProgressDesc,
        Self::ProgressAsc,
    ];

    /// 白名单字面量（`field:dir`）。
    pub fn key(self) -> &'static str {
        match self {
            Self::CreatedAtDesc => "created_at:desc",
            Self::CreatedAtAsc => "created_at:asc",
            Self::UpdatedAtDesc => "updated_at:desc",
            Self::UpdatedAtAsc => "updated_at:asc",
            Self::ProgressDesc => "progress:desc",
            Self::ProgressAsc => "progress:asc",
        }
    }

    /// `ORDER BY` 片段。**主序 + `id` 同向次级**。
    pub fn order_by(self) -> &'static str {
        match self {
            Self::CreatedAtDesc => "created_at DESC, id DESC",
            Self::CreatedAtAsc => "created_at ASC, id ASC",
            Self::UpdatedAtDesc => "updated_at DESC, id DESC",
            Self::UpdatedAtAsc => "updated_at ASC, id ASC",
            Self::ProgressDesc => "progress DESC, id DESC",
            Self::ProgressAsc => "progress ASC, id ASC",
        }
    }

    /// 由 `field:dir` 字面量解析（**调用方须已归一为小写并 trim**）。
    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|sort| sort.key() == key)
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
    /// 该下载器客户端名下**有没有任何**任务行。
    ///
    /// 上游 `delete_client` 的第一道 409（`client_config_service.py:335-341`）用的是
    /// `DownloadTask.select().where(client == id).exists()` —— **不是**「有没有在跑的
    /// 任务」，而是「有没有任何历史任务」。差别很实际：下载记录是随下载器一起
    /// `CASCADE` 掉的，所以上游要拦住的是「你还想留着历史吗」，而不是「任务还在跑吗」。
    pub async fn exists_for_client(&self, client_id: i32) -> Result<bool, DbError> {
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM download_task WHERE client_id = $1)")
                .bind(client_id)
                .fetch_one(&self.pool)
                .await?;
        Ok(exists)
    }

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

    /// 台账列表的一页（上游 `DownloadTaskService.list_tasks` 的查询部分，
    /// `task_service.py:53-63`）。
    ///
    /// 过滤 + 排序 + 分页都在**同一个 `REPEATABLE READ` 快照**里：`COUNT` 与
    /// `SELECT` 共用同一段 `WHERE`，否则「共 N 条」会与当页描述的不是同一个
    /// 集合（客户端据此决定要不要继续翻页）。
    ///
    /// `sort` 是闭集枚举，`ORDER BY` 片段写死，无注入面。
    pub async fn list_page(
        &self,
        filter: &DownloadTaskFilter,
        sort: DownloadTaskSort,
        page: PageRequest,
    ) -> Result<Page<DownloadTask>, DbError> {
        // `in_snapshot_tx` 的闭包是 `for<'c>`，必须持有 owned 值（见
        // `common::page` 的 `PageArg` 文档）。filter 很小，克隆一次即可。
        let filter = filter.clone();
        let result = in_snapshot_tx(&self.pool, move |conn| {
            Box::pin(async move {
                let mut count =
                    QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM download_task t");
                push_download_task_filter(&mut count, &filter);
                let total: i64 = count
                    .build_query_scalar::<i64>()
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(|e| DbError::from(e).with_entity(ENTITY))?;

                let mut items = QueryBuilder::<Postgres>::new("SELECT t.* FROM download_task t");
                push_download_task_filter(&mut items, &filter);
                items.push(" ORDER BY ").push(sort.order_by());
                items
                    .push(" LIMIT ")
                    .push_bind(page.limit())
                    .push(" OFFSET ")
                    .push_bind(page.offset());
                let rows = items
                    .build_query_as::<DownloadTask>()
                    .fetch_all(&mut *conn)
                    .await
                    .map_err(|e| DbError::from(e).with_entity(ENTITY))?;

                Ok(Page::new(rows, total))
            })
        })
        .await?;
        verify_page_shape(&result, &page, ENTITY)?;
        Ok(result)
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
    ///
    /// # 五个合法取值都放行，包括 `skipped`
    ///
    /// 此前这里只认四个（漏了 `skipped`），于是「这一趟没有可导入的媒体
    /// 文件」这个**正常结果**写不进去 —— 只能记成 `failed`。客户端的六分类
    /// 里 `skipped` 是独立一档，写不进去就意味着那一档永远是 0。
    pub async fn set_import_status(
        &self,
        id: i32,
        status: &str,
        import_task_run_id: Option<i64>,
    ) -> Result<DownloadTask, DbError> {
        if !import_status::is_valid(status) {
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

    /// **整批**占用一批下载任务去做导入 —— **全有或全无**。返回受影响行数。
    ///
    /// 对应上游 `ImportTaskService.enqueue_batch` 里那两行
    /// （`import_task_service.py:149-165`）：
    ///
    /// ```python
    /// updated_count = (
    ///     DownloadTask.update(import_status=RUNNING, import_task_run=task_run)
    ///     .where(DownloadTask.id.in_(ids), DownloadTask.import_status == PENDING)
    ///     .execute()
    /// )
    /// if updated_count != len(download_tasks):
    ///     raise ApiError(409, "download_task_import_conflict", ...)
    /// ```
    ///
    /// 上游那两步在同一个事务里，所以「数量不符」会**回滚**整批。本仓用
    /// **单条带计数谓词的 UPDATE** 达到同一个效果：只要有一条不在 `pending`，
    /// 计数谓词不成立，`rows_affected` 就是 **0** —— 一条都不改，调用方拿到 0
    /// 之后自己返回 409。既不需要回滚，也不需要跨表事务。
    ///
    /// 计数走的是**同一条语句的快照**（PostgreSQL 的子查询看不见本语句已做的
    /// 修改），所以「值不符 → 0 行」这个判据不会因为中途改到一半而漂移。哪怕
    /// 对快照语义的判断有误，失效方向也是 **fail-closed**（多报一次 409），
    /// 不会出现「一半改了、一半没改」。
    ///
    /// # 为什么是「返回行数」而不是在这里报 409
    ///
    /// 「有任务已被别的导入占用」是**业务冲突**，上游的码
    /// （`download_task_import_conflict`）与文案都属于 service 层。仓储层
    /// 自己报错只会压成 `DbError::business`，调用方再也拼不回那个响应。
    ///
    /// # 只碰 `import_status` 与 `import_task_run_id`
    ///
    /// 与 [`Self::set_import_status`] 同一个理由：`state` 是 provider 的远端
    /// 状态，两个状态机不混。
    ///
    /// # `import_task_run_id` 是 `i64`
    ///
    /// 与 [`Self::set_import_status`] 一致（列是 `integer`，赋值时有隐式转换）。
    pub async fn mark_import_started(&self, ids: &[i32], task_run_id: i64) -> Result<u64, DbError> {
        if ids.is_empty() {
            // 空批次不发查询：`= ANY('{}')` 恒假，直接返回 0 省一次往返。
            return Ok(0);
        }
        let now = crate::common::time::now_utc();
        let expected = i64::try_from(ids.len()).unwrap_or(i64::MAX);
        let result = sqlx::query(
            "UPDATE download_task AS t \
                SET import_status = $2, import_task_run_id = $3, updated_at = $4 \
              WHERE t.id = ANY($1) \
                AND t.import_status = $5 \
                AND (SELECT COUNT(*) FROM download_task AS c \
                      WHERE c.id = ANY($1) AND c.import_status = $5) = $6",
        )
        .bind(ids)
        .bind(import_status::RUNNING)
        .bind(task_run_id)
        .bind(now)
        .bind(import_status::PENDING)
        .bind(expected)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
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

/// 把 [`DownloadTaskFilter`] 的三个筛选位推到 builder 上（COUNT 与 SELECT 共用）。
///
/// 先 `WHERE TRUE` 再逐个 `AND` —— 避免「第一个条件是否要带 `WHERE`」的分支，
/// 也让两段 SQL 无条件共用同一段代码（口径漂移正是 `total` 失真的根源）。
fn push_download_task_filter(builder: &mut QueryBuilder<Postgres>, filter: &DownloadTaskFilter) {
    builder.push(" WHERE TRUE");
    if let Some(client_id) = filter.client_id {
        builder.push(" AND t.client_id = ").push_bind(client_id);
    }
    if let Some(movie_number) = filter
        .movie_number
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        // **精确匹配**（上游 `build_task_movie_filter`），不是 LIKE。
        builder
            .push(" AND t.movie_number = ")
            .push_bind(movie_number.to_owned());
    }
    if let Some(states) = &filter.states {
        builder
            .push(" AND t.state = ANY(")
            .push_bind(states.clone())
            .push(")");
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
        let import_terminal = [
            import_status::COMPLETED,
            import_status::FAILED,
            import_status::SKIPPED,
        ];

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
