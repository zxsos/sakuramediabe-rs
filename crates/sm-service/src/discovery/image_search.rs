//! 图搜会话与检索（上游 `image_search_service.py`，12.3KB / 301 行）。
//!
//! 依赖：推理客户端 [`super::embedding`] + 稠密向量库 [`super::qdrant::dense`]
//! + 会话仓储 `ImageSearchSessionRepository`。**三者均已就位。**
//!
//! # 会话存的是**查询向量**
//!
//! `image_search_session.query_vector` 存着建会话时算出的那条向量，翻页时直接
//! 拿它去检索 —— **不重新推理**。
//!
//! 两个后果：
//!
//! 1. 换 embedding 模型后老会话的向量无法与新索引比较（**维度可能相同但语义
//!    不同**）。防线是 `image_search_space` 的状态机 +
//!    `ImageSearchSessionRepository::delete_all` —— 重建时**物理删掉全部会话**。
//! 2. 会话表会长到 TTL 上限。`_purge_expired_sessions` 在**每次建会话与每次取
//!    会话时**都跑（`:88`、`:131`、`:93`）—— **读路径上带一次写**。
//!
//! # 游标是不透明串，但**不是加密的**
//!
//! `{"v":1,"offset":N}` → 紧凑 JSON → base64url 去填充。客户端能解开看，
//! 所以**改游标就能翻到任意页**。这不是安全问题（检索结果本身不敏感），但
//! **别把它当防篡改用**。
//!
//! # ⚠️ 一处我之前说错的：`_normalize_ids([])` 返回 `None`
//!
//! 我在路由层与 `plot_image_search` 的注释里写过「`ids=[]` 与不传 ids 语义不同，
//! 空列表是「显式排除全部」」。**那是错的。**
//!
//! 上游 `:49-52` 是 `if not ids: return None` —— Python 里 `[]` 是 falsy，
//! 所以**空列表会变成 `None`（不过滤）**。路由层把 CSV 空串解析成
//! `Some(vec![])` 也没用，服务层会归一成 `None`。**两层语义统一：空 = 不过滤。**

use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use sm_db::common::now_utc;
use sm_db::repo::discovery::{ImageSearchSessionRepository, NewImageSearchSession};

use super::embedding::EmbeddingClient;
use super::image_search_space::ImageSearchIndexSpaceService;
use super::qdrant::dense::DenseStore;
// 读 `ScoredPoint.payload` 的整数键：那个 map 的值是 qdrant 的 `Value`
// （struct + `kind` oneof），**不是** `serde_json::Value`，所以不能
// `and_then(serde_json::Value::as_i64)`。
use super::qdrant::plot_image::payload_i64;
use crate::error::ServiceError;

/// 游标版本。改动游标结构时**必须** bump —— 老的游标解出来是错的 offset。
pub const CURSOR_VERSION: i64 = 1;

/// 会话状态：可用。
pub const SESSION_STATUS_READY: &str = "ready";

/// 归一化 id 列表：**空 → `None`（不过滤）**，并去重保序。
///
/// # 上游用 `dict.fromkeys` 去重（`:52`）—— 保序去重
///
/// 顺序对分页本身没有意义，但**稳定**的顺序让同一组输入产生同一份过滤条件，
/// 便于缓存与对比。
///
/// # ⚠️ 这里纠正了本模块早先的注释
///
/// 早先写「`Some(&[])` 是『显式排除全部』，与 `None` 语义不同」—— **错的**。
/// 上游 `if not ids: return None` 把空列表也归一成 `None`。**空 = 不过滤。**
pub fn normalize_ids(ids: Option<&[i64]>) -> Option<Vec<i64>> {
    let ids = ids?;
    if ids.is_empty() {
        return None;
    }
    let mut seen = HashSet::with_capacity(ids.len());
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if seen.insert(*id) {
            out.push(*id);
        }
    }
    Some(out)
}

/// [`crate::discovery::image_search::ImageSearchService::validate_create_params`]
/// 归一化后的三元组：`(page_size, movie_ids, exclude_movie_ids)`。
pub type SessionParams = (i64, Option<Vec<i64>>, Option<Vec<i64>>);

