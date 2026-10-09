//! 影片评论（上游 `movie_service.get_movie_reviews`，`movie_service.py:886-924`）。
//!
//! # 数据流
//!
//! ```text
//! 番号 → 本地影片行（404 movie_not_found）
//!      → 没有 javdb_id？→ 空列表（**不是 404**：本地片没绑 JavDB，评论无从谈起）
//!      → JavDB 评论接口 → 逐条映射（键名换算只有两处）
//! ```
//!
//! # 错误码与上游逐条对齐
//!
//! | 情况 | 码 |
//! |---|---|
//! | 影片不存在 | 404 `movie_not_found`（details: `movie_number`）|
//! | 本地片没绑 `javdb_id` | **200 空列表**（上游 `if not movie.javdb_id: return []`）|
//! | 评论接口 `NotFound` | 404 `movie_not_found`（details 多带 `javdb_id` —— 上游注释明写「仍统一映射为影片不存在」：本地片在而远端没了，对用户而言就是「这部片的评论看不了」）|
//! | 其它来源错误 | 502 `movie_review_fetch_failed`（details 带 `javdb_id` 与原始 `detail`，方便定位远端失败原因）|

use std::pin::Pin;

use sm_db::repo::MovieRepository;
use sm_db::Db;

use crate::catalog::javdb::{JavdbMovieReview, JavdbProvider};
use crate::error::{details_of, ServiceError};
use crate::system::status::JAVDB_HOST;

/// 评论来源。**出网**。
///
/// 与 [`JavdbProvider`] 分开的理由与 `movie_javdb_backfill::JavdbProvider`
/// 相同：服务要测「影片不存在 → 404」「没绑 id → 空列表」「来源挂了 → 502」
/// 这三条**服务层**的分支，而真 provider 只能在线上 —— 打桩点必须在这里。
///
/// `Box<dyn …>` 而不是泛型：组合根只注入一种实现，泛型会把参数传染到
/// `AppState` 的每个角落。
///
/// # 显式生命周期
///
/// `Pin<Box<dyn Future + Send + 'a>>` 里的 `'a` 是**借参数**的生命周期 ——
/// 返回的 future 持有 `&self` / `&str` 直到完成。省略写法会把它们 unelided
/// 成匿名周期而对不上（E0521 / "lifetime may not live long enough"）。
pub trait JavdbReviewSource: Send + Sync {
    /// 逐字对齐 [`JavdbProvider::movie_reviews`] 的签名。
    fn movie_reviews<'a>(
        &'a self,
        javdb_id: &'a str,
        page: i64,
        limit: i64,
        sort_by: Option<&'a str>,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Vec<JavdbMovieReview>,
                        crate::catalog::metadata_source::MetadataSourceError,
                    >,
                > + Send
                + 'a,
        >,
    >;
}

impl JavdbReviewSource for JavdbProvider {
    fn movie_reviews<'a>(
        &'a self,
        javdb_id: &'a str,
        page: i64,
        limit: i64,
        sort_by: Option<&'a str>,
    ) -> Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Vec<JavdbMovieReview>,
                        crate::catalog::metadata_source::MetadataSourceError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(JavdbProvider::movie_reviews(
            self, javdb_id, page, limit, sort_by,
        ))
    }
}

/// 影片评论服务。
pub struct MovieReviewService {
    movies: MovieRepository,
    provider: Box<dyn JavdbReviewSource>,
}

impl MovieReviewService {
    /// 构造（生产路径）：provider 照上游 `build_javdb_provider()` **每次调用
    /// 现建**（host 硬编码见 [`JAVDB_HOST`] 的拍板记录）。构造只建 HTTP 客户端，
    /// 开销与一次请求同量级 —— 不值得为它做进程级缓存。
    pub fn new(pool: &Db) -> Self {
        // host 是编译期常量且非空，`new` 在这里不可能失败（与
        // `plugin_host.rs` 的构造同一断言）。
        let provider =
            JavdbProvider::new(JAVDB_HOST).expect("JAVDB_HOST 是非空编译期常量，构造不会失败");
        Self::with_provider(pool, Box::new(provider))
    }

    /// 直接给 provider（**测试缝**）—— 与 `JavdbProvider::with_base_url`
    /// 同一存在理由：服务层的三条分支要有不依赖公网的判据。
    pub fn with_provider(pool: &Db, provider: Box<dyn JavdbReviewSource>) -> Self {
        Self {
            movies: MovieRepository::new(pool.clone()),
            provider,
        }
    }

    /// 影片评论。上游 `MovieService.get_movie_reviews`。
    ///
    /// # 参数校验在路由层
    ///
    /// `page` / `page_size` 的下限与 `sort` 的枚举校验（`recently` / `hotly`
    /// 之外的原样透传）都照上游留在 schema/route 一侧；服务只负责「影片在
    /// 不在」与「远端给不给」。
    pub async fn get_movie_reviews(
        &self,
        movie_number: &str,
        page: i64,
        page_size: i64,
        sort_by: Option<&str>,
    ) -> Result<Vec<JavdbMovieReview>, ServiceError> {
        let movie = self
            .movies
            .find_by_number(movie_number)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "movie_not_found",
                    "影片不存在",
                    details_of("movie_number", movie_number),
                )
            })?;
        let Some(javdb_id) = movie.javdb_id.as_deref().filter(|id| !id.is_empty()) else {
            // 本地片没绑 JavDB：上游明确返回**空列表**而不是 404 ——
            // 「没有评论」与「不知道有没有评论」在这里是同一回事（都没有）。
            return Ok(Vec::new());
        };

        match self
            .provider
            .movie_reviews(javdb_id, page, page_size, sort_by)
            .await
        {
            Ok(reviews) => Ok(reviews),
            // 远端 404 统一映射成本地影片 404 —— details 多带 javdb_id，
            // 排查「到底是本地缺片还是远端缺评论」时全靠它。
            Err(crate::catalog::metadata_source::MetadataSourceError::NotFound) => {
                let mut details = details_of("movie_number", movie_number);
                details.insert("javdb_id".to_owned(), serde_json::Value::from(javdb_id));
                Err(ServiceError::not_found_with(
                    "movie_not_found",
                    "影片不存在",
                    details,
                ))
            }
            Err(error) => {
                let mut details = details_of("movie_number", movie_number);
                details.insert("javdb_id".to_owned(), serde_json::Value::from(javdb_id));
                // `MetadataSourceError` 只有 `Debug` 没有 `Display`
                // （它不做错误链展开）—— detail 排查时本来就看 debug 形状。
                details.insert(
                    "detail".to_owned(),
                    serde_json::Value::from(format!("{error:?}")),
                );
                Err(ServiceError::bad_gateway(
                    "movie_review_fetch_failed",
                    "影片评论拉取失败",
                    details,
                ))
            }
        }
    }
}
