//! `transfers` 域模型（6 张表，全部在 `downloads.py`）。
//!
//! 对应 `src/model/transfers/downloads.py`。
//!
//! # 两处刻意的反范式设计（迁移时千万别"修正"）
//!
//! **1. `download_submission_record` 用裸整数列而非外键**
//!
//! ```python
//! client_id = peewee.IntegerField()      # 不是 ForeignKeyField
//! task_id = peewee.IntegerField(null=True, index=True)
//! ```
//!
//! 注释写得很直接：「保留提交历史，不随下载任务或下载器删除」。
//! 若在迁移时"顺手"改成真外键 + CASCADE，提交历史会被连带删除，
//! 而这正是该表存在的意义。
//!
//! **2. `download_task` 有两个互不相干的状态机**
//!
//! | 列 | 归属 | 默认值 |
//! |---|---|---|
//! | `state` | provider 的远端下载状态 | `queued` |
//! | `import_status` | 宿主自己的导入流程 | `pending` |
//!
//! 注释：「导入是宿主自己的业务流程，不能与 provider 的远端状态混用」。
//! 合并成一个 status 列会丢掉「下载完了但导入失败」这个真实存在的状态组合。

use chrono::NaiveDateTime;
use sqlx::FromRow;

/// `download_client` 表：下载器实例。
#[derive(Debug, Clone, FromRow)]
pub struct DownloadClient {
    pub id: i32,
    /// 全局唯一。
    pub name: String,
    /// 不透明 JSON 文本，由 provider 解释。宿主只保存与原样回传。
    pub provider_config: Option<String>,
    /// 归属库。删库会连带删除该库的下载器。
    pub library_id: i32,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl DownloadClient {
    /// 解析 `provider_config` 文本。空串与非法 JSON 都视为 `None`。
    pub fn parsed_config(&self) -> Option<serde_json::Value> {
        let raw = self.provider_config.as_deref()?.trim();
        if raw.is_empty() {
            return None;
        }
        serde_json::from_str(raw).ok()
    }
}

/// 索引器种类（Torznab）。
pub mod indexer_kind {
    /// 站点型索引器。
    pub const PT: &str = "pt";
    /// 种子型索引器。
    pub const BT: &str = "bt";

    pub const ALL: [&str; 2] = [PT, BT];

    pub fn is_valid(kind: &str) -> bool {
        ALL.contains(&kind)
    }
}

/// `indexer` 表：Torznab 索引器。
#[derive(Debug, Clone, FromRow)]
pub struct Indexer {
    pub id: i32,
    /// 全局唯一。
    pub name: String,
    /// Torznab 搜索接口地址。
    pub url: String,
    /// `pt` / `bt`。数据库无 CHECK 约束。
    pub kind: String,
    /// 每个索引器独立的 Torznab 鉴权 key。
    ///
    /// 为空时搜索请求**不带** `apikey` 参数 —— 这不是缺失值，而是协议要求。
    pub api_key: Option<String>,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl Indexer {
    /// 该索引器是否需要携带 apikey 参数。
    pub fn requires_apikey(&self) -> bool {
        self.api_key
            .as_deref()
            .is_some_and(|key| !key.trim().is_empty())
    }
}

/// `indexer_download_client` 表：索引器与下载器的多对多绑定。
///
/// 唯一索引 `(indexer, download_client)` —— 同一组合不重复绑定。
#[derive(Debug, Clone, FromRow)]
pub struct IndexerDownloadClient {
    pub id: i32,
    pub indexer_id: i32,
    pub download_client_id: i32,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

/// 下载任务的远端状态（由 provider 解释）。
///
/// 与 `import_status` 是**两个独立状态机**，不要合并。
pub mod download_state {
    /// 初始态：已排队，尚未提交给下载器。
    pub const QUEUED: &str = "queued";
    /// 已提交给下载器，等待接单。
    pub const SUBMITTED: &str = "submitted";
    /// 下载器已开始下载。
    pub const DOWNLOADING: &str = "downloading";
    /// 下载完成，产物待导入。
    pub const COMPLETED: &str = "completed";
    /// 终态：失败。
    pub const FAILED: &str = "failed";

