//! 索引空间状态机 + 入口图片归一化。
//!
//! 上游 `image_search_index_space_service.py`（4.5KB / 115 行）+ `image_search_input.py`（848B）。
//! **纯 PostgreSQL。**
//!
//! # 这是整个 image_search 的**状态机**，不是查询服务
//!
//! 它回答一个问题：**当前索引能不能查**。四个状态（上游 `:45-72`）：
//!
//! ```text
//!   current_space_id 为空                      -> unavailable    （推理服务没配 / 不可达）
//!   indexed_space_id == current_space_id       -> ready
//!   indexed_space_id 非空但不匹配                -> rebuild_required（空间变了）
//!   indexed_space_id 为空 且 从没成功索引过      -> uninitialized  （还没开始）
//!   indexed_space_id 为空 但 成功索引过          -> rebuild_required（行丢了）
//! ```
//!
//! 最后一行的判据是 `_has_completed_index_records()` —— 它查缩略图 / 剧情图的
//! 索引状态。**单例表可能没有行**，所以判据必须落在别处。
//!
//! # `current_space_id` 是**推理服务**说的，不是配置里写的
//!
//! 来自 [`super::embedding::EmbeddingClient::describe`] 的 `space_id`。
//! 所以「推理服务挂了」会表现为 `current_space_id = None` → `unavailable`。
//!
//! # 三处最容易照抄错的地方
//!
//! **1. `ensure_search_ready` 只在 `rebuild_required` 时抛错 —— `unavailable`
//! 不抛。**
//!
//! 推理服务不可用时**仍然允许查询**（走已有索引，只是没法取新向量）。
//! 照抄。把它改成「unavailable 也抛」会让推理服务一挂，图搜整个不可用。
//!
//! **2. `prepare_for_indexing` 在 `uninitialized` 时就把状态行建成
//! `indexed_space_id = current`。**
//!
//! 也就是**在还没索引任何东西之前就宣称「已索引」**。这是上游行为，照抄 ——
//! 但要意识到后果：首次索引中途失败时，状态行已经写了 `current_space_id`，
//! 而实际没有向量。之后 `get_status` 会返回 `ready`，查询侧不报错但**搜不到东西**。
//! `accepts_session` 那条不变量拦不住这种情况（它比的是维度，不是「有没有向量」）。
//!
//! **3. `prepare_for_indexing` 在 `unavailable` 时抛的是「需重建」。**
//!
//! 不是「推理服务不可用」而是「需重建」—— 语义上不准确（服务不可用不需要
//! 重建），但上游如此。照抄，别自己改成更「合理」的：客户端已经按
//! `movie_similarity` 之外的那套 code 在处理它。
//!
//! # `details` 有三个键，`reason` 最容易被漏
//!
//! 上游 `:29-38`：
//!
//! | 键 | 取值 |
//! |---|---|
//! | `reason` | `space_id_changed`（`indexed_space_id` 非空）/ `historical_space_unknown`（为空）|
//! | `indexed_space_id` | 可能有值 |
//! | `current_space_id` | 可能有值 |
//!
//! `reason` 区分的是「换了模型」与「历史丢了」—— **两者的处置完全不同**：
//! 前者要重建全部，后者要先查为什么行没了。

use serde::{Deserialize, Serialize};
use sm_db::repo::discovery::ImageSearchIndexStateRepository;

use crate::error::ServiceError;

/// `unavailable` —— 推理服务没配或不可达。
pub const STATE_UNAVAILABLE: &str = "unavailable";
/// `ready` —— 索引与当前空间一致，可查。
pub const STATE_READY: &str = "ready";
/// `rebuild_required` —— 索引与当前空间不一致（或状态行丢失但索引过）。
pub const STATE_REBUILD_REQUIRED: &str = "rebuild_required";
/// `uninitialized` —— 从没成功索引过。
pub const STATE_UNINITIALIZED: &str = "uninitialized";

/// 索引空间状态。
///
/// # `state` 是四值字符串，**不是布尔**
///
/// 上游用四个具名常量。**不要**压成 `searchable: bool` —— `unavailable` 与
/// `rebuild_required` 都是「不可查」，但**处置完全不同**（前者等推理服务、
/// 后者要重建索引），客户端要能分开显示。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSearchIndexSpaceStatus {
    /// 四态之一。
    pub state: String,
    /// 索引里记录的已索引空间。`None` = 状态行不存在或为空。
    pub indexed_space_id: Option<String>,
    /// 推理服务当前声明的空间。`None` = 不可达 / 未配置。
    pub current_space_id: Option<String>,
}

