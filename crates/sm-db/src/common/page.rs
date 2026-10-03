//! 分页：`PageRequest`（已校验）与 `Page<T>`（结果）。
//!
//! # 为什么这一层在仓储而不是 service
//!
//! `total` 必须是**过滤后**的总数，而「过滤条件」就是仓储的 `WHERE` 子句。
//! 放到 service 层算 total 意味着把同一组条件写两遍（一次算总数、一次
//! 取数据），两遍一旦不同步就会返回「说好的 100 条其实只有 87 条」，
//! 而且**不会报任何错**。
//!
//! # 快照一致性：为什么在 REPEATABLE READ 里跑两条查询
//!
//! 算 total 和取 items 需要两条 SQL。默认的 READ COMMITTED 下，两条
//! 各自取一个快照，所以并发写入时可能看到不同的世界：
//!
//! ```text
//! 时刻 T1   COUNT(*)  -> 100      （此刻库里 100 行）
//! 时刻 T2   SELECT    -> 90 行     （有人插入了 10 行，offset=80 只剩 10 行可取）
//! ```
//!
//! 结果是 `total=100` 但 `items` 只有 10 条。客户端的
//! `fetch_all_pages.dart` 靠 total 决定拉几页，于是最后一页反复取到空
//! 或者漏掉数据 —— 这个 bug 极难复现和定位。
//!
//! 所以两条查询放进一个 `REPEATABLE READ` 事务：它们看到**同一个**快照，
//! `total` 与 `items` 必然自洽。
//!
//! 代价是一次 `BEGIN` + `SET TRANSACTION`（约 0.1ms）。列表页本来就要
//! 两次往返，这点开销可以忽略；而正确性没法事后补。
//!
//! # 契约来自 `sm_core::pagination`，不在这里重复实现
//!
//! 上限 100、page 从 1 开始、offset = (page-1)*page_size、错误码由端点决定
//! —— 这些都在 `sm-core` 里定好了（对应后端 `validate_page` / `paginate`）。
//! 本模块只做**调用**与**类型化**，不重新定义规则。重复定义会出现「两处
//! 上限不一致」，而其中一处没人记得去改。

use std::future::Future;
use std::pin::Pin;

use sm_core::pagination::{page_offset, validate_page, PageError};
use sqlx::{PgConnection, PgPool, Postgres, Transaction};

use crate::error::DbError;

/// 已校验的分页请求。
///
/// 构造即校验，所以持有它就意味着 `offset >= 0` 且 `1 <= limit <= 100`。
/// 仓储方法接收它而不是裸 `(page, page_size)`，让「忘记校验」不可表达。
///
/// # 响应回显**请求的**值，不是截断后的
///
/// 后端 `paginate` 的注释写明这一点。由于 [`PageRequest::new`] 只接受
/// 通过校验的参数（要么原样通过，要么被拒），实际不存在「截断」这回事 ——
/// 保持这个性质比模拟它更简单。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageRequest {
    page: i64,
    page_size: i64,
}

impl PageRequest {
    /// 校验并构造。
    ///
    /// 失败返回 [`DbError::Business`]（422）而不是 400 —— 「page_size 超了
    /// 上限」是一个可以修正的请求问题，与业务规则同源。端点若要换成别的
    /// 状态码，应当在 service 层先调 `sm_core::pagination::validate_page`
    /// 拿到 [`PageError`] 再自己映射。
    pub fn new(page: i64, page_size: i64) -> Result<Self, DbError> {
        validate_page(page, page_size).map_err(|e| DbError::business("Page", e.message()))?;
        Ok(Self { page, page_size })
    }

    /// 单页（常用于内部只需要第一页的场景，如「取最新一条」）。
    ///
    /// 仍然走校验，所以 `page_size` 超过 100 会失败而不是被静默截断。
    pub fn first_page(page_size: i64) -> Result<Self, DbError> {
        Self::new(1, page_size)
    }

    /// 页码，从 1 开始。回显给客户端用。
    pub fn page(&self) -> i64 {
        self.page
    }

    /// 每页条数。回显给客户端用。
    pub fn page_size(&self) -> i64 {
        self.page_size
    }

    /// SQL OFFSET。
    pub fn offset(&self) -> i64 {
        page_offset(self.page, self.page_size)
    }

    /// SQL LIMIT。与 `page_size` 同值，单列出来是为了让 SQL 里
    /// `$1 = LIMIT`、`$2 = OFFSET` 的意图自明。
    pub fn limit(&self) -> i64 {
        self.page_size
    }
}