    pub fn is_terminal(state: &str) -> bool {
        state == COMPLETED || state == FAILED
    }
}

/// 导入状态（宿主自己的流程，与 provider 无关）。
///
/// # 五个字面量都是**库契约**，不是内部实现
///
/// 上游 `src/common/media_import_status.py:18-22` 显式列出五个值，而 DDL
/// 里是**无 CHECK 约束**的 `varchar(32) NOT NULL DEFAULT 'pending'` ——
/// 数据库不会拦住写错的字面量，所以它们的一致性全靠代码。
///
/// | 值 | 含义 |
/// |---|---|
/// | `pending` | 下载已完成，等待自动导入触发 |
/// | `running` | 导入作业正在执行 |
/// | `completed` | 符合条件的媒体文件已入库 |
/// | `failed` | 存在未成功导入的文件 |
/// | `skipped` | 没有符合条件的媒体文件 |
///
/// # `COMPLETED` 的字面量是 `completed` 而不是 `done`
///
/// 本仓库此前写的是 `DONE = "done"`。那不是命名风格问题，是**会让每一个
/// 导入成功的下载被报成导入失败**：上游
/// `StatusService._download_task_bucket`（`status_service.py:320`）按
/// `import_status == "completed"` 判定 `imported` 桶，`"done"` 既不等于
/// `completed` 也不等于 `skipped`、也不在 `UNFINISHED_IMPORT_STATUSES` 里，
/// 于是落到 `else` 分支 —— `import_failed`。
///
/// 而 `/status/insights` 的六个桶是给用户看「有多少条导入失败了」的。
/// 一个字面量的错字会让那个数字等于「导入成功数」，且**没有任何报错**。
///
/// 这与 `background_task_run.state` 的 `succeeded` → `completed` 是同一类
/// 缺陷（同一个仓库犯过两次），所以这里把五个字面量连同来源一起写全。
pub mod import_status {
    /// 初始态：尚未开始。
    pub const PENDING: &str = "pending";
    /// 导入中（stage / finalize）。
    pub const RUNNING: &str = "running";
    /// 导入成功，`completed_source_ref` 已写入。
    pub const COMPLETED: &str = "completed";
    /// 终态：导入失败。
    pub const FAILED: &str = "failed";
    /// 终态：没有符合条件的媒体文件，**不是失败**。
    ///
    /// 「这一趟下载里没有可导入的文件」是正常结果，不是错误 —— 用户看到它
    /// 归在「导入失败」里会以为出了问题。客户端的六分类里它单独是
    /// `skipped` 一档。
    pub const SKIPPED: &str = "skipped";

    /// 全部合法取值。`set_import_status` 按它校验。
    pub const ALL: [&str; 5] = [PENDING, RUNNING, COMPLETED, FAILED, SKIPPED];

    /// 导入「还在途」的两个取值。
    ///
    /// 上游 `UNFINISHED_IMPORT_STATUSES`（`media_import_status.py:33`）：
    /// `pending`（等自动导入排队）与 `running`（作业正在跑）。其余三个都表示
    /// 这一趟已经跑完、不会再自动推进。
    pub const UNFINISHED: [&str; 2] = [PENDING, RUNNING];

    pub fn is_valid(status: &str) -> bool {
        ALL.contains(&status)
    }