impl ImageSearchIndexSpaceStatus {
    /// 是否可查。
    ///
    /// **只有 `ready` 与 `unavailable` 为 `true`** —— 见模块文档第 1 条。
    pub fn is_searchable(&self) -> bool {
        self.state == STATE_READY || self.state == STATE_UNAVAILABLE
    }

    /// 是否已就绪（严格意义：索引与空间一致）。
    pub fn is_ready(&self) -> bool {
        self.state == STATE_READY
    }
}

/// 索引需重建。
///
/// 上游 `ImageSearchIndexRebuildRequiredError`（`:21-39`）。它**不是**「查不到」
/// 而是「索引本身不可信」—— 检索会返回**语义无关**的结果而不报错。
#[derive(Debug, Clone)]
pub struct ImageSearchIndexRebuildRequired {
    pub status: ImageSearchIndexSpaceStatus,
}

impl std::fmt::Display for ImageSearchIndexRebuildRequired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("图片搜索索引需要重建")
    }
}

impl std::error::Error for ImageSearchIndexRebuildRequired {}

impl ImageSearchIndexRebuildRequired {
    /// 错误 details。**三个键**，`reason` 区分「换模型」与「历史丢失」。
    pub fn details(&self) -> Vec<(String, Option<String>)> {
        vec![
            (
                "reason".to_owned(),
                Some(
                    if self.status.indexed_space_id.is_some() {
                        "space_id_changed"
                    } else {
                        "historical_space_unknown"
                    }
                    .to_owned(),
                ),
            ),
            (
                "indexed_space_id".to_owned(),
                self.status.indexed_space_id.clone(),
            ),
            (
                "current_space_id".to_owned(),
                self.status.current_space_id.clone(),
            ),
        ]
    }

    /// 转成 `ServiceError`（409 —— 「状态不对，重建再来」）。
    ///
    /// 上游在路由层怎么映射要看具体 router；这里选 **409** 而不是 503 ——
    /// 503 表示「稍后重试就会好」，而这个**不会自己好**，必须人去触发重建。
    pub fn into_service_error(self) -> ServiceError {
        ServiceError::conflict(
            "image_search_index_rebuild_required",
            "图片搜索索引需要重建",
            // `details()` 的值是可空字符串，而 `Map<String, Value>` 要的是
            // `Value` —— `None` 显式落成 `Null`（**不省略这个键**：客户端要靠
            // 它区分「没有历史空间」与「读不到」）。
            Some(
                self.details()
                    .into_iter()
                    .map(|(key, value)| {
                        (
                            key,
                            value
                                .map(serde_json::Value::String)
                                .unwrap_or(serde_json::Value::Null),
                        )
                    })
                    .collect(),
            ),
        )
    }
}
/// 索引空间服务。
pub struct ImageSearchIndexSpaceService {
    repo: ImageSearchIndexStateRepository,
}

impl ImageSearchIndexSpaceService {
    /// 构造。
    pub fn new(repo: ImageSearchIndexStateRepository) -> Self {
        Self { repo }
    }

    /// 归一化 `current_space_id`。
    ///
    /// 上游 `(current_space_id or "").strip() or None`（`:48`）——
    /// **空串与纯空白都算 `None`**。不归一化的话 `Some("")` 会与状态行里的
    /// 某个值比较出「不匹配」，把「推理服务没回空间号」误报成「空间变了」。
    pub fn normalize_current(current_space_id: Option<&str>) -> Option<String> {
        current_space_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    }