/// 归一化页大小。
///
/// # 三档，缺省走配置
///
/// | 输入 | 结果 |
/// |---|---|
/// | `None` | `default_page_size`（配置项）|
/// | `<= 0` | **报错**，不夹到 1 |
/// | `> max_page_size` | **报错**，不夹到上界 |
///
/// **夹到边界是错的**：客户端会拿到一个它不知道自己没拿全的结果。
pub fn normalize_page_size(
    page_size: Option<i64>,
    default_page_size: i64,
    max_page_size: i64,
) -> Result<i64, ServiceError> {
    match page_size {
        None => Ok(default_page_size),
        Some(size) if size <= 0 => Err(ServiceError::validation(
            "invalid_image_search_page_size",
            "page_size 必须为正数",
        )),
        Some(size) if size > max_page_size => Err(ServiceError::validation(
            "invalid_image_search_page_size",
            format!("page_size 不能超过 {max_page_size}"),
        )),
        Some(size) => Ok(size),
    }
}

/// 校验 `score_threshold` 在 `[0, 1]`。
///
/// **闭区间** —— `0.0` 与 `1.0` 都合法（分别要「不要更差的」与「只要满分的」）。
pub fn validate_score_threshold(score_threshold: Option<f64>) -> Result<(), ServiceError> {
    match score_threshold {
        None => Ok(()),
        Some(value) if (0.0..=1.0).contains(&value) => Ok(()),
        Some(value) => Err(ServiceError::validation(
            "invalid_image_search_score_threshold",
            format!("score_threshold 必须在 0 到 1 之间，收到 {value}"),
        )),
    }
}
/// 编码游标：`{"v":1,"offset":N}` → 紧凑 JSON → base64url **去填充**。
///
/// # 去掉 `=` 填充是为了 URL 安全
///
/// 游标出现在 query string 里，`=` 在某些客户端 / 代理链路上会被转义成
/// `%3D`。解码时补回去（[`decode_cursor`]）—— **编码侧去掉、解码侧接受，
/// 两侧必须成对**。
///
/// # 手写 base64 而不引 crate
///
/// `sm-service` 刻意不引入 base64 依赖 —— 这 20 行换一个 crate 不划算。
/// **若将来引了 base64 crate，把这两个函数换掉即可**（格式完全兼容）。
pub fn encode_cursor(offset: i64) -> Result<String, ServiceError> {
    if offset < 0 {
        return Err(ServiceError::validation(
            "invalid_image_search_cursor",
            "offset 不能为负",
        ));
    }
    let raw = format!("{{\"v\":{CURSOR_VERSION},\"offset\":{offset}}}");
    Ok(base64url_encode(raw.as_bytes()))
}

/// 解码游标。任何不合法都返回 400（上游 `ValueError`）。
///
/// # 校验三件事，缺一不可
///
/// 1. 能解出 JSON 对象
/// 2. `v` 恰好等于 [`CURSOR_VERSION`]
/// 3. `offset` 是 `>= 0` 的整数
///
/// 第 2 条是**版本闸**：游标结构改了以后老游标必须被拒，而不是解出一个
/// 语义已变的 offset（那样会静默翻到错误的页）。
///
/// # 上游用 `int(payload.get("v", -1))`（`:70`）—— 接受字符串 `"1"`
///
/// 照抄那个宽松度会引入「`{"v":"1"}` 也算合法」这种歧义。**这里只收整数**
/// —— 游标是本仓库自己生成的，不存在需要兼容的外部格式。
pub fn decode_cursor(cursor: &str) -> Result<i64, ServiceError> {
    let invalid = || ServiceError::validation("invalid_image_search_cursor", "cursor 无效");
    if cursor.is_empty() {
        return Err(invalid());
    }
    let raw = base64url_decode(cursor).ok_or_else(invalid)?;
    let text = String::from_utf8(raw).map_err(|_| invalid())?;
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    if !value.is_object() {
        return Err(invalid());
    }
    if value.get("v").and_then(serde_json::Value::as_i64) != Some(CURSOR_VERSION) {
        return Err(invalid());
    }
    let offset = value
        .get("offset")
        .and_then(serde_json::Value::as_i64)
        .ok_or_else(invalid)?;
    if offset < 0 {
        return Err(invalid());
    }
    Ok(offset)
}

