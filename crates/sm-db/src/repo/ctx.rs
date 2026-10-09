//! 执行上下文：让同一份仓储代码既能走连接池也能走事务。
//!
//! # 为什么需要这个类型
//!
//! 业务用例跨表，而仓储按表组织。审计暴露了后果：
//! `DownloadSubmissionRecord` 没有仓储，可 `download.rs` 的幂等提交
//! 「先查后插」要查的正是那张表 —— 按表切分的仓储**无法表达跨表用例**。
//!
//! 直觉做法是让方法接受 `&mut Transaction`。但那在 sqlx 0.9 里
//! **不成立**：
//!
//! ```text
//! Transaction<'c, DB> 里的 'c 是**连接**的寿命，不是事务的。
//! ```
//!
//! 所以「借它一会儿再 commit」在类型上不成立 —— 这也是
//! [`crate::common::page::in_snapshot_tx`] 接受 `&mut PgConnection`
//! 而不是 `&mut Transaction` 的原因。
//!
//! `Ctx` 绕过这一点：`Transaction` deref 到 `PgConnection`，而
//! `PgConnection` 的生命周期是独立的。所以 `Ctx` 内部存
//! `Option<&mut PgConnection>`：
//!
//! | 构造 | 用途 |
//! |---|---|
//! | [`Ctx::pool`] | 单表操作，自动提交 |
//! | [`Ctx::in_tx`] | 跨表用例，与其他仓储共享一个事务 |
//!
//! # 与既有方法的关系
//!
//! 仓储方法成对提供：`insert` 走自己的 pool，`insert_in` 接受 `Ctx`。
//! 两者共用一个私有实现，所以**不存在两套逻辑漂移**的风险 —— 这是
//! `insert` 存在的全部意义，它只是 `insert_in` 的便利包装。
//!
//! 不给全部 3685 行仓储加 `_in` 变体：只有参与组合写入的那些需要，
//! 其余保持现状。

use sqlx::pool::PoolConnection;
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::error::DbError;
// 只在文档里引用，但为了让 intra-doc link 能解析到，
// 这个 import 必须真实存在。
#[allow(unused_imports)]
use crate::playback::media::image_search_index_status;

use super::asset::{MovieActorRepository, MovieTagRepository, TagRepository};
use super::media::MediaRepository;
use super::movie::{MovieRepository, NewMovie};
use super::playback::MediaThumbnailRepository;

/// 执行上下文。
///
/// # 生命周期
///
/// `'a` 借的是 **pool 或 transaction**。两者都不 outlive `'a`，
/// 所以 `Ctx` 不能跨用例持有 —— 这正是想要的：事务边界由
/// [`UnitOfWork`] 圈定。
pub struct Ctx<'a> {
    pool: &'a PgPool,
    /// `Some` 表示在事务里。`None` 表示直接走池，语句自动提交。
    conn: Option<&'a mut PgConnection>,
}

impl<'a> Ctx<'a> {
    /// 直接走连接池。语句自动提交。
    pub fn over_pool(pool: &'a PgPool) -> Self {
        Self { pool, conn: None }
    }

    /// 在一个事务里执行。
    ///
    /// 注意接收 `&mut Transaction` 而**不是**返回它：调用方仍然持有
    /// 事务句柄并负责 commit / rollback，`Ctx` 只是借用它。
    ///
    /// `pool` 要显式传：sqlx 的 `Transaction` 内部**不保存 pool 句柄**,
    /// 所以没法从 `&mut Transaction` 反推回来。仓储方法偶尔需要
    /// 「不在事务里」的辅助查询,那要靠它。
    pub fn in_tx(tx: &'a mut Transaction<'_, Postgres>, pool: &'a PgPool) -> Self {
        Self {
            pool,
            conn: Some(&mut *tx),
        }
    }

    /// 底层 pool。仓储用它做「不在事务里」的那些辅助查询。
    pub fn pool(&self) -> &PgPool {
        self.pool
    }

