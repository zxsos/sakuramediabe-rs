//! 插件目录的**盘点**：扫 `<root_dir>` 列出已装的插件。
//!
//! 上游对应 `PluginManager.list_plugins` / `get_plugin`
//! （`manager.py:103-176`）里**只涉及文件系统**的那一半。剩下的一半 ——
//! 「有没有启用」「加载有没有失败」—— 要读配置与运行时状态，属于服务层
//! （`sm_service::system::plugins`），本模块不碰。
//!
//! # 三条容易搞错的规则
//!
//! **① 没有 `manifest.json` 的目录直接跳过，不是「列出来但出错」。**
//!
//! 上游 `_load_manifest` 在文件**不存在**时返回 `(None, None)`，而
//! `list_plugins` 对 `(None, None)` 是 `continue`。这一条很重要：插件根目录
//! 下常常还有 `.staging/` 之类的工作目录，而用户手工丢进去的杂物也会被扫到 ——
//! 把它们列成「加载失败的插件」会让插件页面上全是自己不认识的东西。
//!
//! **② 有 `manifest.json` 但内容坏掉时要列出来，且 `plugin_id` 取目录名。**
//!
//! 那是「装了但坏了」，用户需要看见它才能去修 —— 而这时 manifest 里读不到
//! `plugin_id`，只能用目录名兜底（上游同样处理）。
//!
//! **③ 以 `.` 开头的目录一律跳过。** `.staging` 就在插件根下，把它的内容
//! 当成插件列出来是最容易踩的一脚。

use std::path::{Path, PathBuf};

use crate::manifest::{PluginManifest, MANIFEST_FILENAME};

/// 扫到的一个插件目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedPlugin {
    /// 插件 id。清单读得出来就用清单里的，否则用目录名（见模块文档第 ② 条）。
    pub plugin_id: String,
    /// 目录名。与 `plugin_id` **可能不同** —— 那种安装是有问题的，
    /// 上游在 `validate_installation` 里据此报错。
    pub dir_name: String,
    /// 插件目录的绝对/相对原样路径。
    pub dir: PathBuf,
    /// 清单。`None` = 清单存在但读不出来（坏 JSON / 缺字段）。
    pub manifest: Option<PluginManifest>,
    /// 清单的问题描述。与 `manifest.is_none()` 同真同假。
    pub manifest_error: Option<String>,
}

impl ScannedPlugin {
    /// 这个插件的目录名与清单里的 `plugin_id` 是否一致。
    ///
    /// 上游 `validate_installation`（`manager.py:94-98`）把不一致当**安装错误**：
    /// 宿主按目录名算可执行文件路径（`<root>/<id>/<id>`）、按清单里的 id 注入
    /// `SAKURAMEDIA_PLUGIN_ID`，两者不同就会「找得到文件但注册不上」。
    pub fn id_matches_dir(&self) -> bool {
        self.plugin_id == self.dir_name
    }
}

/// 扫一遍插件根目录。**不递归、不建目录、不报错。**
///
/// 根目录不存在时返回空表（上游 `if not root_dir.is_dir(): return []`）——
/// 那是新装实例的正常状态，不是错误。
pub fn scan(root_dir: &Path) -> Vec<ScannedPlugin> {
    if !root_dir.is_dir() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(root_dir) else {
        // 读不了（权限）按「一个都没有」处理：插件页空着比 500 更有用，
        // 而真正的故障会从别的地方（安装、拉起）暴露出来。
        return Vec::new();
    };

    let mut found: Vec<ScannedPlugin> = entries
        .flatten()
        .filter_map(|entry| {
            let dir = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            // 规则 ③：`.staging` 之类的工作目录不是插件。
            if name.starts_with('.') || !dir.is_dir() {
                return None;
            }
            read_dir_entry(dir, name)
        })
        .collect();
    // 排序只为让 `GET /system/plugins` 的次序稳定 —— 上游 `sorted(iterdir())`
    // 也是这个目的（按目录名）。
    found.sort_by(|left, right| left.dir_name.cmp(&right.dir_name));
    found
}

/// 读**指定 id** 的那个插件目录，不扫全表。
///
/// 上游 `get_plugin(plugin_id)` 直接拼 `<root>/<plugin_id>`（`manager.py:146`），
/// 不遍历目录 —— 所以 `GET /system/plugins/{id}` 是 O(1) 而不是 O(已装数量)。
///
/// 返回 `None` = **目录里有清单才算装了**。目录不存在、或没有
/// `manifest.json`，都是「这个 id 没有安装」。
pub fn read_one(root_dir: &Path, plugin_id: &str) -> Option<ScannedPlugin> {
    let dir = root_dir.join(plugin_id);
    if !dir.is_dir() {
        return None;
    }
    read_dir_entry(dir, plugin_id.to_owned())
}

