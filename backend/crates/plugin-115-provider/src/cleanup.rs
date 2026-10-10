//! 手动清理任务（上游 `cleanup.py` 的 Rust 子集）。
//!
//! # 两个任务，一个实现了一个没有
//!
//! | 上游任务 | 状态 | 原因 |
//! |---|---|---|
//! | `sakuramedia_115_cleanup_empty_media_dirs` | **已实现** | 只依赖 115 API（媒体库配置 + 目录树 + 删除），当前 ABI 就能完整表达 |
//! | `sakuramedia_115_cleanup_imported_downloads` | **未声明** | 判据是「已入库媒体的 SHA1 索引」（上游 `_load_imported_media_groups` 直读 media 表），而 `PluginHost::ListMedia` 只按 movie_id 查、`MediaSnapshot` 不带 storage_ref/sha1 —— 没有这份数据就无法证明「目录里的东西都已入库」，删除是不安全的 |
//!
//! 为什么不在 `register` 里声明那个做不了的任务：一个永远失败的按钮只会把
//! 「宿主缺数据」伪装成「插件坏了」。等 ABI 把「按库扫媒体（带存储引用）」
//! 补上再接（`host.proto` 的 `ListMediaRequest` 需要一个非按影片的扫描模式 +
//! `MediaSnapshot` 需要存储引用字段）。
//!
//! # 与上游的实现差异（判定语义一致）
//!
//! - 上游读目录树用「子树简表」接口（`/files/downfolders` + `/files/downfiles`，
//!   按 pickcode 一次拉全树）；本实现用 `/files` 逐层递归扫 —— 请求多几轮，
//!   树的形状一致。
//! - 复核用 [`Cloud115Client::list_files_recursive`]（服务端递归、**只有文件**）：
//!   与上游同一条判据 —— 子目录不算「有东西」，删除本来就是递归的。
//! - 上游逐**媒体库**循环（一份配置一个库）；本仓的库配置在插件 settings 里
//!   （单账号单根目录），所以只有一轮。媒体库配置不完整时上游记
//!   `failed_libraries` 继续，这里只有这一个库，直接让任务失败。
//! - 中途失败时已完成的部分**不回滚**（115 没有批量恢复）；任务是幂等的 ——
//!   重跑会重新扫描，剩下该删的继续删。
//!
//! # 破坏性确认
//!
//! 上游 `CleanupConfirmParams { confirm: Literal[True] }`：缺、假、非布尔都
//! 拒（[`extract_confirm_params`]）。表单形状在 [`confirm_params_schema`]。

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::time::Instant;

use prost_types::Struct;
use sm_plugin_api::v1::job_event::Event as JobEventKind;
use sm_plugin_api::v1::{JobEvent, ProgressEvent, RunJobRequest};
use tokio::sync::mpsc;
use tonic::Status;

use crate::client::{Cloud115Client, Cloud115Error};
use crate::config::Plugin115Config;

/// 上游 `DELETE_BATCH_SIZE`：一次删除请求最多带多少个 id。
pub const DELETE_BATCH_SIZE: usize = 1000;

/// 手动任务键（上游 `plugin.py` 的 `JobDefinition.task_key`）。
pub const CLEANUP_EMPTY_DIRS_TASK: &str = "sakuramedia_115_cleanup_empty_media_dirs";

/// 目录树里的一个目录（扫描阶段的产物）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirNode {
    /// 目录 id。
    pub cid: String,
    /// 父目录 id。
    pub parent_cid: String,
    /// 目录名（进度与日志用）。
    pub name: String,
}

/// 破坏性确认位（上游 `CleanupConfirmParams` 的直译）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CleanupConfirmParams {
    pub confirm: bool,
}

/// 从 `RunJobRequest.params` 取确认位。`confirm` **必须**是布尔 `true`：
/// 缺、假、类型不对（`"true"`）都拒 —— 静默放过任何一种都会让「点错按钮」
/// 变成「删了不该删的」。
pub fn extract_confirm_params(request: &RunJobRequest) -> Result<CleanupConfirmParams, Status> {
    let confirmed = request
        .params
        .as_ref()
        .and_then(|params| params.fields.get("confirm"))
        .is_some_and(|value| {
            matches!(value.kind.as_ref(), Some(prost_types::value::Kind::BoolValue(true)))
        });
    if !confirmed {
        return Err(Status::invalid_argument(
            "清理是破坏性操作：需要参数 confirm = true（布尔）才会执行",
        ));
    }
    Ok(CleanupConfirmParams { confirm: true })
}