    /// 当前连接。事务内返回事务的连接，否则从池里临时取一条。
    ///
    /// # 为什么临时连接会「自动提交」
    ///
    /// 不是自动提交，是**根本不在事务里**。调用方若在 `Ctx::pool` 的
    /// 上下文里做需要原子性的多步写入，那是用例设计错误 —— 没有任何
    /// 东西会替它回滚。
    ///
    /// 取临时连接时用 `PoolConnection` 而不是裸 `PgConnection`：
    /// 前者析构时归还连接，后者一旦归还就变成悬垂引用。
    pub async fn conn(&mut self) -> Result<CtxConnection<'_>, DbError> {
        match self.conn.as_deref_mut() {
            Some(conn) => Ok(CtxConnection::Borrowed(conn)),
            None => {
                let conn = self.pool.acquire().await?;
                Ok(CtxConnection::Pooled(conn))
            }
        }
    }

    /// 是否在事务里。
    pub fn in_transaction(&self) -> bool {
        self.conn.is_some()
    }
}

/// [`Ctx::conn`] 的结果：要么借用事务的连接，要么临时持有一条池连接。
///
/// 之所以要区分而不是统一返回 `&mut PgConnection`：临时连接的所有权
/// 属于这个包装，调用方拿到 `&mut` 之后既不能把它存进结构体，也不能
/// 在 `Ctx` 之外使用 —— 生命周期由类型保证。
pub enum CtxConnection<'c> {
    /// 事务的连接。生命周期与事务一致。
    Borrowed(&'c mut PgConnection),
    /// 从池里临时取的连接。析构时归还。
    Pooled(PoolConnection<Postgres>),
}

impl CtxConnection<'_> {
    /// 转成 sqlx 需要的 `&mut PgConnection`。
    ///
    /// 两个变体都满足这个签名，所以调用方的查询代码在事务内外
    /// **完全一致** —— 这正是要的效果：同一份 SQL 编译两次行为相同。
    pub fn as_conn(&mut self) -> &mut PgConnection {
        match self {
            Self::Borrowed(conn) => conn,
            // `PoolConnection` deref 到 `PgConnection`，返回值类型让
            // auto-deref 完成转换。不写 `as_mut()`：它在 sqlx 版本间
            // 变过签名。
            Self::Pooled(conn) => conn,
        }
    }
}

/// 一次用例执行：所有参与写入的仓储共享同一个事务。
///
/// # 为什么存在
///
/// 仓储按表组织，用例按流程组织。审计暴露了后果：
/// `DownloadSubmissionRecord` 没有仓储，可幂等提交的「先查后插」
/// 要查的正是那张表 —— 按表切分的仓储**无法表达跨表用例**。
///
/// 这个类型是那个缺口的解法：它按**动词**暴露方法，每个方法内部
/// 编排多个仓储的 `_in` 变体。
///
/// | 层 | 方法形态 | 事务 | 对应 |
/// |---|---|---|---|
/// | 仓储 | 动词短语（`upsert`、`find_by_number`） | 无 | 一行数据 |
/// | 用例 | 动词（`generate_thumbnail`） | **有** | 一个 API 端点 |
///
/// # 用法
///
/// ```ignore
/// let mut uow = UnitOfWork::begin(&pool).await?;
/// let thumb = uow.generate_thumbnail(media_id, offset, image_id).await?;
/// uow.commit().await?;
/// ```
///
/// # 提交语义
///
/// 忘记 `commit()` 就等于回滚 —— [`UnitOfWork`] **不实现 `Drop` 里的
/// 自动提交**。这是刻意的：自动提交会让「我以为会回滚」变成
/// 「数据已经写进去了」。
pub struct UnitOfWork<'a> {
    tx: Option<Transaction<'a, Postgres>>,
    pool: &'a PgPool,
}

impl<'a> UnitOfWork<'a> {
    /// 开一个新事务。
    pub async fn begin(pool: &'a PgPool) -> Result<Self, DbError> {
        let tx = pool.begin().await?;
        Ok(Self { tx: Some(tx), pool })
    }

    /// 提交。**只能调用一次。**
    pub async fn commit(mut self) -> Result<(), DbError> {
        match self.tx.take() {
            Some(tx) => Ok(tx.commit().await?),
            None => Err(DbError::business(
                "UnitOfWork",
                "事务已经结束（可能已提交或已回滚）",
            )),
        }
    }