/// base64url 编码（**无填充**）。
const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn base64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        // 3 字节 -> 4 字符；末组不足时先占位再统一去掉 '='。
        for i in 0..4 {
            let index = ((triple >> (18 - i * 6)) & 0x3f) as usize;
            out.push(if i <= chunk.len() {
                B64URL[index] as char
            } else {
                '='
            });
        }
    }
    out.trim_end_matches('=').to_owned()
}

/// base64url 解码（**接受无填充输入**）。
fn base64url_decode(text: &str) -> Option<Vec<u8>> {
    fn value_of(byte: u8) -> Option<u32> {
        Some(match byte {
            b'A'..=b'Z' => (byte - b'A') as u32,
            b'a'..=b'z' => (byte - b'a') as u32 + 26,
            b'0'..=b'9' => (byte - b'0') as u32 + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        })
    }
    let mut out = Vec::with_capacity(text.len() / 4 * 3);
    let mut buffer = 0u32;
    let mut bits = 0u32;
    for byte in text.bytes() {
        if byte == b'=' {
            break;
        }
        // 用 `checked_shl` 防溢出：连续喂 6 位时 buffer 会一直涨。
        buffer = buffer.checked_shl(6)? | value_of(byte)?;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
        }
    }
    Some(out)
}
/// 检索结果条目。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSearchItem {
    pub thumbnail_id: i64,
    pub media_id: i64,
    pub movie_id: Option<i64>,
    /// 相似度，已夹到 [0, 1]。
    pub score: f32,
}

/// 检索结果分页。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSearchPage {
    pub items: Vec<ImageSearchItem>,
    /// `None` = 没有下一页。
    pub next_cursor: Option<String>,
}

/// 会话 + 第一页。建会话端点的响应体。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageSearchSessionPage {
    pub session_id: String,
    #[serde(flatten)]
    pub page: ImageSearchPage,
}

/// 图搜服务配置（从配置快照读，构造时注入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageSearchLimits {
    pub default_page_size: i64,
    pub max_page_size: i64,
    /// 会话 TTL（秒）。
    pub session_ttl_seconds: i64,
}

/// 图搜服务。
pub struct ImageSearchService {
    store: std::sync::Arc<DenseStore>,
    embedding: std::sync::Arc<EmbeddingClient>,
    sessions: ImageSearchSessionRepository,
    space: ImageSearchIndexSpaceService,
    limits: ImageSearchLimits,
}

impl ImageSearchService {
    /// 构造。
    pub fn new(
        store: std::sync::Arc<DenseStore>,
        embedding: std::sync::Arc<EmbeddingClient>,
        sessions: ImageSearchSessionRepository,
        space: ImageSearchIndexSpaceService,
        limits: ImageSearchLimits,
    ) -> Self {
        Self {
            store,
            embedding,
            sessions,
            space,
            limits,
        }
    }

    /// 建会话前的索引就绪闸门。
    ///
    /// 上游 `_ensure_searchable_index`（`:99-112`）：
    ///
    /// 1. `describe()` 失败 → 把推理服务的错误码**原样透传**（503 / 502）
    /// 2. `ensure_search_ready(space_id)` 抛重建错误 → **409** + `details`
    ///
    /// **这个顺序不能换**：先查索引再建会话，否则会在索引不可用时留下一堆
    /// 永远查不出结果的会话。
    pub async fn ensure_searchable_index(
        &self,
    ) -> Result<super::embedding::EmbeddingSpace, ServiceError> {
        let space = self.embedding.describe().await?;
        // 重建错误由 `image_search_space` 转成 409（带 `reason` 等三个 details 键）。
        self.space.ensure_search_ready(&space.space_id).await?;
        Ok(space)
    }

    /// 清理过期会话。
    ///
    /// **读路径上带一次写**（上游每次取会话都跑）。不清理的话会话表无界增长。
    pub async fn purge_expired_sessions(&self) -> Result<u64, ServiceError> {
        let now = now_utc();
        Ok(self.sessions.delete_expired(now).await?)
    }