/// 任务参数表单（JSON Schema）：一个必须显式勾上的布尔。
pub fn confirm_params_schema() -> Struct {
    use prost_types::value::Kind;
    use prost_types::{ListValue, Value};
    let confirm = Struct {
        fields: std::collections::BTreeMap::from([
            (
                "type".to_owned(),
                Value {
                    kind: Some(Kind::StringValue("boolean".to_owned())),
                },
            ),
            (
                "const".to_owned(),
                Value {
                    kind: Some(Kind::BoolValue(true)),
                },
            ),
            (
                "title".to_owned(),
                Value {
                    kind: Some(Kind::StringValue("我确认要删除这些空目录".to_owned())),
                },
            ),
        ]),
    };
    Struct {
        fields: std::collections::BTreeMap::from([
            (
                "type".to_owned(),
                Value {
                    kind: Some(Kind::StringValue("object".to_owned())),
                },
            ),
            (
                "properties".to_owned(),
                Value {
                    kind: Some(Kind::StructValue(Struct {
                        fields: std::collections::BTreeMap::from([(
                            "confirm".to_owned(),
                            Value {
                                kind: Some(Kind::StructValue(confirm)),
                            },
                        )]),
                    })),
                },
            ),
            (
                "required".to_owned(),
                Value {
                    kind: Some(Kind::ListValue(ListValue {
                        values: vec![Value {
                            kind: Some(Kind::StringValue("confirm".to_owned())),
                        }],
                    })),
                },
            ),
        ]),
    }
}

/// **空目录候选**：没被任何文件「占住」、而父目录被占住的目录，按父目录分组。
///
/// 上游 `_find_empty_directories`（`cleanup.py:437`）的纯函数化 —— 网络（扫描）
/// 与判定分离，判定用单测钉死：
///
/// 1. 根目录与**有文件的目录**及其全部祖先都算「被占住」（一个目录里只要
///    深处有文件，它和它的每一层祖先都删不得）；
/// 2. 没被占住、父目录被占住 → 候选（删除请求挂在父目录下发）。
///
/// 与上游同样的**单遍**语义：候选的子目录不在同一轮里（但删除是递归的，
/// 它们跟着父目录一起走）。
///
/// 输出按 `(父目录, cid)` 升序 —— 删除顺序确定，测试与日志都可复现。
pub fn empty_dir_candidates(
    root_cid: &str,
    directories: &[DirNode],
    parents_with_files: &BTreeSet<String>,
) -> BTreeMap<String, Vec<String>> {
    let by_cid: HashMap<&str, &DirNode> = directories
        .iter()
        .map(|node| (node.cid.as_str(), node))
        .collect();
    let mut occupied: BTreeSet<String> = BTreeSet::from([root_cid.to_owned()]);
    // 从每个「有文件的目录」一路爬到根：沿途全占（含它自己）。
    for parent in parents_with_files {
        let mut current = parent.clone();
        while !occupied.contains(&current) {
            occupied.insert(current.clone());
            match by_cid.get(current.as_str()) {
                Some(node) => current = node.parent_cid.clone(),
                // 树是从真实列表里长出来的，不该有断链；断了的分支当根处理。
                None => break,
            }
        }
    }
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for node in directories {
        if !occupied.contains(&node.cid) && occupied.contains(&node.parent_cid) {
            out.entry(node.parent_cid.clone()).or_default().push(node.cid.clone());
        }
    }
    for ids in out.values_mut() {
        ids.sort();
    }
    out
}

fn client_status(error: Cloud115Error) -> Status {
    match error {
        // Cookie 失效 / 目录不存在是**配置问题**：调用方修好配置再跑。
        Cloud115Error::Auth(message) | Cloud115Error::NotFound(message) => {
            Status::failed_precondition(message)
        }
        other => Status::unavailable(other.to_string()),
    }
}

/// 终态摘要帧。`control` 在任务主流程成功后调用。
pub fn result_event(value: serde_json::Value) -> JobEvent {
    JobEvent {
        event: Some(JobEventKind::Result(
            sm_plugin_api::json_struct::json_to_struct(&value).unwrap_or_default(),
        )),
    }
}