    /// 显式回滚。`commit` 失败后调用它清理连接。
    ///
    /// 事务在 [`Self`] 被 drop 时由 sqlx 自动回滚，所以正常路径下
    /// 不需要显式调用。这个方法是为了「commit 报错后想立刻释放连接」
    /// 这种情况存在的。
    pub async fn rollback(mut self) -> Result<(), DbError> {
        if let Some(tx) = self.tx.take() {
            tx.rollback().await?;
        }
        Ok(())
    }

    /// 取出当前事务供 [`Ctx`] 借用。
    ///
    /// # 为什么是 `expect` 而不是返回 Result
    ///
    /// 事务只在 `commit` / `rollback` 之后消失，而那两个方法
    /// **消耗 `self`**。所以持有 `&mut UnitOfWork` 时事务一定还在。
    /// 真的走到这个 `expect` 说明有代码在 borrow 结束前把 self 移走了,
    /// 那属于逻辑错误而不是运行时状态。
    fn ctx(&mut self) -> Ctx<'_> {
        Ctx::in_tx(
            self.tx
                .as_mut()
                .expect("事务已结束：commit/rollback 消耗 self，之后不可能再借"),
            self.pool,
        )
    }

    /// Media 仓储。它的方法需要显式传 `&mut Ctx` 才会走本事务。
    pub fn media(&self) -> MediaRepository {
        MediaRepository::new(self.pool.clone())
    }

    /// 缩略图仓储。同上。
    pub fn thumbnails(&self) -> MediaThumbnailRepository {
        MediaThumbnailRepository::new(self.pool.clone())
    }

    /// Movie 仓储。同上：方法需要显式传 `&mut Ctx` 才会走本事务。
    pub fn movies(&self) -> MovieRepository {
        MovieRepository::new(self.pool.clone())
    }

    /// 标签仓储。
    pub fn tags(&self) -> TagRepository {
        TagRepository::new(self.pool.clone())
    }

    /// 影片-演员关联仓储。
    pub fn movie_actors(&self) -> MovieActorRepository {
        MovieActorRepository::new(self.pool.clone())
    }

    /// 影片-标签关联仓储。
    pub fn movie_tags(&self) -> MovieTagRepository {
        MovieTagRepository::new(self.pool.clone())
    }

    /// **用例**：为某条 Media 的某个时刻点生成缩略图。
    ///
    /// 三步在**同一事务**里：
    ///
    /// 1. 写 `media_thumbnail`（产物）
    /// 2. 把 `media` 标记为缩略图成功
    /// 3. 把缩略图标记为已进入（或不进入）检索索引
    ///
    /// # 为什么必须原子
    ///
    /// 如果第 2 步成功而第 1 步回滚，`media` 会永久停在 `succeeded`
    /// 而产物不存在 —— 缩略图状态机认为做完了，实际没有任何图。
    /// 反过来第 1 步成功第 2 步失败，则会留下一张无人认领的缩略图，
    /// 而 `media` 还在 `retry_wait` 里被反复重试。
    ///
    /// 两种都不是「下次重试就好」能解决的：`succeeded` 是终态，
    /// 不会被重新扫描。
    ///
    /// # `index_status` 决定终态
    ///
    /// 非 JAV 媒体的缩略图不进检索索引，调用方应传
    /// [`image_search_index_status::SKIPPED`] 而不是 `PENDING` ——
    /// 否则它会永久滞留待处理队列。
    pub async fn generate_thumbnail(
        &mut self,
        media_id: i32,
        offset_seconds: i32,
        image_id: i32,
        index_status: i32,
    ) -> Result<GeneratedThumbnail, DbError> {
        // 仓储先构造：它们只持有一个 pool 句柄，本身无状态，所以在
        // 事务被借出之前构造好，就不需要在 `ctx` 存活期间再借 `self`。
        let thumbs = self.thumbnails();
        let media_repo = self.media();

        // 先写产物：它可能因唯一约束或外键失败，失败得越早越好。
        // 两步共享同一个 Ctx，也就是同一个事务。
        let mut ctx = self.ctx();
        let thumb = thumbs
            .upsert_in(&mut ctx, media_id, offset_seconds, image_id, index_status)
            .await?;
        let media = media_repo
            .record_thumbnail_success_in(&mut ctx, media_id)
            .await?;

        Ok(GeneratedThumbnail { thumb, media })
    }
}