/// 一页结果。
///
/// 与 `sm_core::pagination::Paginated` 的差别：这里只有仓储能算出的
/// 部分（`items` 与 `total`）。`page` / `page_size` / `synced_at` 属于
/// HTTP 表达层，由 service 层用 [`PageRequest`] 补齐 ——
/// 见 [`Page::into_paginated`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// 过滤后的总条数。**不是**本页条数。
    pub total: i64,
}

impl<T> Page<T> {
    /// 构造一页。`total` 必须是过滤后的总数。
    pub fn new(items: Vec<T>, total: i64) -> Self {
        Self { items, total }
    }

    /// 空页，但带着正确的 `total`。
    ///
    /// 这不是「没有数据」而是「请求的页超出了范围」——客户端需要 `total`
    /// 才能判断该不该继续翻页，所以不能返回 `total=0`。
    pub fn empty(total: i64) -> Self {
        Self {
            items: Vec::new(),
            total,
        }
    }

    /// 本页条数。
    pub fn len(&self) -> i64 {
        self.items.len() as i64
    }

    /// 本页是否为空。
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 补齐 HTTP 表达层的字段，得到可直接序列化的分页响应。
    ///
    /// `synced_at` 留空由调用方按需 [`sm_core::pagination::Paginated::with_synced_at`] 附加 ——
    /// 它的语义是「这批数据的抓取时间」，仓储不知道。
    pub fn into_paginated(self, request: &PageRequest) -> sm_core::pagination::Paginated<T> {
        sm_core::pagination::Paginated::new(self.items, request.page, request.page_size, self.total)
    }
}

/// 把仓储 list 方法的参数转成 owned 值。
///
/// [`crate::paged_list`] 生成的闭包是 `BoxFuture<'c>`，其中 `'c` 是**连接**的借用。
/// 被 `async` 块捕获的任何引用都必须活过 `'c`，而方法参数的生命周期比整个
/// 调用短——所以参数必须在进入闭包**之前**变成 owned。
///
/// blanket impl 覆盖 `T: Clone`（`i32`、`bool`、`String`），而 `&str` 走
/// 下面那个专门 impl 变成 `String`。两者都得到「与输入语义相同、但不借用
/// 调用方作用域」的值。
/// 把仓储 list 方法的参数转成 owned 值。
///
/// [`crate::paged_list`] 生成的闭包是 `BoxFuture<'c>`，其中 `'c` 是**连接**的借用。
/// 被 `async` 块捕获的任何引用都必须活过 `'c`，而方法参数的生命周期比整个
/// 调用短——所以参数必须在进入闭包**之前**变成 owned。
///
/// # 为什么不能只写 `impl<T: Clone>`
///
/// 那样会与 `&str` 的专门实现冲突（`&str` 也是 `Clone`），而 `&str` 必须
/// 走专门实现才能得到 `String`——blanket impl 给的会是 `&str`，也就是
/// **原来的借用**，问题没解决。
pub trait PageArg {
    /// owned 后的类型。
    type Owned;
    /// 执行转换。
    fn into_page_arg(self) -> Self::Owned;
}

/// 标记「可以按值复制、不需要特殊处理」的类型。
///
/// 只有它 blanket impl `PageArg`，从而让 `&str` 落到自己的实现上。
pub trait PageArgCopy: Clone {}

impl PageArgCopy for i32 {}
impl PageArgCopy for i64 {}
impl PageArgCopy for bool {}
impl PageArgCopy for String {}
impl PageArgCopy for u32 {}
impl PageArgCopy for f64 {}
impl PageArgCopy for chrono::NaiveDateTime {}

impl<T: PageArgCopy> PageArg for T {
    type Owned = T;
    fn into_page_arg(self) -> T {
        self
    }
}

impl PageArg for &str {
    type Owned = String;
    fn into_page_arg(self) -> String {
        self.to_owned()
    }
}

/// 校验一页结果的形状。
///
/// 独立于取页流程，因为它校验的是**调用方已经取回的行**，
/// 而不是一个可以顺手检查的中间值。
pub fn verify_page_shape<T>(
    page: &Page<T>,
    request: &PageRequest,
    entity: &'static str,
) -> Result<(), DbError> {
    if page.len() > request.limit() {
        return Err(DbError::business(
            entity,
            format!(
                "本页返回 {} 条，超过 LIMIT {} —— 查询漏了 LIMIT，total 与 items 会不一致",
                page.len(),
                request.limit()
            ),
        ));
    }
    // items 不可能超过 total：total 是过滤后的全量。
    if page.total < page.len() {
        return Err(DbError::business(
            entity,
            format!(
                "total({}) 小于本页条数({})，COUNT 与 SELECT 的条件不一致",
                page.total,
                page.len()
            ),
        ));
    }
    Ok(())
}