fn progress_event(text: String, current: i32, total: i32) -> JobEvent {
    JobEvent {
        event: Some(JobEventKind::Progress(ProgressEvent {
            text,
            current,
            total,
        })),
    }
}

fn clamp(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

/// 任务主流程（上游 `_cleanup_empty_media_dirs` 的单库版）。
///
/// 结果字典的键与上游一致（`scanned_directories` / `candidate_directories` /
/// `deleted_directories` / `skipped_directories` / `libraries` /
/// `failed_libraries` / `elapsed_seconds`），调用方的摘要展示不用分叉。
pub async fn run_cleanup_empty_media_dirs(
    tx: &mpsc::Sender<Result<JobEvent, Status>>,
    config: &Plugin115Config,
) -> Result<serde_json::Value, Status> {
    let started = Instant::now();
    let report = |text: String, current: i32, total: i32| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(Ok(progress_event(text, current, total))).await;
        }
    };

    let cookie = config.cookie().ok_or_else(|| {
        Status::failed_precondition("未配置 115 Cookie（web_cookie / device_cookie 至少填一个）")
    })?;
    if config.media_root_path.trim().is_empty() {
        return Err(Status::failed_precondition(
            "未配置媒体根目录 media_root_path",
        ));
    }
    let client = Cloud115Client::new(cookie).map_err(client_status)?;
    report("解析媒体根目录".to_owned(), 0, 0).await;
    let root_cid = client
        .resolve_path(config.media_root_path.trim())
        .await
        .map_err(client_status)?;
    // 与上游 `_load_directory_tree` 同一条闸：账号根目录拒绝（那是整个账号，
    // 「清理空目录」的爆炸半径不该是它）。
    if root_cid == "0" {
        return Err(Status::invalid_argument(
            "115 账号根目录不支持空目录清理，请把 media_root_path 配到具体目录",
        ));
    }

    // 扫描：逐层 `list_directory`，目录进树、文件把父目录标记为「被占住」。
    report("扫描目录".to_owned(), 0, 0).await;
    let mut directories: Vec<DirNode> = Vec::new();
    let mut parents_with_files: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::from([root_cid.clone()]);
    while let Some(cid) = queue.pop_front() {
        for entry in client.list_directory(&cid).await.map_err(client_status)? {
            if entry.is_dir {
                queue.push_back(entry.id.clone());
                directories.push(DirNode {
                    cid: entry.id,
                    parent_cid: entry.parent_id,
                    name: entry.name,
                });
            } else {
                parents_with_files.insert(entry.parent_id);
            }
        }
    }
    report(
        format!("扫描完成，共 {} 个目录", directories.len()),
        clamp(directories.len()),
        clamp(directories.len()),
    )
    .await;

    let candidates = empty_dir_candidates(&root_cid, &directories, &parents_with_files);
    let total_candidates: usize = candidates.values().map(Vec::len).sum();
    let (mut deleted_directories, mut skipped_directories) = (0_usize, 0_usize);

    // 复核 + 删除：整棵子树复核无文件后才删，避免沿用扫描阶段的判空结果
    // （扫描到删除之间 115 里的东西可能变过 —— 与上游同一条防线）。
    report("复核清理".to_owned(), 0, clamp(total_candidates)).await;
    let mut processed = 0_usize;
    for (parent_cid, cids) in &candidates {
        for batch in cids.chunks(DELETE_BATCH_SIZE) {
            let mut empty: Vec<String> = Vec::new();
            for cid in batch {
                let has_files = !client
                    .list_files_recursive(cid)
                    .await
                    .map_err(client_status)?
                    .is_empty();
                if has_files {
                    skipped_directories += 1;
                } else {
                    empty.push(cid.clone());
                }
            }
            if !empty.is_empty() {
                client
                    .delete_files(&empty, Some(parent_cid))
                    .await
                    .map_err(client_status)?;
                deleted_directories += empty.len();
            }
            processed += batch.len();
            report(
                format!("已处理 {processed}/{total_candidates} 个候选目录"),
                clamp(processed),
                clamp(total_candidates),
            )
            .await;
        }
    }
    // 结果字典的键与上游一致，摘要展示不用分叉。
    Ok(serde_json::json!({
        "libraries": 1,
        "scanned_directories": directories.len(),
        "candidate_directories": total_candidates,
        "deleted_directories": deleted_directories,
        "skipped_directories": skipped_directories,
        "failed_libraries": 0,
        "elapsed_seconds": started.elapsed().as_secs(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(cid: &str, parent: &str) -> DirNode {
        DirNode {
            cid: cid.to_owned(),
            parent_cid: parent.to_owned(),
            name: cid.to_owned(),
        }
    }

    fn set(values: &[&str]) -> BTreeSet<String> {
        values.iter().map(|value| value.to_string()).collect()
    }

    /// ★ 树：
    ///
    /// ```text
    /// root
    /// ├── a/          └─ b/ ── file.mkv   （a 空但 b 里有文件）
    /// ├── c/          └─ d/               （c、d 全空）
    /// └── e/                              （e 空直挂根）
    /// ```
    ///
    /// a 的**祖先** b 有文件 → a、b 都占住，都删不得；c、e 是候选。d 的父
    /// c 自己也只是候选（没被占住），所以 d **不在本轮** —— 但删除是递归的，
    /// c 被删时 d 跟着走。
    #[test]
    fn ancestors_of_files_are_occupied_and_empty_branches_are_candidates() {
        let directories = vec![
            dir("a", "root"),
            dir("b", "a"),
            dir("c", "root"),
            dir("d", "c"),
            dir("e", "root"),
        ];
        let candidates = empty_dir_candidates("root", &directories, &set(&["b"]));
        assert_eq!(
            candidates,
            BTreeMap::from([("root".to_owned(), vec!["c".to_owned(), "e".to_owned()])])
        );
    }

    /// 一个目录深处有文件 → 它与**每一层**祖先都占住（爬到根），一层都不能少
    /// —— 少爬一层就会把「有文件的中转目录」标成候选。
    #[test]
    fn occupation_climbs_all_the_way_to_the_root() {
        let directories = vec![dir("a", "root"), dir("b", "a"), dir("c", "b")];
        let candidates = empty_dir_candidates("root", &directories, &set(&["c"]));
        assert!(candidates.is_empty(), "全树都被 c 的文件占住了");
    }

    /// 单遍语义：候选（c）的子目录（d）不在同一轮里 —— 但删除是递归的，
    /// d 跟着 c 一起走。这个测试钉住的是「不要在同一轮里重复删一遍」。
    #[test]
    fn children_of_candidates_wait_for_the_next_round() {
        let directories = vec![dir("c", "root"), dir("d", "c")];
        let candidates = empty_dir_candidates("root", &directories, &BTreeSet::new());
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates["root"], vec!["c".to_owned()]);
    }

    /// ★ 确认位：缺、假、字符串 "true" 都拒 —— 只有布尔 `true` 过。
    #[test]
    fn confirmation_requires_a_literal_true() {
        let base = RunJobRequest {
            run_id: "r".to_owned(),
            task_key: CLEANUP_EMPTY_DIRS_TASK.to_owned(),
            params: None,
            data_dir: String::new(),
        };
        let err = extract_confirm_params(&base).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);

        let with = |confirm: prost_types::Value| RunJobRequest {
            params: Some(Struct {
                fields: std::collections::BTreeMap::from([(
                    "confirm".to_owned(),
                    confirm,
                )]),
            }),
            ..base.clone()
        };
        use prost_types::value::Kind;
        use prost_types::Value;
        for value in [
            Value {
                kind: Some(Kind::BoolValue(false)),
            },
            Value {
                kind: Some(Kind::StringValue("true".to_owned())),
            },
            Value {
                kind: Some(Kind::NumberValue(1.0)),
            },
        ] {
            let err = extract_confirm_params(&with(value)).unwrap_err();
            assert_eq!(err.code(), tonic::Code::InvalidArgument);
        }
        assert!(
            extract_confirm_params(&with(Value {
                kind: Some(Kind::BoolValue(true)),
            }))
            .is_ok()
        );
    }

    /// 表单声明 `confirm` 必填且恒为 `true` —— 表单与运行时校验说的是同一件事。
    #[test]
    fn the_schema_declares_confirm_as_required_const_true() {
        let schema = sm_plugin_api::json_struct::struct_to_json(Some(&confirm_params_schema()));
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["properties"]["confirm"]["type"], "boolean");
        assert_eq!(schema["properties"]["confirm"]["const"], true);
        assert_eq!(schema["required"][0], "confirm");
    }
}
