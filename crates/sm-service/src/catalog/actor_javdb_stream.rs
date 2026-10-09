//! 演员搜索流式导入（上游 `actor_service.stream_search_and_upsert_actor_from_javdb`，
//! `actor_service.py:437-605`）。
//!
//! # 帧序列（7 类，逐帧对拍上游）
//!
//! ```text
//! search_started
//!   ├─ 搜不到 ─→ completed {success: false, reason: "actor_not_found"}
//!   └─ 来源挂了 ─→ completed {success: false, reason: "internal_error"}
//! actor_found {actors: [{javdb_id, name, avatar_url}], total}
//! upsert_started {total}
//!   （逐条）image_download_started → image_download_finished
//! upsert_finished {total, created_count, already_exists_count, failed_count}
//! completed {success, actors?, failed_items, stats?}
//! ```
//!
//! # 两处**结构**差异（都写清了理由，别照上游想当然）
//!
//! 1. **没有 `image_download_*` 的真实下载**：本仓没有 image store，入库不写
//!    头像（见 [`CatalogImportService::upsert_actor_from_javdb_resource`] 的文档）。
//!    但两帧**照发**、`has_avatar` 仍取自**资源**上有没有头像 URL ——
//!    上游该字段本就是 `bool(actor_resource.avatar_url)`，与是否落盘无关；
//!    帧不发才是真的破坏契约（客户端按帧名画进度）。
//! 2. **`failed_items` 只有 `upsert_failed` 一类**：上游另有一类
//!    `image_download_failed`（捕获 `ImageDownloadError`）。本仓没有下载，
//!    那一类**永不可能触发**，所以这里不写死分支去捕获一个不存在的错误类型。
//!
//! # 早退与失败是**同一件事**的两面
//!
//! 搜不到、来源挂了、以及「全部失败导致一个都没入库」三种情况都发
//! `completed {success: false}`，区别只在 `reason`（前两者）与
//! `failed_items`（后者）。客户端只认 `success`：**不新增「HTTP 500 中途断流」**
//! —— 流一旦开始，就一定以一个 `completed` 结束。
//!
//! [`CatalogImportService::upsert_actor_from_javdb_resource`]: super::catalog_import::CatalogImportService::upsert_actor_from_javdb_resource

use std::sync::Arc;

use sm_db::repo::ActorRepository;
use sm_db::Db;

use serde_json::Value;

use super::actor::{ActorService, ActorView};
use super::metadata_source::{MetadataSourceError, MetadataSourceService};
use super::movie_metadata_refresh::UpsertStats;

/// `actor_found` 里的一条候选（上游那个内联字典）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ActorCandidate {
    pub javdb_id: String,
    pub name: String,
    pub avatar_url: Option<String>,
}

/// 一条失败项（上游 `failed_items` 的元素）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ActorFailure {
    pub javdb_id: String,
    /// `upsert_failed`（本仓唯一的类别，见模块文档）。
    pub reason: &'static str,
    pub detail: String,
}

/// 流式事件（SSE 帧的类型化形态）。事件名与载荷照上游逐帧对齐。
///
/// `completed` 的 `actors` 是**服务形态**的视图（[`ActorView`]），线格式由
/// 路由层经 `ActorResource::from_view` 转换 —— 头像签名密钥只有路由层有。
/// `reason` / `failed_items` / `stats` 是**可选**的：上游的成功帧不带
/// `reason`，早退帧不带 `failed_items`/`stats`。
#[derive(Debug)]
pub enum ActorStreamFrame {
    SearchStarted {
        actor_name: String,
    },
    ActorFound {
        actors: Vec<ActorCandidate>,
        total: i64,
    },
    UpsertStarted {
        total: i64,
    },
    ImageDownloadStarted {
        javdb_id: String,
        index: i64,
        total: i64,
    },
    ImageDownloadFinished {
        javdb_id: String,
        index: i64,
        total: i64,
        has_avatar: bool,
    },
    UpsertFinished {
        stats: UpsertStats,
    },
    Completed {
        success: bool,
        reason: Option<&'static str>,
        actors: Vec<ActorView>,
        failed_items: Vec<ActorFailure>,
        stats: Option<UpsertStats>,
    },
}