/// 在一个 `REPEATABLE READ` 快照里执行闭包。
///
/// 解决 [`PageRequest`] 模块文档里描述的 total/items 不一致问题。
/// 闭包返回 `Result`，错误会随事务一起回滚。
///
/// # 为什么不用 `pool.begin()` 就够了
///
/// sqlx 的 `begin()` 发的是 `BEGIN`，即 READ COMMITTED。必须紧跟一条
/// `SET TRANSACTION ISOLATION LEVEL REPEATABLE READ` —— 而它**必须是
/// 事务里的第一条语句**，所以不能靠连接池的 `after_connect` 钩子（那会在
/// 任何事务之前跑，无效）。
pub async fn in_snapshot_tx<T, F>(pool: &PgPool, f: F) -> Result<T, DbError>
where
    F: for<'c> FnOnce(
        &'c mut PgConnection,
    ) -> Pin<Box<dyn Future<Output = Result<T, DbError>> + Send + 'c>>,
{
    let mut tx: Transaction<'_, Postgres> = pool.begin().await.map_err(DbError::from)?;

    // 必须在任何查询之前设置，否则 PostgreSQL 报
    // "SET TRANSACTION ISOLATION LEVEL must be called before any query"。
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .map_err(|e| DbError::from(e).with_entity("Page"))?;

    let out = f(&mut tx).await;

    match out {
        Ok(value) => {
            tx.commit().await.map_err(DbError::from)?;
            Ok(value)
        }
        Err(err) => {
            // 回滚失败时保留原始错误 —— 它才是调用方要处理的。
            let _ = tx.rollback().await;
            Err(err)
        }
    }
}

/// 把 [`PageError`] 转成 [`DbError`]。
///
/// 供那些想自己映射状态码的端点使用：先拿到 `PageError`，按端点约定
/// 决定 400 还是 422，再决定要不要继续构造仓储查询。
pub fn page_error_to_db(error: PageError) -> DbError {
    DbError::business("Page", error.message())
}