    /// 是否已到终态。**包括 `skipped`** —— 它不会再自动推进。
    pub fn is_terminal(status: &str) -> bool {
        !UNFINISHED.contains(&status) && is_valid(status)
    }
}

/// 失败条目的分类（上游 `FAILED_FILE_KIND_*`，`common/media_import_status.py:36-40`）。
///
/// # 它决定客户端**能对这一条做什么**
///
/// | kind | 含义 | 客户端可做的操作 |
/// |---|---|---|
/// | `file` | 单个媒体文件级失败 | 重导 / 删除 / 重命名 |
/// | `skipped` | 主动跳过（小文件、格式不支持、已索引）| 仅信息展示 |
/// | `warning` | 导入后告警（删源失败、多字幕）| 不做文件级操作 |
/// | `job` | 任务级失败（`path` 通常是目录）| 不做文件级操作 |
///
/// # 分类由**写入侧**算好，读取侧直接读
///
/// `failed_files[].kind` 是**存进库里**的值（`make_failure_item` 是唯一来源，
/// `common/media_import_status.py:78-85`），而 `import_task` 读失败项时是
/// `item["kind"]` 原样投影（`shared/import_task_service.py:524`）。
///
/// **不要**在读侧重算：存量数据是用当时那张表算出来的，重算会让「改一次分类表」
/// 与历史行不一致 —— 而那表现为「同一条失败项，列表里的分类与它能不能被删
/// 对不上」，最难查的一类。
///
/// [`classify`](crate::transfers::downloads::failed_file_kind::classify)
/// 只给写入侧用（`import_service` 还没落地）。
pub mod failed_file_kind {
    /// 单个媒体文件级失败 —— **未知原因也落这一档**。
    pub const FILE: &str = "file";
    /// 主动跳过。
    pub const SKIPPED: &str = "skipped";
    /// 导入后告警。
    pub const WARNING: &str = "warning";
    /// 任务级失败。
    pub const JOB: &str = "job";

    /// 全部取值。
    pub const ALL: [&str; 4] = [FILE, SKIPPED, WARNING, JOB];