    /// 校验并归一化建会话的参数。**顺序照上游**（`:122-128`）。
    ///
    /// 返回 [`SessionParams`] 而不是裸三元组 —— `clippy::type_complexity` 之外，
    /// 调用点也不必再靠位置记住哪个是 `movie_ids`、哪个是 `exclude_movie_ids`。
    pub fn validate_create_params(
        &self,
        image_bytes: &[u8],
        page_size: Option<i64>,
        movie_ids: Option<&[i64]>,
        exclude_movie_ids: Option<&[i64]>,
        score_threshold: Option<f64>,
    ) -> Result<SessionParams, ServiceError> {
        if image_bytes.is_empty() {
            return Err(ServiceError::validation(
                "image_search_empty_image",
                "image file is empty",
            ));
        }
        // 上游顺序：空图 -> page_size -> ids -> threshold。**不重排**。
        let page_size = normalize_page_size(
            page_size,
            self.limits.default_page_size,
            self.limits.max_page_size,
        )?;
        let movie_ids = normalize_ids(movie_ids);
        let exclude_movie_ids = normalize_ids(exclude_movie_ids);
        validate_score_threshold(score_threshold)?;
        Ok((page_size, movie_ids, exclude_movie_ids))
    }
}
impl ImageSearchService {
    /// 以图为 query 建会话并返回第一页。
    ///
    /// # 顺序**不能换**（上游 `:122-151`）
    ///
    /// 1. 校验参数（空图 / page_size / ids / threshold）
    /// 2. **`ensure_searchable_index`** —— 索引不可用就**在建会话之前**拒
    /// 3. 清理过期会话
    /// 4. **推理取向量** —— 失败即中止
    /// 5. 建会话（存向量）
    /// 6. 检索第一页
    ///
    /// 第 2 步在第 5 步之前是关键：索引不可用时若先建会话，会留下一堆**永远
    /// 查不出结果**的会话，而用户看到的是 200。
    ///
    /// 第 4 步同理 —— 上游注释（`:135`）：「远端推理不可用时直接中止建会话，
    /// 避免写入无法使用的查询会话」。
    pub async fn create_session_and_first_page(
        &self,
        image_bytes: &[u8],
        page_size: Option<i64>,
        movie_ids: Option<&[i64]>,
        exclude_movie_ids: Option<&[i64]>,
        score_threshold: Option<f64>,
    ) -> Result<ImageSearchSessionPage, ServiceError> {
        let (page_size, movie_ids, exclude_movie_ids) = self.validate_create_params(
            image_bytes,
            page_size,
            movie_ids,
            exclude_movie_ids,
            score_threshold,
        )?;
        let space = self.ensure_searchable_index().await?;
        self.purge_expired_sessions().await?;

        // 一次只送一张 —— 建会话只需要「这一张图的向量」。
        let mut vectors = self.embedding.embed_images(&[image_bytes.to_vec()]).await?;
        if vectors.is_empty() {
            return Err(ServiceError::validation(
                "image_search_embedding_empty",
                "推理服务没有返回向量",
            ));
        }
        let vector = vectors.remove(0);
        // 维度必须与 describe 声明的一致 —— 推理换了模型而 describe 还报旧
        // 空间时，这里是唯一能发现的地方。
        debug_assert_eq!(
            vector.len(),
            space.dimension,
            "推理返回的向量维度与 describe 声明的空间维度不一致"
        );

        let session_id = new_session_id();
        let now = now_utc();
        let session = self
            .sessions
            .create(&NewImageSearchSession {
                session_id: session_id.clone(),
                page_size: page_size as i32,
                query_vector: Some(vector_json(&vector)),
                score_threshold,
                expires_at: now + chrono::Duration::seconds(self.limits.session_ttl_seconds),
            })
            .await?;
        // ⚠️ `NewImageSearchSession` **没有** `movie_ids` / `exclude_movie_ids`
        // 字段，要靠 `set_exclusions` 单独补 —— 上游是一个 `create` 写全。
        // 分两次写有窗口期：会话已可见而过滤条件还没生效，那一页会**不过滤**。
        // 所以**建完立刻补**，不要留在中间。
        let movie_ids_i32 = to_i32(movie_ids.as_deref());
        let exclude_movie_ids_i32 = to_i32(exclude_movie_ids.as_deref());
        self.sessions
            .set_filters(
                &session.session_id,
                movie_ids_i32.as_deref(),
                exclude_movie_ids_i32.as_deref(),
            )
            .await?;

        let page = self
            .search_page(&session, vector, 0, page_size as usize)
            .await?;
        Ok(ImageSearchSessionPage { session_id, page })
    }
}
impl ImageSearchService {
    /// 以文本为 query 建会话并返回第一页。
    ///
    /// **与图版唯一的差别**是取向量那一步用 `embed_texts`。其余（校验顺序、
    /// 就绪闸门、清过期、建会话、补排除条件、检索第一页）**完全相同** ——
    /// 刻意不抽成泛型，两条路径的差别只有一行。
    pub async fn create_text_session_and_first_page(
        &self,
        text: &str,
        page_size: Option<i64>,
        movie_ids: Option<&[i64]>,
        exclude_movie_ids: Option<&[i64]>,
        score_threshold: Option<f64>,
    ) -> Result<ImageSearchSessionPage, ServiceError> {
        // 文本版的「空」判据是**去空白后为空**（`min_length=1` 只挡空串）。
        if text.trim().is_empty() {
            return Err(ServiceError::validation(
                "image_search_empty_text",
                "text is empty",
            ));
        }
        let page_size = normalize_page_size(
            page_size,
            self.limits.default_page_size,
            self.limits.max_page_size,
        )?;
        let movie_ids = normalize_ids(movie_ids);
        let exclude_movie_ids = normalize_ids(exclude_movie_ids);
        validate_score_threshold(score_threshold)?;

        let space = self.ensure_searchable_index().await?;
        self.purge_expired_sessions().await?;
        let mut vectors = self.embedding.embed_texts(&[text.to_owned()]).await?;
        if vectors.is_empty() {
            return Err(ServiceError::validation(
                "image_search_embedding_empty",
                "推理服务没有返回向量",
            ));
        }
        let vector = vectors.remove(0);
        debug_assert_eq!(vector.len(), space.dimension, "文本向量维度与空间不一致");

        let session_id = new_session_id();
        let now = now_utc();
        let session = self
            .sessions
            .create(&NewImageSearchSession {
                session_id: session_id.clone(),
                page_size: page_size as i32,
                query_vector: Some(vector_json(&vector)),
                score_threshold,
                expires_at: now + chrono::Duration::seconds(self.limits.session_ttl_seconds),
            })
            .await?;
        let movie_ids_i32 = to_i32(movie_ids.as_deref());
        let exclude_movie_ids_i32 = to_i32(exclude_movie_ids.as_deref());
        self.sessions
            .set_filters(
                &session.session_id,
                movie_ids_i32.as_deref(),
                exclude_movie_ids_i32.as_deref(),
            )
            .await?;
        let page = self
            .search_page(&session, vector, 0, page_size as usize)
            .await?;
        Ok(ImageSearchSessionPage { session_id, page })
    }