/// 演员搜索流式导入服务。
pub struct ActorJavdbStreamService {
    db: Db,
    source: Arc<MetadataSourceService>,
    import: super::catalog_import::CatalogImportService,
}

impl ActorJavdbStreamService {
    pub fn new(
        db: &Db,
        source: Arc<MetadataSourceService>,
        import: super::catalog_import::CatalogImportService,
    ) -> Self {
        Self {
            db: db.clone(),
            source,
            import,
        }
    }

    /// ★ 流式搜索并入库。上游 `stream_search_and_upsert_actor_from_javdb`
    /// （`:437-605`）。
    ///
    /// # 演员名的归一在**路由层**
    ///
    /// 上游的 `actor_name.strip()` 一半在 schema 校验里（`min_length=1` +
    /// strip 后为空即 422），这里收到的已是去过空白的名字；`search_started`
    /// 回显的也是归一后的值 —— 客户端据此对齐它发的请求与收到的进度。
    ///
    /// # 两次去重，语义**不同**
    ///
    /// 1. 搜索候选按 `javdb_id` 去重（在 provider 里做，上游也在那里）；
    /// 2. 入库结果按**本地 canonical id** 去重（本方法末尾）—— JavDB 的不同
    ///    卡片可能都已并到同一条保留记录，不去重的话 `completed.actors` 里
    ///    会出现同一个本地演员两遍。
    ///
    /// ⚠️ 与影片那条流同样：本仓用 `Vec` 代替真流式，**全部完成才返回**。
    /// 帧序不变，只是到达时间被压缩 —— 别用它驱动进度条。
    pub async fn stream_search_and_upsert_actor_from_javdb(
        &self,
        actor_name: &str,
    ) -> Vec<ActorStreamFrame> {
        let normalized_name = actor_name.trim().to_owned();
        let mut frames = vec![ActorStreamFrame::SearchStarted {
            actor_name: normalized_name.clone(),
        }];

        // ① 搜索。两种失败都早退，reason 不同（上游 `:445-463`）。
        let resources = match self.source.search_actors(&normalized_name).await {
            Ok(resources) => resources,
            Err(MetadataSourceError::NotFound) => {
                frames.push(completed_early("actor_not_found"));
                return frames;
            }
            Err(error) => {
                tracing::warn!(detail = ?error, "JavDB 演员搜索失败");
                frames.push(completed_early("internal_error"));
                return frames;
            }
        };
        // ② 候选原样进循环。
        //
        // ⚠️ **不要**在这里把 `Value` 转成结构体再 `filter_map(...ok())`：
        // 转换一失败就会**静默丢掉一位候选**，而 `total` 还是按原数量算 ——
        // 用户看到「共 3 位」却只处理了 2 位，且没有任何失败项解释差在哪。
        // provider 交付的就是「要被写库的那份数据」，形状不对属于**入库失败**，
        // 交给 upsert 去报（它本来就会校验 `javdb_id` / `name` 并给出错误码）。
        let total = resources.len() as i64;
        frames.push(ActorStreamFrame::ActorFound {
            actors: resources
                .iter()
                .map(|actor| ActorCandidate {
                    javdb_id: text_field(actor, "javdb_id"),
                    name: text_field(actor, "name"),
                    avatar_url: actor
                        .get("avatar_url")
                        .and_then(Value::as_str)
                        .filter(|url| !url.is_empty())
                        .map(str::to_owned),
                })
                .collect(),
            total,
        });
        frames.push(ActorStreamFrame::UpsertStarted { total });

        // ③ 逐条入库。失败只记账，**不中断整条流**（上游同样 `continue`）。
        let repo = ActorRepository::new(self.db.clone());
        let mut created_count = 0_i64;
        let mut already_exists_count = 0_i64;
        let mut failed_count = 0_i64;
        let mut failed_items: Vec<ActorFailure> = Vec::new();
        let mut imported_actors: Vec<ActorView> = Vec::new();

        for (offset, resource) in resources.iter().enumerate() {
            let index = offset as i64 + 1;
            let javdb_id = text_field(resource, "javdb_id");
            // 图片下载是前端最关心的慢步骤，单独发事件（本仓无下载，帧照发）。
            frames.push(ActorStreamFrame::ImageDownloadStarted {
                javdb_id: javdb_id.clone(),
                index,
                total,
            });
            // 「入库前存在吗」要在 upsert **之前**问 —— 之后再问永远是 true，
            // `created_count` 会恒为 0。
            let existed_before = repo
                .find_by_javdb_id(&javdb_id)
                .await
                .ok()
                .flatten()
                .is_some();
            let upsert = async {
                // `update_gender = false`：演员搜索只能同步身份与头像，**不覆盖
                // 用户改过的性别**（上游同款默认值）。
                let actor_id = self
                    .import
                    .upsert_actor_from_javdb_resource(resource, false)
                    .await?;
                ActorService::new(&self.db).detail(actor_id).await
            }
            .await;

            match upsert {
                Ok(view) => {
                    if existed_before {
                        already_exists_count += 1;
                    } else {
                        created_count += 1;
                    }
                    frames.push(ActorStreamFrame::ImageDownloadFinished {
                        javdb_id: javdb_id.clone(),
                        index,
                        total,
                        // 取自**资源**上有没有头像 URL（上游 `bool(
                        // actor_resource.avatar_url)`），与是否落盘无关。
                        has_avatar: resource
                            .get("avatar_url")
                            .and_then(Value::as_str)
                            .map(|url| !url.is_empty())
                            .unwrap_or(false),
                    });
                    imported_actors.push(view);
                }
                Err(error) => {
                    failed_count += 1;
                    tracing::warn!(
                        javdb_id = %javdb_id,
                        code = error.code(),
                        "演员入库失败"
                    );
                    failed_items.push(ActorFailure {
                        javdb_id,
                        reason: "upsert_failed",
                        detail: error.api.message,
                    });
                }
            }
        }

        // ④ 按本地 canonical id 再去重（上游 `:565-573`）。
        let mut seen_actor_ids: std::collections::HashSet<i32> = std::collections::HashSet::new();
        imported_actors.retain(|view| seen_actor_ids.insert(view.actor.id));

        let stats = UpsertStats {
            total,
            created_count,
            already_exists_count,
            failed_count,
        };
        frames.push(ActorStreamFrame::UpsertFinished { stats });

        // ⑤ 收尾。一个都没入库 → `success: false`（即使「失败数」为 0，比如
        // 候选全被判为合并且没有可返回的视图）。
        if imported_actors.is_empty() {
            frames.push(ActorStreamFrame::Completed {
                success: false,
                reason: Some("internal_error"),
                actors: Vec::new(),
                failed_items,
                stats: Some(stats),
            });
        } else {
            frames.push(ActorStreamFrame::Completed {
                success: true,
                reason: None,
                actors: imported_actors,
                failed_items,
                stats: Some(stats),
            });
        }
        frames
    }
}

/// 取一个文本字段，缺失/非字符串/空串都算空串。
///
/// 进帧的 `javdb_id` / `name` 因此**恒是字符串**（不会 `null`），与上游
/// pydantic 模型的 `str` 字段一致。
fn text_field(resource: &Value, key: &str) -> String {
    resource
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// 早退的 `completed` 帧：**不带** `failed_items` / `stats`（上游早退帧就没有
/// 这两个键 —— 塞空值会让客户端把「没跑」渲染成「跑了但什么都没做成」）。
fn completed_early(reason: &'static str) -> ActorStreamFrame {
    ActorStreamFrame::Completed {
        success: false,
        reason: Some(reason),
        actors: Vec::new(),
        failed_items: Vec::new(),
        stats: None,
    }
}