    /// 按失败原因归类。对应上游 `classify_failed_file_kind`（`:73-75`）。
    ///
    /// 未知原因落 [`FILE`]（上游是 `_FAILED_FILE_KIND_BY_REASON.get(reason, FILE)`）
    /// —— 未知原因多半是**新加的、还没归类**的文件级失败，当成「不可操作」
    /// 会让用户连重导都点不到。
    pub fn classify(reason: &str) -> &'static str {
        use super::failure_reason as r;
        match reason {
            r::MOVIE_NUMBER_NOT_FOUND
            | r::METADATA_FETCH_FAILED
            | r::IMAGE_DOWNLOAD_FAILED
            | r::METADATA_UPSERT_FAILED
            | r::MEDIA_IMPORT_FAILED => FILE,
            r::FILE_TOO_SMALL | r::UNSUPPORTED_FORMAT | r::ALREADY_INDEXED_PATH => SKIPPED,
            r::SOURCE_DELETE_FAILED => WARNING,
            r::NO_MEDIA_FILES_FOUND => JOB,
            // 两个**已废弃的历史 reason** 不能删：merge_subtitle 的老告警行对应的
            // 媒体**已经入库**，一旦掉到默认的 `file`，前端会把它当成可删除项，
            // 误操作会删掉已入库媒体的源文件。上游为此在映射表里显式写死了这两条
            // （`media_import_status.py:64-67` 的注释）。
            "multi_part_merge_failed" => FILE,
            "merge_subtitle_skipped_multiple_sidecars" => WARNING,
            _ => FILE,
        }
    }
}

/// `failed_files[].reason`：单条导入失败的具体原因
/// （上游 `media_import_status.py:42-52`，**十**个取值）。
///
/// # 为什么单独一个模块
///
/// 这些字符串会**落进 `background_task_run.result_summary` 的 JSON**，
/// 也被两类逻辑当判据读：
///
/// - 失败项是否**用户可修**（只有
///   [`movie_number_not_found`](crate::transfers::downloads::failure_reason::MOVIE_NUMBER_NOT_FOUND)
///   与
///   [`metadata_fetch_failed`](crate::transfers::downloads::failure_reason::METADATA_FETCH_FAILED)）——
///   见 `sm_service::transfers::import_task::ImportTaskService::MANUAL_SEARCH_FAILURE_REASONS`；
/// - 归类成哪个 [`failed_file_kind`]。
///
/// ⚠️ 骨架期 `sm-service` 另有一份**自造**的六项集合（`is_collection` /
/// `unsafe_filename` / `stage_failed` / `finalize_failed` 四个上游**没有**，
/// 且少四个真的）。已删，全仓只留这一份。
pub mod failure_reason {
    /// 番号识别不出来。
    pub const MOVIE_NUMBER_NOT_FOUND: &str = "movie_number_not_found";
    /// 元数据抓取失败。
    pub const METADATA_FETCH_FAILED: &str = "metadata_fetch_failed";
    /// 封面等图片下载失败。
    pub const IMAGE_DOWNLOAD_FAILED: &str = "image_download_failed";
    /// 元数据写库失败。
    pub const METADATA_UPSERT_FAILED: &str = "metadata_upsert_failed";
    /// 媒体入库失败（默认档）。
    pub const MEDIA_IMPORT_FAILED: &str = "media_import_failed";
    /// 文件太小，主动跳过。
    pub const FILE_TOO_SMALL: &str = "file_too_small";
    /// 格式不支持，主动跳过。
    pub const UNSUPPORTED_FORMAT: &str = "unsupported_format";
    /// 删源失败（`delete_after_commit` 之后）。
    pub const SOURCE_DELETE_FAILED: &str = "source_delete_failed";
    /// 这一趟没找到可导入的媒体文件（任务级）。
    pub const NO_MEDIA_FILES_FOUND: &str = "no_media_files_found";
    /// 这个路径已经索引过。
    pub const ALREADY_INDEXED_PATH: &str = "already_indexed_path";
}

/// `download_task` 表：一次下载任务。
///
/// 唯一索引 `(client, remote_id)` —— 同一下载器内的远端任务 id 唯一，
/// 这是幂等提交的基础：重复提交同一资源会命中该约束而非产生第二条记录。
#[derive(Debug, Clone, FromRow)]
pub struct DownloadTask {
    pub id: i32,
    pub client_id: i32,
    /// 影片番号（**字符串，非外键**）。
    ///
    /// 注释：「影片番号不是 provider 身份，只是宿主业务投影，
    /// 允许任务早于影片入库」。所以这里不能建成指向 `Movie` 的外键 ——
    /// 搜索结果先于刮削入库是正常流程。
    pub movie_number: Option<String>,
    /// 下载器侧的远端任务 id。
    pub remote_id: String,
    pub name: String,
    /// provider 的远端状态，默认 `queued`。
    pub state: String,
    /// 0.0 – 1.0，由 provider 汇报。
    ///
    /// 必须是 `f64` 而非 `f32`：DDL 里是 `double precision`（float8），
    /// 而 sqlx 的 `f32: Decode` 走 `decode_float4`，读 float8 列会报类型
    /// 不匹配。对拍脚本曾把 f32 一并映射成 float8，所以这个错误能通过
    /// L1、只在集成测试才暴露。
    pub progress: f64,
    /// 结构由**同 bundle 的 storage provider** 定义，不是 client 的。
    pub completed_source_ref: Option<String>,
    /// 宿主导入状态，默认 `pending`。
    pub import_status: String,
    /// 关联的后台任务台账。删台账记录只置空，不影响下载任务。
    pub import_task_run_id: Option<i32>,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl DownloadTask {
    /// 下载侧是否已进入终态。
    pub fn download_finished(&self) -> bool {
        download_state::is_terminal(&self.state)
    }

    /// 导入侧是否已进入终态。
    pub fn import_finished(&self) -> bool {
        import_status::is_terminal(&self.import_status)
    }

    /// 整条链路是否走完 —— 两侧都到终态。
    pub fn fully_settled(&self) -> bool {
        self.download_finished() && self.import_finished()
    }

    /// 下载已完成但导入失败 —— 最常见的「卡住」形态，需要单独的告警口径。
    ///
    /// 只有把两个状态机分开建模，才能表达这个组合。
    pub fn is_stuck_after_download(&self) -> bool {
        self.state == download_state::COMPLETED && self.import_status == import_status::FAILED
    }
}

/// 提交前的资源黑名单。
///
/// 40 字符定长 = BT **v1** info hash 的 hex 长度，与 `svc-hash` 的
/// `canonical_info_hash` 契约一致。v2-only 种子在上游就被拒，
/// 所以这里不需要区分 hash 版本。
#[derive(Debug, Clone, FromRow)]
pub struct DownloadResourceBlacklist {
    pub id: i32,
    /// 全局唯一。v1 info hash，小写 hex。
    pub info_hash: String,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

/// 校验是否为合法的 40 位 hex（v1 info hash）。
///
/// **模块级而非挂在某张表上**：`info_hash` 的规范化规则被
/// `download_resource_blacklist` 与 `download_submission_record` 共用 ——
/// 后者要与前者比对才能知道「这个资源是不是被拉黑过」。挂在其中一张表
/// 上会让另一张表的调用方写出一句读不通的话
/// （`Blacklist::is_valid_info_hash(某次提交的 hash)`）。
pub fn is_valid_info_hash(hash: &str) -> bool {
    hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

impl DownloadResourceBlacklist {
    /// 校验是否为合法的 40 位 hex。
    pub fn is_valid_info_hash(hash: &str) -> bool {
        is_valid_info_hash(hash)
    }
}

/// `download_submission_record` 表：提交历史。
///
/// **`client_id` / `task_id` 是裸整数，不是外键。** 这是刻意的：
/// 提交历史必须独立于下载任务与下载器存活，否则清理下载器时
/// 历史会连带消失，而审计与排查恰恰依赖这些记录。
///
/// 生命周期因此是**不可逆的单向引用** —— 任务删除后 `task_id` 变成
/// 悬空整数，Rust 侧用 `Option<i64>` 表达，不能假设它指向存在的行。
#[derive(Debug, Clone, FromRow)]
pub struct DownloadSubmissionRecord {
    pub id: i32,
    /// 裸整数，无外键约束。
    pub client_id: i32,
    /// 裸整数，无外键约束。任务删除后成为悬空引用。
    pub task_id: Option<i32>,
    pub movie_number: String,
    pub indexer_name: String,
    pub title: String,
    /// 磁力链接或种子文件地址。
    pub source_uri: String,
    /// 40 位 v1 info hash，小写 hex。与黑名单表同一套规范化规则。
    pub info_hash: String,
    /// 默认 `submitting`。
    pub state: String,
    /// 提交成功后由下载器返回；未成功时为空。
    pub remote_id: Option<String>,
    pub error_code: Option<String>,
    pub created_at: Option<NaiveDateTime>,
    pub updated_at: Option<NaiveDateTime>,
}

impl DownloadSubmissionRecord {
    /// 提交是否已成功拿到远端任务 id。
    pub fn succeeded(&self) -> bool {
        self.remote_id.is_some() && self.error_code.is_none()
    }

    /// 该记录是否已失去其任务引用（任务被删）。
    ///
    /// 这类记录**必须保留** —— 它是「曾经提交过」的唯一证据。
    pub fn is_orphaned(&self) -> bool {
        self.task_id.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task(state: &str, import: &str) -> DownloadTask {
        DownloadTask {
            id: 1,
            client_id: 1,
            movie_number: None,
            remote_id: "r".to_owned(),
            name: "n".to_owned(),
            state: state.to_owned(),
            progress: 0.0,
            completed_source_ref: None,
            import_status: import.to_owned(),
            import_task_run_id: None,
            created_at: None,
            updated_at: None,
        }
    }

    fn indexer(kind: &str, key: Option<&str>) -> Indexer {
        Indexer {
            id: 1,
            name: "n".to_owned(),
            url: "https://example.com".to_owned(),
            kind: kind.to_owned(),
            api_key: key.map(str::to_owned),
            created_at: None,
            updated_at: None,
        }
    }

    #[test]
    fn indexer_apikey_presence_is_protocol_level() {
        // Torznab 允许无鉴权索引器，此时请求不能带 apikey 参数。
        assert!(!indexer("bt", None).requires_apikey());
        assert!(
            !indexer("bt", Some("   ")).requires_apikey(),
            "空白视同无 key"
        );
        assert!(indexer("bt", Some("k")).requires_apikey());
    }

    #[test]
    fn indexer_kinds_are_torznab_categories() {
        assert!(indexer_kind::is_valid("pt"));
        assert!(indexer_kind::is_valid("bt"));
        assert!(!indexer_kind::is_valid("torznab"));
    }

    /// 五个字面量逐条钉住 —— `import_status` 是无 CHECK 约束的
    /// `varchar(32)`，字面量的一致性全靠代码。
    ///
    /// 这条测试存在的原因：本仓库犯过两次同类错误（`task_state` 的
    /// `succeeded`→`completed`，`import_status` 的 `done`→`completed`），
    /// 两次都是「改了常量名、没改字面量」或反过来。
    #[test]
    fn import_status_literals_match_upstream_verbatim() {
        // 上游 src/common/media_import_status.py:18-22
        assert_eq!(import_status::PENDING, "pending");
        assert_eq!(import_status::RUNNING, "running");
        assert_eq!(import_status::COMPLETED, "completed");
        assert_eq!(import_status::FAILED, "failed");
        assert_eq!(import_status::SKIPPED, "skipped");

        // 本仓库曾写成 "done"，而上游按 "completed" 判桶
        assert_ne!(
            import_status::COMPLETED,
            "done",
            "字面量必须是 completed：上游 _download_task_bucket 按它判 imported 桶"
        );
        // 五个都在白名单里
        for status in import_status::ALL {
            assert!(import_status::is_valid(status));
        }
        // 且不多不少
        assert_eq!(import_status::ALL.len(), 5);
    }

    /// `UNFINISHED` 恰好是「还在途」的两个 —— 上游用它决定要不要自动推进。
    #[test]
    fn unfinished_is_exactly_the_two_in_flight_values() {
        assert_eq!(import_status::UNFINISHED, ["pending", "running"]);
        for status in import_status::UNFINISHED {
            assert!(!import_status::is_terminal(status), "{status} 在途");
        }
        for status in [
            import_status::COMPLETED,
            import_status::FAILED,
            import_status::SKIPPED,
        ] {
            assert!(import_status::is_terminal(status), "{status} 是终态");
        }
    }

    /// 未知字面量既不合法、也不算终态 —— 不能让它「看起来已完成」。
    #[test]
    fn an_unknown_status_is_neither_valid_nor_terminal() {
        for bogus in ["done", "succeeded", "imported", "", "COMPLETED"] {
            assert!(!import_status::is_valid(bogus), "{bogus:?} 不该合法");
            assert!(!import_status::is_terminal(bogus), "{bogus:?} 不该算终态");
        }
    }

    #[test]
    fn two_state_machines_have_disjoint_terminal_sets() {
        // 这是不能把 state 与 import_status 合并成一个 status 列的原因。
        assert!(download_state::is_terminal(download_state::COMPLETED));
        assert!(download_state::is_terminal(download_state::FAILED));
        assert!(!download_state::is_terminal(download_state::DOWNLOADING));

        assert!(import_status::is_terminal(import_status::COMPLETED));
        assert!(import_status::is_terminal(import_status::FAILED));
        assert!(
            import_status::is_terminal(import_status::SKIPPED),
            "skipped 是终态 —— 这一趟不会再自动推进"
        );
        assert!(!import_status::is_terminal(import_status::RUNNING));
    }

    #[test]
    fn detects_download_done_but_import_failed() {
        let stuck = task(download_state::COMPLETED, import_status::FAILED);
        assert!(stuck.download_finished());
        assert!(stuck.import_finished());
        assert!(stuck.fully_settled());
        assert!(
            stuck.is_stuck_after_download(),
            "下载成功但导入失败是最常见的卡住形态，需要单独告警口径"
        );

        assert!(
            !task(download_state::COMPLETED, import_status::COMPLETED).is_stuck_after_download()
        );
        assert!(!task(download_state::DOWNLOADING, import_status::PENDING).fully_settled());
    }

    #[test]
    fn movie_number_is_projection_not_identity() {
        // 任务可以早于影片入库，所以 movie_number 允许为空且不是外键。
        let mut t = task(download_state::QUEUED, import_status::PENDING);
        assert!(t.movie_number.is_none());
        t.movie_number = Some("ABC-001".to_owned());
        assert_eq!(t.movie_number.as_deref(), Some("ABC-001"));
    }

    #[test]
    fn blacklist_info_hash_is_v1_hex() {
        assert!(DownloadResourceBlacklist::is_valid_info_hash(
            "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c"
        ));
        assert!(!DownloadResourceBlacklist::is_valid_info_hash("abc"));
        assert!(!DownloadResourceBlacklist::is_valid_info_hash(
            &"z".repeat(40)
        ));
        assert!(
            !DownloadResourceBlacklist::is_valid_info_hash(&"a".repeat(64)),
            "v2 是 64 位 hex，本表存不下 —— svc-hash 上游已提前拒绝 v2-only"
        );
    }

    #[test]
    fn submission_record_outlives_its_task() {
        let mut rec = DownloadSubmissionRecord {
            id: 1,
            client_id: 7,
            task_id: Some(42),
            movie_number: "ABC-001".to_owned(),
            indexer_name: "idx".to_owned(),
            title: "t".to_owned(),
            source_uri: "magnet:?xt=urn:btih:dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c".to_owned(),
            info_hash: "dd8255ecdc7ca55fb0bbf81323d87062db1f6d1c".to_owned(),
            state: "submitting".to_owned(),
            remote_id: None,
            error_code: None,
            created_at: None,
            updated_at: None,
        };

        assert!(!rec.succeeded(), "还没拿到 remote_id");
        assert!(!rec.is_orphaned());

        rec.remote_id = Some("remote-1".to_owned());
        assert!(rec.succeeded());

        rec.error_code = Some("invalid_download_torrent".to_owned());
        assert!(!rec.succeeded(), "有错误码就不算成功");

        rec.error_code = None;
        rec.task_id = None;
        assert!(
            rec.is_orphaned(),
            "任务删除后记录仍在 —— 这正是无外键设计的目的"
        );
        assert!(rec.succeeded(), "孤立记录仍保留提交成功的事实");
    }

    #[test]
    fn client_config_tolerates_blank_and_invalid_json() {
        let make = |raw: Option<&str>| DownloadClient {
            id: 1,
            name: "n".to_owned(),
            provider_config: raw.map(str::to_owned),
            library_id: 1,
            created_at: None,
            updated_at: None,
        };
        assert!(make(Some("  ")).parsed_config().is_none());
        assert!(make(Some("nonsense")).parsed_config().is_none());
        assert!(make(None).parsed_config().is_none());
        let parsed = make(Some(r#"{"host":"h"}"#)).parsed_config().unwrap();
        assert_eq!(parsed["host"], "h");
    }

    // ------------------------------------------------ 失败项的 reason 与 kind

    /// 十个 reason 字面量**逐个钉死**。
    ///
    /// 它们会落进 `background_task_run.result_summary` 的 JSON。改名不会报错，
    /// 只表现为：存量失败项在新代码里读不出「原因」，而 `kind` 也会跟着错档
    /// （未知原因落 `file`，于是「小文件」被当成「可删除的文件级失败」）。
    #[test]
    fn the_failure_reasons_are_upstreams_ten_literals() {
        use failure_reason as r;
        assert_eq!(r::MOVIE_NUMBER_NOT_FOUND, "movie_number_not_found");
        assert_eq!(r::METADATA_FETCH_FAILED, "metadata_fetch_failed");
        assert_eq!(r::IMAGE_DOWNLOAD_FAILED, "image_download_failed");
        assert_eq!(r::METADATA_UPSERT_FAILED, "metadata_upsert_failed");
        assert_eq!(r::MEDIA_IMPORT_FAILED, "media_import_failed");
        assert_eq!(r::FILE_TOO_SMALL, "file_too_small");
        assert_eq!(r::UNSUPPORTED_FORMAT, "unsupported_format");
        assert_eq!(r::SOURCE_DELETE_FAILED, "source_delete_failed");
        assert_eq!(r::NO_MEDIA_FILES_FOUND, "no_media_files_found");
        assert_eq!(r::ALREADY_INDEXED_PATH, "already_indexed_path");
    }

    /// 分类表的**每一条**分支都要对上上游（含两个已废弃的历史 reason）。
    #[test]
    fn the_failed_file_kind_classification_matches_upstream() {
        use failed_file_kind as k;
        use failure_reason as r;

        for reason in [
            r::MOVIE_NUMBER_NOT_FOUND,
            r::METADATA_FETCH_FAILED,
            r::IMAGE_DOWNLOAD_FAILED,
            r::METADATA_UPSERT_FAILED,
            r::MEDIA_IMPORT_FAILED,
        ] {
            assert_eq!(k::classify(reason), k::FILE, "{reason} 是文件级失败");
        }
        for reason in [
            r::FILE_TOO_SMALL,
            r::UNSUPPORTED_FORMAT,
            r::ALREADY_INDEXED_PATH,
        ] {
            assert_eq!(k::classify(reason), k::SKIPPED, "{reason} 是主动跳过");
        }
        assert_eq!(k::classify(r::SOURCE_DELETE_FAILED), k::WARNING);
        assert_eq!(k::classify(r::NO_MEDIA_FILES_FOUND), k::JOB);

        // 两条历史 reason 必须在表里**显式**锁死：掉到默认的 `file` 会让前端
        // 把它们当可删除项，误删已入库媒体的源文件。
        assert_eq!(k::classify("multi_part_merge_failed"), k::FILE);
        assert_eq!(
            k::classify("merge_subtitle_skipped_multiple_sidecars"),
            k::WARNING
        );
    }

    /// 未知原因落 `file`（上游 `dict.get(..., FILE)`）——
    /// 那多半是**新加的、还没归类**的文件级失败，判成「不可操作」会让用户
    /// 连重导都点不到。
    #[test]
    fn unknown_reasons_are_treated_as_actionable_file_failures() {
        assert_eq!(
            failed_file_kind::classify("brand-new-reason"),
            failed_file_kind::FILE
        );
        assert_eq!(failed_file_kind::classify(""), failed_file_kind::FILE);
    }

    /// 分类只产出那四个合法取值 —— 客户端按它决定显示哪些操作。
    #[test]
    fn every_classification_lands_in_the_closed_kind_set() {
        use failure_reason as r;
        let reasons = [
            r::MOVIE_NUMBER_NOT_FOUND,
            r::METADATA_FETCH_FAILED,
            r::IMAGE_DOWNLOAD_FAILED,
            r::METADATA_UPSERT_FAILED,
            r::MEDIA_IMPORT_FAILED,
            r::FILE_TOO_SMALL,
            r::UNSUPPORTED_FORMAT,
            r::SOURCE_DELETE_FAILED,
            r::NO_MEDIA_FILES_FOUND,
            r::ALREADY_INDEXED_PATH,
            "multi_part_merge_failed",
            "merge_subtitle_skipped_multiple_sidecars",
        ];
        for reason in reasons {
            assert!(
                failed_file_kind::ALL.contains(&failed_file_kind::classify(reason)),
                "{reason} 归类出了闭集"
            );
        }
    }
}
