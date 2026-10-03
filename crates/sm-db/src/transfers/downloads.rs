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
pub mod import_status {
    /// 初始态：尚未开始。
    pub const PENDING: &str = "pending";
    /// 导入中（stage / finalize）。
    pub const RUNNING: &str = "running";
    /// 导入成功，`completed_source_ref` 已写入。
    pub const DONE: &str = "done";
    /// 终态：导入失败。
    pub const FAILED: &str = "failed";

    pub fn is_terminal(status: &str) -> bool {
        status == DONE || status == FAILED
    }
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

impl DownloadResourceBlacklist {
    /// 校验是否为合法的 40 位 hex。
    pub fn is_valid_info_hash(hash: &str) -> bool {
        hash.len() == 40 && hash.bytes().all(|b| b.is_ascii_hexdigit())
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

    #[test]
    fn two_state_machines_have_disjoint_terminal_sets() {
        // 这是不能把 state 与 import_status 合并成一个 status 列的原因。
        assert!(download_state::is_terminal(download_state::COMPLETED));
        assert!(download_state::is_terminal(download_state::FAILED));
        assert!(!download_state::is_terminal(download_state::DOWNLOADING));

        assert!(import_status::is_terminal(import_status::DONE));
        assert!(import_status::is_terminal(import_status::FAILED));
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

        assert!(!task(download_state::COMPLETED, import_status::DONE).is_stuck_after_download());
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
}