/// 读一个目录的清单。`None` = 连 `manifest.json` 都没有（见模块文档第 ① 条）。
fn read_dir_entry(dir: PathBuf, dir_name: String) -> Option<ScannedPlugin> {
    if !dir.join(MANIFEST_FILENAME).is_file() {
        return None;
    }
    match PluginManifest::read_from_dir(&dir) {
        Ok(manifest) => Some(ScannedPlugin {
            plugin_id: manifest.plugin_id.clone(),
            dir_name,
            dir,
            manifest: Some(manifest),
            manifest_error: None,
        }),
        // 规则 ②：坏了也要列出来，id 用目录名兜底。
        Err(problem) => Some(ScannedPlugin {
            plugin_id: dir_name.clone(),
            dir_name,
            dir,
            manifest: None,
            manifest_error: Some(problem.message()),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "sm-plugins-inventory-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    fn write_plugin(root: &Path, dir_name: &str, manifest: &str) {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(&dir).expect("建插件目录");
        std::fs::write(dir.join(MANIFEST_FILENAME), manifest).expect("写清单");
    }

    fn good_manifest(plugin_id: &str) -> String {
        serde_json::json!({
            "plugin_id": plugin_id,
            "display_name": "测试插件",
            "version": "1.0.0",
        })
        .to_string()
    }

    #[test]
    fn a_missing_root_directory_is_an_empty_list_not_an_error() {
        // 新装实例还没有插件目录 —— 那是正常状态。
        let root = temp_root("absent").join("nope");
        assert!(scan(&root).is_empty());
        assert!(read_one(&root, "local").is_none());
    }

    /// ★ 没有 `manifest.json` 的目录**不出现**在列表里（规则 ①）。
    #[test]
    fn directories_without_a_manifest_are_skipped_entirely() {
        let root = temp_root("no-manifest-dir");
        std::fs::create_dir_all(root.join("just-a-folder")).expect("建杂物目录");
        write_plugin(&root, "local", &good_manifest("local"));

        let found = scan(&root);
        assert_eq!(found.len(), 1, "只该看到 local");
        assert_eq!(found[0].plugin_id, "local");
    }

    /// ★ `.staging` 与其它点开头目录不是插件（规则 ③）。
    #[test]
    fn dot_directories_are_never_plugins() {
        let root = temp_root("dot-dirs");
        write_plugin(&root, ".staging", &good_manifest("ghost"));
        std::fs::create_dir_all(root.join(".staging/uploads")).expect("建上传目录");
        write_plugin(&root, "local", &good_manifest("local"));

        let found = scan(&root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].plugin_id, "local");
    }

    /// ★ 清单坏掉时**要**列出来，且 id 用目录名（规则 ②）。
    #[test]
    fn a_broken_manifest_is_listed_with_the_directory_name() {
        let root = temp_root("broken");
        write_plugin(&root, "local", "{ not json");

        let found = scan(&root);
        assert_eq!(found.len(), 1, "装了但坏了，用户需要看见它");
        assert_eq!(found[0].plugin_id, "local", "读不到清单就用目录名");
        assert!(found[0].manifest.is_none());
        assert!(found[0].manifest_error.is_some());
    }

    /// 目录名与清单 `plugin_id` 不一致时，两条都要留着（上游据此报安装错误）。
    #[test]
    fn a_directory_name_that_disagrees_with_the_manifest_is_visible() {
        let root = temp_root("mismatch");
        write_plugin(&root, "wrong_name", &good_manifest("local"));

        let found = scan(&root);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].plugin_id, "local", "以清单为准");
        assert_eq!(found[0].dir_name, "wrong_name");
        assert!(!found[0].id_matches_dir(), "不一致必须能看出来");
    }

    #[test]
    fn the_list_is_ordered_by_directory_name() {
        let root = temp_root("ordered");
        write_plugin(&root, "zeta", &good_manifest("zeta"));
        write_plugin(&root, "alpha", &good_manifest("alpha"));
        write_plugin(&root, "mid", &good_manifest("mid"));

        let ids: Vec<String> = scan(&root).into_iter().map(|p| p.plugin_id).collect();
        assert_eq!(ids, vec!["alpha", "mid", "zeta"]);
    }

    /// `read_one` 只读那一个目录，不扫全表。
    #[test]
    fn read_one_targets_a_single_directory() {
        let root = temp_root("one");
        write_plugin(&root, "local", &good_manifest("local"));
        write_plugin(&root, "other", &good_manifest("other"));

        let local = read_one(&root, "local").expect("应当找到");
        assert_eq!(local.plugin_id, "local");

        assert!(read_one(&root, "nope").is_none(), "不存在的 id");
        assert!(
            read_one(&root, ".staging").is_none(),
            "没有清单的目录 = 没装"
        );
    }

    /// 有目录但没有清单 → `read_one` 返回 `None`（= 没装），而不是一个空壳。
    #[test]
    fn read_one_needs_a_manifest_to_count_as_installed() {
        let root = temp_root("one-no-manifest");
        std::fs::create_dir_all(root.join("local")).expect("建目录");
        assert!(read_one(&root, "local").is_none());
    }
}