    /// 翻页。
    ///
    /// **取会话前先清过期**（上游 `_get_session_model:93`）。会话不存在或已过期
    /// → **404**（上游 `LookupError`，路由转 404）。
    pub async fn list_results(
        &self,
        session_id: &str,
        cursor: Option<&str>,
    ) -> Result<ImageSearchPage, ServiceError> {
        self.purge_expired_sessions().await?;
        // `not_found` 的第 4 参是 `i32` 实体 id，而 session_id 是字符串 ——
        // 用 `not_found_with` 才放得下，所以这里走它。
        let session = self
            .sessions
            .find_by_session_id(session_id)
            .await?
            .ok_or_else(|| {
                ServiceError::not_found_with(
                    "image_search_session_not_found",
                    "image search session not found or expired",
                    [("session_id".to_owned(), serde_json::json!(session_id))]
                        .into_iter()
                        .collect(),
                )
            })?;
        let offset = match cursor {
            Some(cursor) => decode_cursor(cursor)?,
            None => 0,
        };
        let vector = parse_query_vector(&session)?;
        self.search_page(
            &session,
            vector,
            offset as usize,
            session.page_size as usize,
        )
        .await
    }
}
impl ImageSearchService {
    /// ★ 循环扫描直到填满页面。
    ///
    /// # ⚠️ 这里曾被我记为「与上游有一处无法避免的行为差异」，**那是错的**
    ///
    /// 我当时的判断是：`DenseStore::search` 没有 `score_threshold` 参数，阈值只能
    /// 在应用层过滤，于是「多取一条判断有没有下一页」的那条**可能被滤掉**，导致
    /// **明明还有下一条却返回 `next_cursor = None`**，且没有任何报错。我把它记成
    /// 「要么改存储层签名、要么循环补足，两条都有代价，所以先记下」。
    ///
    /// **而上游的剧情图那版（`movie_plot_image_search_service.py:184-233`）早就是
    /// 循环补足的** —— 我是在写那个文件时才看到它的。
    ///
    /// 所以现在用**同一个模式**改掉这边。两个要点：
    ///
    /// 1. **循环到填满为止** —— 过滤掉几个就多扫几个，页面永远是满的
    ///    （除非真的没有更多）。
    /// 2. **`raw_offset` 一边扫一边推进** —— 游标记的是**原始扫描位置**，
    ///    不是「返回了几条」。被丢掉的那几条**仍然消耗了扫描位置**，
    ///    用 `items.len()` 当 offset 会导致翻页重复或跳过。
    ///
    /// # 提前退出：`hits.len() < batch_size` 说明到末尾了
    ///
    /// 不满一批就是没更多了（上游 `:232`）。这个判断**在过滤之前**做 ——
    /// 因为「取到的条数」不受过滤影响。
    pub async fn search_page(
        &self,
        session: &sm_db::discovery::image_search::ImageSearchSession,
        vector: Vec<f32>,
        start_offset: usize,
        page_size: usize,
    ) -> Result<ImageSearchPage, ServiceError> {
        let page_size = page_size.max(1);
        let batch_size = page_size.max(SEARCH_SCAN_BATCH_SIZE as usize);
        let movie_ids = parse_id_list(session.movie_ids.as_deref());
        let exclude_movie_ids = parse_id_list(session.exclude_movie_ids.as_deref());
        let threshold = session.score_threshold;
        let mut items: Vec<ImageSearchItem> = Vec::with_capacity(page_size);
        // 原始扫描位置。**不是** `start_offset + items.len()`。
        let mut raw_offset = start_offset;

        loop {
            let scored = self
                .store
                .search(
                    vector.clone(),
                    batch_size,
                    raw_offset,
                    movie_ids.as_deref(),
                    exclude_movie_ids.as_deref(),
                )
                .await?;
            if scored.is_empty() {
                break;
            }
            let hit_count = scored.len();
            for point in scored {
                // **先推进 raw_offset** —— 哪怕这一条被阈值滤掉，位置也消耗了。
                raw_offset += 1;
                let score = super::qdrant::dense::normalize_score(point.score);
                if let Some(threshold) = threshold {
                    if (score as f64) < threshold {
                        continue;
                    }
                }
                // 缺键就跳过这条（与 `qdrant::thumbnail::parse_hit` 同口径）：
                // 上游 `int(payload[...])` 缺键会抛 KeyError 炸掉整次检索，
                // 而这里选择丢掉这一条。
                let (Some(thumbnail_id), Some(media_id)) = (
                    payload_i64(&point.payload, "thumbnail_id"),
                    payload_i64(&point.payload, "media_id"),
                ) else {
                    continue;
                };
                items.push(ImageSearchItem {
                    thumbnail_id,
                    media_id,
                    movie_id: payload_i64(&point.payload, "movie_id"),
                    score,
                });
                if items.len() == page_size {
                    break;
                }
            }
            if items.len() == page_size || hit_count < batch_size {
                break;
            }
        }

        // 「还有下一页」：只要是**因为满了**而停，就认为还有。
        //
        // 保守方向的取舍：多给一个游标 → 客户端多翻一页拿到空列表（可接受）；
        // 少给游标 → **后面的结果永远看不到**（不可接受）。
        let next_cursor = if items.len() == page_size {
            Some(encode_cursor(raw_offset as i64)?)
        } else {
            None
        };
        items.truncate(page_size);
        Ok(ImageSearchPage { items, next_cursor })
    }
}