/// 一次缩略图生成的结果。
#[derive(Debug, Clone)]
pub struct GeneratedThumbnail {
    /// 已落库的缩略图行。
    pub thumb: crate::playback::media::MediaThumbnail,
    /// 状态机已推进到终态的 Media 行。
    pub media: crate::playback::media::Media,
}

/// 一次影片导入的结果。
#[derive(Debug, Clone)]
pub struct ImportedMovie {
    /// 已落库的影片行。
    pub movie: crate::catalog::movie::Movie,
    /// upsert 后的标签（已存在的会被复用）。
    pub tags: Vec<crate::catalog::asset::Tag>,
    /// 建立的演员关联。
    pub actor_links: usize,
}

impl UnitOfWork<'_> {
    /// **用例**：导入一部影片及其标签、演员关联。
    ///
    /// 全部在**同一事务**里：影片行、N 个标签 upsert、M 条演员关联。
    ///
    /// # 为什么必须原子
    ///
    /// 上游 `catalog_import_service.py` 的流程是「先建影片，再逐个
    /// upsert 标签，再逐条建演员关联」。拆成独立提交时，中途失败会留下：
    ///
    /// - 影片有了但演员关联没建 → 影片页显示「未知演员」，且**没有任何
    ///   机制会发现**，因为关联表是空的而非标记为不完整
    /// - 标签建了一半 → 标签筛选器里出现该影片只有部分标签，而用户
    ///   看到的是一个「正常」的影片
    ///
    /// # 标签去重由数据库做
    ///
    /// `upsert_by_name` 走 `ON CONFLICT (name) DO UPDATE ... RETURNING *`，
    /// 所以并发导入同一部影片的不同资源时不会撞唯一约束，也不会产生
    /// 重���标签。
    ///
    /// # `actor_ids` 里的重复会被 `link` 吞掉
    ///
    /// `(movie_id, actor_id)` 唯一索引 + `ON CONFLICT DO UPDATE`，
    /// 所以调用方不需要先去重。
    pub async fn import_movie(
        &mut self,
        movie: &NewMovie,
        tag_names: &[&str],
        actor_ids: &[i32],
    ) -> Result<ImportedMovie, DbError> {
        // 仓储先构造：它们只持 pool 句柄、本身无状态，所以在事务被借出
        // 之前构造好，就不必在 `ctx` 存活期间再借 `self`。
        let tag_repo = self.tags();
        let movie_repo = self.movies();
        let actor_link_repo = self.movie_actors();
        let tag_link_repo = self.movie_tags();

        // 先 upsert 标签：它们可能被多部影片共用，先建好能让关联插入
        // 不必担心外键目标不存在。标签天然重复，所以冲突概率高。
        let mut tags = Vec::with_capacity(tag_names.len());
        for name in tag_names {
            let mut ctx = self.ctx();
            tags.push(tag_repo.upsert_by_name_in(&mut ctx, name).await?);
        }

        let movie_row = {
            let mut ctx = self.ctx();
            movie_repo.insert_in(&mut ctx, movie).await?
        };

        // 影片行到位后才建关联 —— 顺序反过来会撞外键。
        let mut actor_links = 0usize;
        for actor_id in actor_ids {
            let mut ctx = self.ctx();
            actor_link_repo
                .link_in(&mut ctx, movie_row.id, *actor_id)
                .await?;
            actor_links += 1;
        }
        for tag in &tags {
            let mut ctx = self.ctx();
            tag_link_repo
                .link_in(&mut ctx, movie_row.id, tag.id)
                .await?;
        }

        Ok(ImportedMovie {
            movie: movie_row,
            tags,
            actor_links,
        })
    }
}
