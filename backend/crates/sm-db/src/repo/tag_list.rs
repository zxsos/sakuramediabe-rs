//! 标签列表的**计数查询**（上游 `TagService._tag_count_query`）。
//!
//! 单独立一个文件而不是塞进 `asset.rs`：那边是「插入 / 按名查找」这类点操作，
//! 这里是带 `GROUP BY` 的聚合投影，混在一个 impl 块里会让标签仓储看起来有
//! 两块职责不同的区域。

use super::asset::TagRepository;
use super::movie::safe_sql;
use crate::error::DbError;

/// 标签列表的排序键。**闭集** —— `ORDER BY` 片段由本模块给出，调用方递不进
/// 裸 SQL。
///
/// 上游 `_build_tag_sort` 接受 `field:direction`，字段是
/// `movie_count` / `name`；缺省（含空白串）归一为 `movie_count:desc`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagListSort {
    /// 上游缺省。
    MovieCountDesc,
    MovieCountAsc,
    NameAsc,
    NameDesc,
}

/// 一行 `(tag_id, name, movie_count)`。
///
/// 元组而不是结构体：投影行没有上游 Peewee 模型，在 `sm-db` 里声明成结构体会
/// 让对拍门禁报 `UNCHECKED_STRUCT`（同 `MovieResolutionLevelRow` 的理由）。
pub type TagCountRow = (i32, String, i64);

impl TagRepository {
    /// 标签列表（带影片数）。对应上游 `list_tags`。
    ///
    /// **不分页** —— 标签筛选器一次要全部（上游同样没有分页参数）。
    ///
    /// # `LEFT JOIN` 与 `COUNT(mt.movie_id)`
    ///
    /// 左连接让「一个影片都没挂」的标签也出现；计数列用 `mt.movie_id` 而不是
    /// `COUNT(*)` —— 后者会把左连接补出的那一行 NULL 也算成 1。
    ///
    /// # `query` 是名字的 `LIKE %q%`，**不转义** `%`
    ///
    /// 与影片检索同一口径：上游 `.contains()` 直接拼通配符，用户输入的 `%`
    /// 在那里也是通配符。转义会改变既有筛选行为。
    pub async fn list_with_counts(
        &self,
        query: Option<&str>,
        sort: TagListSort,
    ) -> Result<Vec<TagCountRow>, DbError> {
        let mut sql = String::from(
            "SELECT t.id, t.name, COUNT(mt.movie_id) AS movie_count \
             FROM tag t LEFT JOIN movie_tag mt ON mt.tag_id = t.id",
        );
        if query.is_some() {
            sql.push_str(" WHERE t.name LIKE $1");
        }
        sql.push_str(" GROUP BY t.id ");
        sql.push_str(order_by(sort));

        let mut stmt = sqlx::query_as::<_, TagCountRow>(safe_sql(sql));
        if let Some(query) = query {
            stmt = stmt.bind(format!("%{query}%"));
        }
        Ok(stmt.fetch_all(self.pool()).await?)
    }

    /// 单个标签（带影片数）。对应上游 `get_tag` 的查询部分 ——
    /// **未命中返回 `None`**，404 由 service 层给（它才知道错误码）。
    pub async fn find_with_count(&self, tag_id: i32) -> Result<Option<TagCountRow>, DbError> {
        Ok(sqlx::query_as::<_, TagCountRow>(
            "SELECT t.id, t.name, COUNT(mt.movie_id) AS movie_count \
             FROM tag t LEFT JOIN movie_tag mt ON mt.tag_id = t.id \
             WHERE t.id = $1 GROUP BY t.id",
        )
        .bind(tag_id)
        .fetch_optional(self.pool())
        .await?)
    }
}

/// `ORDER BY` 片段。全部是常量。
///
/// `movie_count` 的两个方向都补 `name ASC`：上游 `_movie_count_order` 的
/// `extra=(Tag.name.asc(),)` —— 影片数并列时按名字定序，否则标签筛选器的展示
/// 顺序会在两次刷新之间抖动（而用户会把它当成「标签乱了」）。
fn order_by(sort: TagListSort) -> &'static str {
    match sort {
        TagListSort::MovieCountDesc => "ORDER BY COUNT(mt.movie_id) DESC, t.name ASC, t.id DESC",
        TagListSort::MovieCountAsc => "ORDER BY COUNT(mt.movie_id) ASC, t.name ASC, t.id ASC",
        TagListSort::NameAsc => "ORDER BY t.name ASC, t.id ASC",
        TagListSort::NameDesc => "ORDER BY t.name DESC, t.id DESC",
    }
}