/// 声明一个「COUNT 与 SELECT 共用同一组 bind」的取页方法。
///
/// # 为什么用宏而不是让每个仓储手写
///
/// 取页的骨架完全固定：
///
/// 1. 在 `REPEATABLE READ` 快照里开事务
/// 2. 先 `COUNT(*)`，绑定与 items 相同的过滤参数
/// 3. 再取 items，额外 bind `LIMIT` / `OFFSET`
/// 4. 校验返回形状
///
/// 手写 17 遍意味着 17 个地方可能漏掉第 4 步，或者只给 15 个方法加上
/// `LIMIT`。而这两种遗漏**都不会让编译失败** —— 只在数据量超过一页时
/// 才显形，测试用小数据集根本碰不到。
///
/// # SQL 是字面量，不是 `format!`
///
/// 过滤条件全部走 bind，所以这两段 SQL 没有理由是动态的。写成字面量
/// 让 sqlx 0.9 的 `SqlSafeStr` 检查通过，同时**从类型上排除**了拼接
/// 用户输入的可能。占位符约定：items 查询里 `$1..$n` 是过滤参数，
/// `$n+1` 是 LIMIT，`$n+2` 是 OFFSET。
/// 展开成「COUNT 与 SELECT 共用同一组 bind」的取页方法。
///
/// # 为什么用宏而不是让每个仓储手写
#[macro_export]
macro_rules! paged_list {
    (
        $(#[$meta:meta])*
        $vis:vis async fn $name:ident(
            &self
            $(, $arg:ident : $ty:ty)* $(,)?
        ) -> Result<Page<$item:ty>, DbError> {
            count = $count_sql:literal,
            items = $items_sql:literal,
        }
    ) => {
        $(#[$meta])*
        $vis async fn $name(
            &self
            $(, $arg: $ty)*
            , page: PageRequest
        ) -> Result<Page<$item>, DbError> {
            // 参数必须活到事务结束，但 `async move` 会把 async 块**外部**
            // 的借用一起捕获，而那些借用的生命周期比事务短。
            //
            // `ToOwned` 正好解决这个：std 对 `T: Clone` 有 blanket impl
            // （`i32 -> i32`、`bool -> bool`），而 `&str -> String`。
            // 于是所有参数都变成 owned，async 块不再借用外层作用域。
            //
            // 额外拷贝一次 String 对列表查询可以接受 —— 一次 memcpy，
            // 换掉的是一整类生命周期错误。

            $(let $arg = $crate::common::page::PageArg::into_page_arg($arg);)*

            $crate::common::page::in_snapshot_tx(&self.pool, |conn| {
                Box::pin(async move {
                    // 1. 总数。过滤条件与 items 查询完全一致 —— 这是
                    //    total 可信的前提，靠宏保证两段 SQL 写在同一次
                    //    编辑里。
                    let count_query = sqlx::query_scalar::<_, i64>($count_sql);
                    $(let count_query = count_query.bind($arg.clone());)*
                    let total = count_query
                        .fetch_one(&mut *conn)
                        .await
                        .map_err(|e| DbError::from(e).with_entity("Page"))?;

                    // 2. 本页。bind 顺序：过滤参数 -> LIMIT -> OFFSET。
                    let query = sqlx::query_as::<_, $item>($items_sql);
                    $(let query = query.bind($arg.clone());)*
                    let rows = query
                        .bind(page.limit())
                        .bind(page.offset())
                        .fetch_all(&mut *conn)
                        .await
                        .map_err(|e| DbError::from(e).with_entity("Page"))?;

                    Ok(Page::new(rows, total))
                })
            })
            .await
            .and_then(|result| {
                $crate::common::page::verify_page_shape(&result, &page, "Page")?;
                Ok(result)
            })
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_contracts_valid_range() {
        let req = PageRequest::new(1, 20).unwrap();
        assert_eq!(req.page(), 1);
        assert_eq!(req.page_size(), 20);
        assert_eq!(req.offset(), 0);
        assert_eq!(req.limit(), 20);

        // 上限本身是合法的
        assert!(PageRequest::new(1, sm_core::pagination::MAX_PAGE_SIZE).is_ok());
        // 很大的页码也合法 —— 越界由空页表达，不在这里拒绝
        assert!(PageRequest::new(9999, 20).is_ok());
    }

    #[test]
    fn rejects_what_the_contract_rejects() {
        // 上限 100 来自 sm-core，本模块不重新定义
        for (page, size) in [(0, 20), (-1, 20), (1, 0), (1, 101), (1, 1000)] {
            let err = PageRequest::new(page, size).unwrap_err();
            assert!(
                matches!(err, DbError::Business { .. }),
                "page={page} size={size} 应是业务错误(422)，实际 {err:?}"
            );
        }
    }

    #[test]
    fn page_is_checked_before_page_size() {
        // 与后端顺序一致：两个都非法时只报 page。
        let err = PageRequest::new(0, 0).unwrap_err();
        assert!(
            err.to_string().contains("page must be greater than 0"),
            "{err}"
        );
    }

    #[test]
    fn offset_is_one_based_and_matches_the_contract() {
        assert_eq!(PageRequest::new(1, 20).unwrap().offset(), 0);
        assert_eq!(PageRequest::new(2, 20).unwrap().offset(), 20);
        assert_eq!(PageRequest::new(4, 25).unwrap().offset(), 75);
        // 校验保证 offset 永远非负 —— 这正是「构造即校验」的价值
        assert!(PageRequest::new(1, 20).unwrap().offset() >= 0);
    }

    #[test]
    fn empty_page_keeps_the_real_total() {
        // 越界页必须带回真实 total，否则客户端不知道该不该继续翻页。
        let page: Page<i32> = Page::empty(137);
        assert!(page.is_empty());
        assert_eq!(page.len(), 0);
        assert_eq!(page.total, 137, "空页不等于没有数据");
    }

    #[test]
    fn shape_verification_catches_a_missing_limit() {
        let req = PageRequest::new(1, 20).unwrap();
        // 返回了 25 条但 LIMIT 是 20 —— 查询漏了 LIMIT
        let over = Page::new(vec![1i32; 25], 100);
        let err = verify_page_shape(&over, &req, "Movie").unwrap_err();
        assert!(err.to_string().contains("漏了 LIMIT"), "{err}");

        // total 小于 items —— COUNT 与 SELECT 条件不一致
        let inconsistent = Page::new(vec![1i32; 5], 3);
        let err = verify_page_shape(&inconsistent, &req, "Movie").unwrap_err();
        assert!(err.to_string().contains("COUNT 与 SELECT"), "{err}");

        // 正常页通过
        assert!(verify_page_shape(&Page::new(vec![1i32; 20], 100), &req, "Movie").is_ok());
        // 空页但 total 正确也通过
        assert!(verify_page_shape(&Page::<i32>::empty(100), &req, "Movie").is_ok());
    }

    #[test]
    fn into_paginated_echoes_the_request_not_a_truncation() {
        let req = PageRequest::new(3, 25).unwrap();
        let page = Page::new(vec!["a".to_owned(), "b".to_owned()], 137);
        let paginated = page.into_paginated(&req);

        assert_eq!(paginated.items.len(), 2);
        assert_eq!(paginated.total, 137);
        // 回显请求值
        assert_eq!(paginated.page, 3);
        assert_eq!(paginated.page_size, 25);
        assert!(paginated.synced_at.is_none());
        // 与客户端 fetch_all_pages 的算法一致
        assert_eq!(sm_core::pagination::last_page(137, 25), 6);
    }
}