/// 扫库批大小的**下界**。对应上游 `image_search.search_scan_batch_size`
/// （默认 100，已移植在 `config_schema.rs:400`）。
///
/// 存在的理由：一次只取 `page_size` 条的话，**过滤掉几个就得再扫一轮**，
/// 而每轮都是一次网络往返。取 `max(page_size, 100)` 让常见的「阈值滤掉几个」
/// 在**一轮内**就填满。
pub const SEARCH_SCAN_BATCH_SIZE: i64 = 100;

// ================================================================ 会话相关的共享函数
//
// 剧情图检索（`super::plot_image_search`）用的是**同一张表**、同一套生命周期。
// 上游那两个服务各有一份 `_purge_expired_sessions` / `_get_session_model` 的
// 副本 —— Rust 侧只保留一份，避免「两份实现漂移」。
// （这正是 `normalize_ids` 那次教训的直接应用。）

/// 生成会话 id。上游是 `uuid.uuid4().hex`（`:140`）—— **32 位小写十六进制，
/// 无连字符**（`.hex` 而非 `str(uuid4())`）。
///
/// 用 `Uuid::new_v4().simple()` 对应：同样是 32 个十六进制字符。
/// **不要改成带连字符的形式** —— 会话 id 会进 URL，且库里已有数据是无连字符的。
pub fn new_session_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// 查询向量 → `query_vector` 列的文本（JSON 浮点数组）。
///
/// 上游写的是 `query_vector=[float(item) for item in vector]`，peewee 的
/// `JsonTextField.db_value` 落成 JSON 文本。**逐元素转 f32** 与上游一致，
/// 不是直接序列化 `Vec<f64>`。
pub fn vector_json(vector: &[f32]) -> String {
    serde_json::to_string(vector).unwrap_or_else(|_| "[]".to_owned())
}