    /// 状态机的**纯逻辑**部分。
    ///
    /// 拆出来是为了能直接测 —— 上游那 20 行分支（`:50-72`）是这个服务里最容易
    /// 写错的部分（第五个分支是「状态行没了但索引过」）。
    pub fn classify(
        indexed_space_id: Option<String>,
        current_space_id: Option<&str>,
        has_completed_records: bool,
    ) -> ImageSearchIndexSpaceStatus {
        let current = Self::normalize_current(current_space_id);
        // 1. 推理服务不可达 / 未配置。不查有没有索引过 —— 问了也没用。
        if current.is_none() {
            return ImageSearchIndexSpaceStatus {
                state: STATE_UNAVAILABLE.to_owned(),
                indexed_space_id,
                current_space_id: None,
            };
        }
        let current = current.expect("上面已判过非空");
        // 2. 一致。
        if indexed_space_id.as_deref() == Some(current.as_str()) {
            return ImageSearchIndexSpaceStatus {
                state: STATE_READY.to_owned(),
                indexed_space_id,
                current_space_id: Some(current),
            };
        }
        // 3. 索引过（状态行有值，或缩略图/剧情图里有成功记录）-> 要重建。
        if indexed_space_id.is_some() || has_completed_records {
            return ImageSearchIndexSpaceStatus {
                state: STATE_REBUILD_REQUIRED.to_owned(),
                indexed_space_id,
                current_space_id: Some(current),
            };
        }
        // 4. 从没成功索引过。
        ImageSearchIndexSpaceStatus {
            state: STATE_UNINITIALIZED.to_owned(),
            indexed_space_id: None,
            current_space_id: Some(current),
        }
    }

    /// 当前状态。`current_space_id` 来自推理服务的 `describe()`。
    ///
    /// **短路顺序要紧**：`unavailable` 分支**不查**
    /// [`has_completed_index_records`](ImageSearchIndexStateRepository) ——
    /// 那个查询要扫两张表，而推理服务不可用时问了也不用。
    pub async fn get_status(
        &self,
        current_space_id: Option<&str>,
    ) -> Result<ImageSearchIndexSpaceStatus, ServiceError> {
        let indexed = self.repo.get().await?.map(|state| state.indexed_space_id);
        // 提前判「不可达」是为了省掉那次 EXISTS —— 见上面说明。
        if Self::normalize_current(current_space_id).is_none() {
            return Ok(Self::classify(indexed, None, false));
        }
        let has_records = self.repo.has_completed_index_records().await?;
        Ok(Self::classify(indexed, current_space_id, has_records))
    }

    /// 查询前的闸门。**只在 `rebuild_required` 时抛**（见模块文档第 1 条）。
    pub async fn ensure_search_ready(&self, current_space_id: &str) -> Result<(), ServiceError> {
        let status = self.get_status(Some(current_space_id)).await?;
        if status.state == STATE_REBUILD_REQUIRED {
            return Err(ImageSearchIndexRebuildRequired { status }.into_service_error());
        }
        Ok(())
    }

    /// 索引前的闸门。三分支（见模块文档第 2、3 条）。
    ///
    /// `uninitialized` 时**把状态行建成已索引** —— 上游行为，照抄。后果写在
    /// 模块文档里：首次索引中途失败会让状态说「已索引」而实际没有向量。
    pub async fn prepare_for_indexing(&self, current_space_id: &str) -> Result<(), ServiceError> {
        let status = self.get_status(Some(current_space_id)).await?;
        match status.state.as_str() {
            // 已经是当前空间 -> 无事可做。
            STATE_READY => Ok(()),
            // 从没索引过 -> 直接建状态行。
            STATE_UNINITIALIZED => {
                self.repo.set_indexed_space(current_space_id).await?;
                Ok(())
            }
            // rebuild_required 与 unavailable 都落到这里（模块文档第 3 条）。
            _ => Err(ImageSearchIndexRebuildRequired { status }.into_service_error()),
        }
    }

    /// 标记某空间已索引完成。
    ///
    /// 仓储层的 `set_indexed_space` 是 **upsert**（`ON CONFLICT (id) DO UPDATE`），
    /// 所以「行不存在 -> 建」与「值相同 -> 无变化」两种情况都被它覆盖 ——
    /// 上游那三个分支（`:92-98`）在 SQL 层面是一个语句。
    pub async fn set_indexed_space(&self, current_space_id: &str) -> Result<(), ServiceError> {
        self.repo.set_indexed_space(current_space_id).await?;
        Ok(())
    }
}
/// 归一化检索用图片字节。
///
/// 上游 `normalize_image_search_query`（`image_search_input.py:7`，全文
/// **18 行**）。**这是整个 image_search 的入口转换** —— 拿到的字节要转成
/// 推理服务能吃的格式。
///
/// # 为什么放在这个文件而不是 `image_search.rs`
///
/// 它被**两条**检索路径共用（图搜与剧情图搜），放进任一个都会让另一条反向
/// 依赖。这个文件是两者共同的**前置状态**，所以放这儿。
///
/// # 上游全文只有 18 行，而它是个**转换器**不是校验器
///
/// 骨架期这里写「具体校验规则（尺寸上限、格式白名单）没在骨架阶段确认，所以
/// 保留签名但不猜实现」。**读完上游全文后这句话作废** —— 骨架作者把
/// 「848B」当成了行数（它是**字节数**），因而以为后面还有一大堆没读到。真相是
/// 那 18 行里**既没有尺寸上限，也没有格式白名单**：
///
/// ```python
/// with PillowImage.open(BytesIO(image_bytes)) as image:
///     image.seek(0); image.load()
///     normalized = ImageOps.exif_transpose(image)          # 方向转正
///     if normalized.mode not in {"RGB", "RGBA"}:            # 模式归一
///         normalized = normalized.convert("RGBA" if "transparency" in normalized.info else "RGB")
///     normalized.save(output, format="WEBP", lossless=True) # 无损 WebP
/// ```
///
/// 真正要做的只有三件：**EXIF 转正 → 模式归一 → 无损 WebP**。尺寸上限与格式
/// 白名单在**别的**文件（`image_search_service.py`）里，接那两条端点时再去读。
///
/// # 为什么签名从 `Vec<u8>` 改成 `Result`
///
/// 骨架签名 **无法表达失败**，而上游会抛 `ValueError`（路由层转成 **400**）。
/// 强行 `unwrap` 或返回空 `Vec`，都会把「用户传了张坏图」变成「静默搜不出
/// 东西」—— 那比报错难查得多。
///
/// # 模式归一的一处**已知差异**（比上游更保守）
///
/// 上游按 `"transparency" in info` 决定 RGBA / RGB；这里按解码后的
/// `color().has_alpha()`。差异只在 **PNG 的 `tRNS` 块**上：Pillow 把它当透明
/// 通道，本仓解码器**丢掉**它，于是「灰度 + `tRNS`」的 PNG 会从带 alpha 变成
/// 不带。图搜场景里**没有可见后果**（只用像素内容做 embedding，不看 alpha），
/// 但别把它当「完全等价」—— 动解码器时要记得这条。
pub fn normalize_image_search_query(image_bytes: &[u8]) -> Result<Vec<u8>, ServiceError> {
    // 转换链（EXIF 转正 / 模式归一 / 无损 WebP）在 `svc_image` 里 —— 那才是
    // 唯一依赖 `image` 的地方，测试也才能造真图。这里只做**错误映射**。
    //
    // 400 而不是 422：上游是 `raise ValueError` → 路由 `except ValueError` →
    // `HTTPException(400)`（`image_search.py:56-57`）。而 422 在本仓是
    // 「请求体本身不合 schema」—— 图能解码失败不是 schema 的问题。
    svc_image::normalize_for_embedding(image_bytes).map_err(|error| {
        ServiceError::from_status(
            400,
            "invalid_image_search_query",
            format!("上传图片无效或不支持的格式：{error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 坏输入 → **400 `invalid_image_search_query`**（不是 500、不是静默空图）。
    ///
    /// 转换本身（EXIF 转正 / 模式归一 / 无损 WebP）的真图测试在
    /// [`svc_image::normalize_for_embedding`] 那侧 —— 那里才有 `image` crate
    /// 能造 PNG / JPEG / 带 EXIF 的图。本文件是薄封装，只负责错误映射。
    #[test]
    fn garbage_input_is_a_400_not_a_500() {
        for bad in [
            b"not an image at all".to_vec(),
            Vec::new(),
            // 只有 SOI 的截断 JPEG
            vec![0xFF, 0xD8, 0xFF],
            // RIFF 容器但不是 WebP
            b"RIFF\x00\x00\x00\x00WAVEfmt ".to_vec(),
        ] {
            let error = normalize_image_search_query(&bad).expect_err("坏输入该报错");
            assert_eq!(error.status, 400, "{bad:?}");
            assert_eq!(error.code(), "invalid_image_search_query");
        }
    }

    /// 错误消息要说清是「图」的问题，不是把它当成内部故障。
    #[test]
    fn the_message_names_the_problem_as_the_uploaded_image() {
        let error = normalize_image_search_query(b"nope").expect_err("该报错");
        assert!(
            error.api.message.contains("图片"),
            "消息要指向用户能理解的原因：{}",
            error.api.message
        );
    }
}