/// 读回会话的查询向量。
///
/// 上游是 `session.query_vector or []`（`:205`）—— **缺失即空向量**。
/// 空向量送去检索会命中「距离最近但无意义」的一批，但上游就是这么做的
/// （实际不会发生：建会话必然写向量）。**不在这里报错**，保持与上游一致。
pub fn parse_query_vector(
    session: &sm_db::discovery::image_search::ImageSearchSession,
) -> Result<Vec<f32>, ServiceError> {
    let Some(value) = sm_db::common::decode_json_text(session.query_vector.as_deref()) else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Ok(Vec::new());
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        // 上游 `float(item)`：整数与浮点都收，字符串也收（能转就转）。
        // 这里只收数字 —— 库里不该出现字符串，出现就是脏数据，丢掉整条。
        match item.as_f64() {
            Some(number) => out.push(number as f32),
            None => {
                return Err(ServiceError::validation(
                    "image_search_session_vector_invalid",
                    "会话存的查询向量不是数字数组",
                ));
            }
        }
    }
    Ok(out)
}

/// 读回会话的 id 列表。**空列表归一成 `None`（不过滤）**。
///
/// 与 [`normalize_ids`] 同一条规则（`if not ids: return None`）：空与缺失
/// **都是「不过滤」**，不是「排除全部」。这里是读侧的最后一道 —— 即便库里
/// 存了个 `[]`，读回来也还是「不过滤」。
pub fn parse_id_list(raw: Option<&str>) -> Option<Vec<i64>> {
    let value = sm_db::common::decode_json_text(raw)?;
    let items = value.as_array()?;
    let mut out: Vec<i64> = Vec::with_capacity(items.len());
    for item in items {
        // 非整数元素跳过，不整条作废 —— 上游 `int(...)` 会对脏值抛异常，
        // 那会让一行脏数据让整次翻页 500。
        if let Some(id) = item.as_i64() {
            out.push(id);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// 建一个会话行。**只写会话本身**，排除条件要另调 `set_exclusions`。
///
/// ⚠️ `NewImageSearchSession` **没有** `movie_ids` / `exclude_movie_ids` 字段，
/// 而上游是一个 `create` 写全 —— 所以调用方**必须紧跟着**补排除条件，
/// 中间留窗口期会让会话可见但过滤未生效。
pub async fn create_session(
    repo: &ImageSearchSessionRepository,
    vector: Vec<f32>,
    page_size: i64,
    score_threshold: Option<f64>,
    session_ttl_seconds: i64,
) -> Result<sm_db::discovery::image_search::ImageSearchSession, ServiceError> {
    let now = now_utc();
    Ok(repo
        .create(&NewImageSearchSession {
            session_id: new_session_id(),
            page_size: page_size as i32,
            query_vector: Some(vector_json(&vector)),
            score_threshold,
            expires_at: now + chrono::Duration::seconds(session_ttl_seconds),
        })
        .await?)
}

/// 取会话，不存在或已过期 → **404**。
///
/// 上游 `_get_session_model`（`:92-97`）抛 `LookupError`，路由转 404。
/// **先清过期**（`:93`）—— 否则「刚过期但还没被清掉」的会话会被当成有效。
pub async fn require_session(
    repo: &ImageSearchSessionRepository,
    session_id: &str,
) -> Result<sm_db::discovery::image_search::ImageSearchSession, ServiceError> {
    purge_expired_sessions(repo).await?;
    repo.find_by_session_id(session_id).await?.ok_or_else(|| {
        ServiceError::not_found_with(
            "image_search_session_not_found",
            "image search session not found or expired",
            [("session_id".to_owned(), serde_json::json!(session_id))]
                .into_iter()
                .collect(),
        )
    })
}

/// 清理过期会话。**读路径上带一次写。**
///
/// 不清的话会话表无界增长（每次建会话与取会话都产生一行）。
pub async fn purge_expired_sessions(
    repo: &ImageSearchSessionRepository,
) -> Result<u64, ServiceError> {
    Ok(repo.delete_expired(now_utc()).await?)
}

/// 读回会话存的查询向量。
pub fn session_vector(
    session: &sm_db::discovery::image_search::ImageSearchSession,
) -> Result<Vec<f32>, ServiceError> {
    parse_query_vector(session)
}

/// 读回会话存的 id 列表。**空列表归一成 `None`（不过滤）**。
pub fn session_id_list(raw: Option<&str>) -> Option<Vec<i64>> {
    parse_id_list(raw)
}

/// 单图取向量（建会话用）。
pub async fn embed_one_image(
    embedding: &EmbeddingClient,
    image_bytes: &[u8],
) -> Result<Vec<f32>, ServiceError> {
    let mut vectors = embedding.embed_images(&[image_bytes.to_vec()]).await?;
    if vectors.is_empty() {
        return Err(ServiceError::validation(
            "image_search_embedding_empty",
            "推理服务没有返回向量",
        ));
    }
    Ok(vectors.remove(0))
}

/// 单文本取向量（建会话用）。
pub async fn embed_one_text(
    embedding: &EmbeddingClient,
    text: &str,
) -> Result<Vec<f32>, ServiceError> {
    let mut vectors = embedding.embed_texts(&[text.to_owned()]).await?;
    if vectors.is_empty() {
        return Err(ServiceError::validation(
            "image_search_embedding_empty",
            "推理服务没有返回向量",
        ));
    }
    Ok(vectors.remove(0))
}

/// 索引就绪闸门（两个检索服务共用）。
///
/// 上游 `_ensure_searchable_index`（`:99-112`）：`describe()` 失败 → 透传推理
/// 服务的错误码（503 / 502）；`ensure_search_ready` 抛重建错误 → **409**。
pub async fn ensure_searchable_index(
    embedding: &EmbeddingClient,
    space: &ImageSearchIndexSpaceService,
) -> Result<super::embedding::EmbeddingSpace, ServiceError> {
    let described = embedding.describe().await?;
    space.ensure_search_ready(&described.space_id).await?;
    Ok(described)
}

/// `i64` id 列表 -> `i32`（仓储层那一侧）。
///
/// **为什么需要转**：服务层与路由层用 `i64`（Rust 默认、且 `ServiceError`
/// 的 `not_found` 第 4 参是 `i32`），而 `ImageSearchSessionRepository` 的签名
/// 用 `i32`（与表列类型一致）。
///
/// **`as` 转换是安全的**：这些 id 来自 `bigint` 主键，实际值远小于 `i32::MAX`；
/// 万一超了，`as` 会截断成负数 —— 那会让过滤条件变成「匹配不到任何东西」
/// 而不是报错。**真要防的话该在仓储层校验**，不在这里。
pub(crate) fn to_i32(ids: Option<&[i64]>) -> Option<Vec<i32>> {
    ids.map(|slice| slice.iter().map(|id| *id as i32).collect())
}
