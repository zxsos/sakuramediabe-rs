//! 插件包安装：完整性校验与安全解压。
//!
//! 上游对应 `src/plugins/installer.py`（157 行）。**只负责把 zip 变成「可发布
//! 的插件目录」**：校验 → 安全解压到 `<root>/.staging/<plugin_id>`。发布
//! （`staging → <root>/<plugin_id>`、保留 `data/`）与启停由 service 层的
//! 插件管理器承担 —— 上游也是这么切分的。
//!
//! # 包内形状（与上游唯一的实质差别）
//!
//! ```text
//! <zip 根>
//!   manifest.json    ← 必需，且在**根部**（上游 `_read_manifest_from_zip`
//!                      就是直接 `archive.read(MANIFEST_FILENAME)`）
//!   <plugin_id>      ← 必需。上游这里是 `__init__.py`；Rust 插件是
//!                      可执行文件（Windows 上是 `<plugin_id>.exe`）
//!   其它文件（可选）
//! ```
//!
//! ⚠️ **不要在 zip 里再套一层 `<plugin_id>/`**。解压后的目录名由宿主用
//! `manifest.plugin_id` 决定（暂存目录就叫那个名字）。套一层会得到
//! `<staging>/<plugin_id>/<plugin_id>`，宿主按约定去找入口就找不到了 ——
//! 而那会以「插件起不来」的形式暴露，与包本身的问题长得很像。
//!
//! ⚠️ **Windows 上可执行文件带 `.exe`**：插件作者打的是
//! `target/release/<id>.exe`。校验时两个名字都认
//! （见 [`crate::installer::entry_point_of`]）—— 硬编无扩展名会让 Windows 上
//! 打出来的包一个都装不上。
//!
//! # 四道上限（照上游）
//!
//! | 上限 | 值 | 防的是 |
//! |---|---|---|
//! | zip 文件大小 | 100 MiB | 拿一个巨大文件耗尽磁盘 |
//! | 解压后总体积 | 500 MiB | **zip 炸弹**（几 KB 的包解出几个 GB） |
//! | 条目数 | 5000 | 海量小文件拖死文件系统 |
//! | 路径 | 必须在目标目录内 | `../../etc/passwd` 之类的路径穿越 / 符号链接 |
//!
//! ⚠️ 体积按**实际写出的字节**累计，不是按 zip 中央目录里记的
//! `file_size`。上游用的是后者，而那个值在恶意包里可以随便写 —— 一个
//! `file_size = 0` 的炸弹能绕过去。

use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};

use hashing::Sha256;

use crate::manifest::{ManifestProblem, PluginManifest, MANIFEST_FILENAME};

/// 暂存目录名（插件根下）。上游 `PluginManager._staging_dir` 用的是 `.staging`。
pub const STAGING_DIR_NAME: &str = ".staging";

/// 插件的数据目录名（宿主托管，**发布与卸载都保留它**）。
///
/// 它同时是 [`unpack`] 要丢弃的东西（包里自带的一律不要）与 [`publish`] 要
/// 从旧安装搬过来的东西 —— 两处用同一个常量，改名就不会只改一半。
pub const DATA_DIR_NAME: &str = "data";

/// 解压/校验的上限。默认值取上游的三个常量（`installer.py:23-25`）。
///
/// **做成结构体而不是直接用常量**：超限分支只有在能传小值时才是可测的，
/// 而「造一个 500 MiB 的包」会让测试慢到没人愿意跑。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// zip 本身的大小上限。
    pub archive_bytes: u64,
    /// 解压后累计写出的字节上限。
    pub unpacked_bytes: u64,
    /// 条目数上限。
    pub files: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            archive_bytes: 100 * 1024 * 1024,
            unpacked_bytes: 500 * 1024 * 1024,
            files: 5000,
        }
    }
}

/// 安装失败的环节。字符串与上游 `PluginInstallError.stage` 一致。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstallStage {
    /// 校验 zip 本身（存在 / 大小 / sha256）。
    Zip,
    /// 读或解析 `manifest.json`。
    Manifest,
    /// 解压（路径 / 符号链接 / 上限）。
    Extract,
    /// 包内容不合格（缺入口文件）。
    Package,
}

impl InstallStage {
    /// 上游那个字符串，供 API 与日志用。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::Manifest => "manifest",
            Self::Extract => "extract",
            Self::Package => "package",
        }
    }
}

/// 安装失败。
///
/// `plugin_id` 在还没读到清单时是 `"?"` —— 上游也是这么做的
/// （`PluginInstallError("?", "zip", …)`），因为那几个阶段失败时**确实
/// 还不知道**是哪个插件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallError {
    pub plugin_id: String,
    pub stage: InstallStage,
    pub message: String,
}

impl InstallError {
    fn new(plugin_id: &str, stage: InstallStage, message: impl Into<String>) -> Self {
        Self {
            plugin_id: plugin_id.to_owned(),
            stage,
            message: message.into(),
        }
    }

    /// 稳定的错误标识，进 API 的 `error.code`。
    pub fn code(&self) -> String {
        format!("plugin_install_{}_failed", self.stage.as_str())
    }

    /// 人类可读的说明。
    pub fn message(&self) -> String {
        format!(
            "插件{}失败（plugin_id={}）：{}",
            self.stage.as_str(),
            self.plugin_id,
            self.message
        )
    }
}

impl From<ManifestProblem> for InstallError {
    fn from(problem: ManifestProblem) -> Self {
        // 清单已经读出来了，只是内容不合法 —— 但**它可能正是 `plugin_id`
        // 那一项**，所以这里仍然报「不知道是谁」（上游同样给 `"?"`）。
        Self::new("?", InstallStage::Manifest, problem.message())
    }
}

/// 把 zip 解到 `<root>/.staging/<plugin_id>`，返回清单与暂存目录。
///
/// 调用方拿到暂存目录后**必须**二选一：发布（`rename` 到
/// `<root>/<plugin_id>`）或丢弃（删掉暂存目录）。本函数在失败时已经清理
/// 过了，成功时**不**清理 —— 那是发布步骤的事。
pub fn unpack(
    root_dir: &Path,
    zip_path: &Path,
    sha256: Option<&str>,
) -> Result<(PluginManifest, PathBuf), InstallError> {
    unpack_with_limits(root_dir, zip_path, sha256, &Limits::default())
}

/// [`unpack`] 的可注入上限版本（测试用）。
pub fn unpack_with_limits(
    root_dir: &Path,
    zip_path: &Path,
    sha256: Option<&str>,
    limits: &Limits,
) -> Result<(PluginManifest, PathBuf), InstallError> {
    validate_archive(zip_path, sha256, limits)?;
    let manifest = read_manifest_from_zip(zip_path)?;

    let staging = root_dir.join(STAGING_DIR_NAME).join(&manifest.plugin_id);
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(|error| {
            InstallError::new(
                &manifest.plugin_id,
                InstallStage::Extract,
                format!("清理旧暂存目录 {} 失败：{error}", staging.display()),
            )
        })?;
    }
    std::fs::create_dir_all(&staging).map_err(|error| {
        InstallError::new(
            &manifest.plugin_id,
            InstallStage::Extract,
            format!("建暂存目录 {} 失败：{error}", staging.display()),
        )
    })?;

    if let Err(error) = safe_extract(zip_path, &staging, limits) {
        // 失败就把暂存目录清掉 —— 半个包留在那里，下一次安装会先删它，
        // 但「下一次」可能永远不来（作者改包重试之前不会）。
        let _ = std::fs::remove_dir_all(&staging);
        return Err(error);
    }

    // 入口文件：上游查的是 `__init__.py`，Rust 插件查可执行文件。
    if entry_point_of(&staging, &manifest.plugin_id).is_none() {
        let _ = std::fs::remove_dir_all(&staging);
        return Err(InstallError::new(
            &manifest.plugin_id,
            InstallStage::Package,
            format!(
                "包根缺少入口文件：既没有 `{}` 也没有 `{}.exe`",
                manifest.plugin_id, manifest.plugin_id
            ),
        ));
    }

    Ok((manifest, staging))
}

/// 把暂存目录**发布**为正式插件目录，并保留已有的 `data/`。
///
/// 上游 `PluginManager._publish_staging_locked`（`manager.py:296-321`）。
///
/// # 三步，顺序不能换
///
/// ```text
///   1. 丢掉**包里**的 data/          （插件作者不该自带数据）
///   2. 旧安装的 data/ 移进暂存目录   （★ 用户数据在这里，不能随旧代码一起删）
///   3. 删旧目录，再把暂存 rename 成新目录
/// ```
///
/// 第 2 步在第 3 步**之前**：反过来的话，`remove_dir_all(target)` 已经把用户的
/// `data/` 删掉了，再去搬就是搬一个不存在的东西 —— 那会表现为「升级一次，
/// 插件的配置与缓存全没了」，而且不报错。
///
/// # 为什么 rename 而不是拷贝
///
/// 暂存目录就在 `<root_dir>/.staging/` 下，与目标是同一个文件系统，
/// `rename` 是原子的。拷贝会在中途留一个「代码是新的、文件不全」的窗口，
/// 而看门狗恰好在那时拉起插件就会崩 —— 崩了还会被退避重启，反复几次。
///
/// # 失败时清理暂存目录
///
/// 发布失败（比如权限）后暂存目录里是**半个包**，留着只会让下一次安装先删它；
/// 而「下一次」可能永远不来。
pub fn publish(root_dir: &Path, staging: &Path, plugin_id: &str) -> Result<PathBuf, InstallError> {
    let target = root_dir.join(plugin_id);
    if !staging.is_dir() {
        return Err(InstallError::new(
            plugin_id,
            InstallStage::Package,
            format!("暂存目录不存在：{}", staging.display()),
        ));
    }

    let result = publish_inner(&target, staging);
    if let Err(error) = result {
        let _ = std::fs::remove_dir_all(staging);
        return Err(InstallError::new(
            plugin_id,
            InstallStage::Package,
            format!("发布插件目录失败：{error}"),
        ));
    }
    Ok(target)
}

/// [`publish`] 的裸操作（错误原样冒泡，便于测试断言 IO 错误）。
fn publish_inner(target: &Path, staging: &Path) -> io::Result<()> {
    // 1. 包里的 data/ 一律丢弃 —— 那是宿主托管目录，只能来自旧安装。
    let packaged_data = staging.join(DATA_DIR_NAME);
    if packaged_data.exists() {
        std::fs::remove_dir_all(&packaged_data)?;
    }
    // 2. 旧安装的 data/ 搬到暂存目录里（`rename`，不是拷贝）。
    if target.is_dir() {
        let old_data = target.join(DATA_DIR_NAME);
        if old_data.is_dir() {
            std::fs::rename(&old_data, &packaged_data)?;
        }
        std::fs::remove_dir_all(target)?;
    }
    // 3. 暂存 → 正式。
    std::fs::rename(staging, target)
}

/// 插件目录里的入口可执行文件。
///
/// 约定落点是 `<dir>/<plugin_id>`，但 **Windows 上的编译产物带 `.exe`** ——
/// 所以两个名字都认，返回**先命中的那个**。
///
/// 返回 `None` = 这个目录不是可用的插件目录（安装时拒收；拉起时该报
/// 「找不到可执行文件」而不是去 exec 一个不存在的路径）。
pub fn entry_point_of(dir: &Path, plugin_id: &str) -> Option<PathBuf> {
    let plain = dir.join(plugin_id);
    if plain.is_file() {
        return Some(plain);
    }
    let with_exe = dir.join(format!("{plugin_id}.exe"));
    if with_exe.is_file() {
        return Some(with_exe);
    }
    None
}

/// 校验 zip 本身：存在、大小、可选 sha256。
///
/// `sha256` 是**期望值**（十六进制小写，来自 Release API 的 `digest`）；比较前
/// 统一转小写 —— GitHub 给的可能是大写，而不一致会被报成「包损坏」。
fn validate_archive(
    zip_path: &Path,
    sha256: Option<&str>,
    limits: &Limits,
) -> Result<(), InstallError> {
    let metadata = std::fs::metadata(zip_path).map_err(|error| {
        InstallError::new(
            "?",
            InstallStage::Zip,
            format!("zip 不存在或读不到：{error}"),
        )
    })?;
    if !metadata.is_file() {
        return Err(InstallError::new(
            "?",
            InstallStage::Zip,
            format!("{} 不是文件", zip_path.display()),
        ));
    }
    if metadata.len() > limits.archive_bytes {
        return Err(InstallError::new(
            "?",
            InstallStage::Zip,
            format!(
                "zip 超过大小上限（{} > {} 字节）",
                metadata.len(),
                limits.archive_bytes
            ),
        ));
    }

    let Some(expected) = sha256 else {
        return Ok(());
    };
    let actual = sha256_of(zip_path).map_err(|error| {
        InstallError::new("?", InstallStage::Zip, format!("算 sha256 失败：{error}"))
    })?;
    if !actual.eq_ignore_ascii_case(expected.trim()) {
        return Err(InstallError::new(
            "?",
            InstallStage::Zip,
            format!("sha256 不匹配（期望 {expected}，实际 {actual}）"),
        ));
    }
    Ok(())
}

/// 流式算文件的 SHA-256（十六进制小写）。包可能上百 MB，不整个读进内存。
fn sha256_of(path: &Path) -> io::Result<String> {
    use std::io::Read;

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hashing::hex(&hasher.finalize()))
}

/// 从 zip 根部读 `manifest.json`。
///
/// 找不到这个条目就是「包里没有清单」—— 先报 [`InstallStage::Manifest`]，
/// 因为此时还不知道 plugin_id（上游同样给 `"?"`）。
fn read_manifest_from_zip(zip_path: &Path) -> Result<PluginManifest, InstallError> {
    let file = File::open(zip_path).map_err(|error| {
        InstallError::new("?", InstallStage::Zip, format!("打开 zip 失败：{error}"))
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| {
        InstallError::new("?", InstallStage::Zip, format!("不是合法的 zip：{error}"))
    })?;
    let mut entry = archive.by_name(MANIFEST_FILENAME).map_err(|_| {
        InstallError::new(
            "?",
            InstallStage::Manifest,
            format!("zip 里没有 {MANIFEST_FILENAME}（它必须在**包根**，不要在子目录里）"),
        )
    })?;
    let mut raw = Vec::new();
    io::copy(&mut entry, &mut raw).map_err(|error| {
        InstallError::new(
            "?",
            InstallStage::Manifest,
            format!("读 {MANIFEST_FILENAME} 失败：{error}"),
        )
    })?;
    Ok(PluginManifest::parse(&raw)?)
}

/// 安全解压：拒绝路径穿越与符号链接，并执行文件数 / 解压体积上限。
///
/// 路径检查用 zip crate 的 `enclosed_name()` —— 它比手写 `..` 字符串扫描可靠
/// （能处理 `a/../../b`、绝对路径、以及 Windows 的盘符前缀）。
fn safe_extract(zip_path: &Path, dest: &Path, limits: &Limits) -> Result<(), InstallError> {
    let file = File::open(zip_path).map_err(|error| {
        InstallError::new("?", InstallStage::Zip, format!("打开 zip 失败：{error}"))
    })?;
    let mut archive = zip::ZipArchive::new(file).map_err(|error| {
        InstallError::new("?", InstallStage::Zip, format!("不是合法的 zip：{error}"))
    })?;

    let entries = archive.len();
    if entries > limits.files {
        return Err(InstallError::new(
            "?",
            InstallStage::Extract,
            format!("zip 条目数超过上限（{entries} > {}）", limits.files),
        ));
    }

    let mut unpacked: u64 = 0;
    for index in 0..entries {
        let mut entry = archive.by_index(index).map_err(|error| {
            InstallError::new(
                "?",
                InstallStage::Extract,
                format!("读第 {index} 项失败：{error}"),
            )
        })?;
        // 目录条目由下面的 `create_dir_all` 按需创建；不单独处理它可以让
        // 「zip 里没有目录条目」的包（有些工具不写目录项）也能正常解压。
        if entry.is_dir() {
            continue;
        }
        let Some(relative) = entry.enclosed_name() else {
            return Err(InstallError::new(
                "?",
                InstallStage::Extract,
                format!("zip 包含越界路径：{}", entry.name()),
            ));
        };
        // zip 把 Unix 权限位塞在外部属性的高 16 位，`0o120000` 是 `S_IFLNK`。
        // 没有 unix 模式的条目（Windows 打的包）一律当普通文件 —— 那种包
        // 里本来也不该有链接。
        const S_IFMT: u32 = 0o170000;
        const S_IFLNK: u32 = 0o120000;
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & S_IFMT == S_IFLNK)
        {
            return Err(InstallError::new(
                "?",
                InstallStage::Extract,
                format!("zip 不允许符号链接：{}（它会指向包外的文件）", entry.name()),
            ));
        }

        let target = dest.join(&relative);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                InstallError::new(
                    "?",
                    InstallStage::Extract,
                    format!("建目录 {} 失败：{error}", parent.display()),
                )
            })?;
        }
        let mut out = File::create(&target).map_err(|error| {
            InstallError::new(
                "?",
                InstallStage::Extract,
                format!("建文件 {} 失败：{error}", target.display()),
            )
        })?;
        let written = io::copy(&mut entry, &mut out).map_err(|error| {
            InstallError::new(
                "?",
                InstallStage::Extract,
                format!("解压 {} 失败：{error}", entry.name()),
            )
        })?;
        unpacked += written;
        if unpacked > limits.unpacked_bytes {
            return Err(InstallError::new(
                "?",
                InstallStage::Extract,
                format!(
                    "解压体积超过上限（已写 {unpacked} > {} 字节）",
                    limits.unpacked_bytes
                ),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// 造一个 zip。`files` 是 `(条目名, 内容)`。
    ///
    /// 用 STORED（不压缩）—— 本 crate 开的 `deflate` 特性也能写，但测试不该
    /// 依赖压缩实现；而**读** deflate 包的能力由 `deflate_reads_compressed_packages`
    /// 单独验。
    fn make_zip(path: &Path, files: &[(&str, &[u8])]) {
        let file = File::create(path).expect("建 zip");
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        for (name, content) in files {
            writer.start_file(*name, options).expect("写条目");
            writer.write_all(content).expect("写内容");
        }
        writer.finish().expect("收尾");
    }

    fn manifest_bytes(plugin_id: &str) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "plugin_id": plugin_id,
            "display_name": "测试插件",
            "version": "1.0.0",
        }))
        .expect("序列化")
    }

    /// 一个合法的包：根部有清单 + 入口可执行文件。
    fn valid_zip(path: &Path, plugin_id: &str) {
        let manifest = manifest_bytes(plugin_id);
        let entry = plugin_id.as_bytes().to_vec();
        make_zip(
            path,
            &[
                (MANIFEST_FILENAME, &manifest),
                (plugin_id, &entry),
                ("assets/readme.txt", b"hello"),
            ],
        );
    }

    fn temp_root(tag: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "sm-plugins-installer-{tag}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        dir
    }

    // ============================================================ 正常路径

    #[test]
    fn a_valid_package_lands_in_staging() {
        let root = temp_root("valid");
        let zip_path = root.join("local.zip");
        valid_zip(&zip_path, "local");

        let (manifest, staging) = unpack(&root, &zip_path, None).expect("应当装成功");

        assert_eq!(manifest.plugin_id, "local");
        assert_eq!(staging, root.join(STAGING_DIR_NAME).join("local"));
        // 暂存目录名 = plugin_id，而不是 zip 里的某个目录名。
        assert!(staging.join(MANIFEST_FILENAME).is_file());
        assert!(staging.join("local").is_file());
        assert!(
            staging.join("assets/readme.txt").is_file(),
            "子目录也要解出来"
        );
    }

    /// Windows 上编译产物带 `.exe` —— 那个包必须也能装。
    #[test]
    fn an_entry_point_with_exe_suffix_is_accepted() {
        let root = temp_root("exe");
        let zip_path = root.join("local.zip");
        let manifest = manifest_bytes("local");
        make_zip(
            &zip_path,
            &[(MANIFEST_FILENAME, &manifest), ("local.exe", b"MZ")],
        );

        let (manifest, staging) = unpack(&root, &zip_path, None).expect("带 .exe 也该装成功");
        assert_eq!(manifest.plugin_id, "local");
        assert_eq!(
            entry_point_of(&staging, "local"),
            Some(staging.join("local.exe")),
            "入口解析要认得 .exe"
        );
    }

    #[test]
    fn a_matching_sha256_passes() {
        let root = temp_root("sha-ok");
        let zip_path = root.join("local.zip");
        valid_zip(&zip_path, "local");
        let digest = sha256_of(&zip_path).expect("算摘要");

        unpack(&root, &zip_path, Some(&digest)).expect("摘要对上应当通过");
        // GitHub 可能给大写 —— 不该因此判成「包损坏」。
        unpack(&root, &zip_path, Some(&digest.to_uppercase())).expect("大小写不该影响");
    }

    // ============================================================ 拒绝路径

    #[test]
    fn a_mismatched_sha256_is_rejected_before_touching_the_disk() {
        let root = temp_root("sha-bad");
        let zip_path = root.join("local.zip");
        valid_zip(&zip_path, "local");

        let error = unpack(&root, &zip_path, Some(&"0".repeat(64))).expect_err("摘要不符该拒");
        assert_eq!(error.stage, InstallStage::Zip);
        assert!(
            !root.join(STAGING_DIR_NAME).exists(),
            "校验没过就不该建出暂存目录"
        );
    }

    #[test]
    fn a_package_without_manifest_is_rejected() {
        let root = temp_root("no-manifest");
        let zip_path = root.join("local.zip");
        make_zip(&zip_path, &[("local", b"binary")]);

        let error = unpack(&root, &zip_path, None).expect_err("缺清单该拒");
        assert_eq!(error.stage, InstallStage::Manifest);
        assert_eq!(error.plugin_id, "?", "还没读到清单，只能报「不知道是谁」");
        assert!(
            !root.join(STAGING_DIR_NAME).exists(),
            "失败时不该留下暂存目录"
        );
    }

    #[test]
    fn a_package_without_entry_point_is_rejected_and_cleaned_up() {
        let root = temp_root("no-entry");
        let zip_path = root.join("local.zip");
        let manifest = manifest_bytes("local");
        make_zip(
            &zip_path,
            &[(MANIFEST_FILENAME, &manifest), ("readme.txt", b"x")],
        );

        let error = unpack(&root, &zip_path, None).expect_err("缺入口该拒");
        assert_eq!(error.stage, InstallStage::Package);
        assert_eq!(error.plugin_id, "local", "这时已经知道是谁了");
        assert!(
            !root.join(STAGING_DIR_NAME).join("local").exists(),
            "包不合格时必须把暂存目录清掉（不能留半个包）"
        );
    }

    /// ★ 路径穿越。用 `enclosed_name()` 而不是手写 `..` 扫描。
    #[test]
    fn path_traversal_is_rejected() {
        let root = temp_root("traversal");
        let zip_path = root.join("evil.zip");
        let manifest = manifest_bytes("evil");
        make_zip(
            &zip_path,
            &[
                (MANIFEST_FILENAME, &manifest),
                ("evil", b"bin"),
                ("../../escaped.txt", b"pwned"),
            ],
        );

        let error = unpack(&root, &zip_path, None).expect_err("穿越路径该拒");
        assert_eq!(error.stage, InstallStage::Extract);
        assert!(
            !root.join("escaped.txt").exists(),
            "越界文件绝不能落在插件根之外"
        );
        assert!(
            !std::env::temp_dir().join("escaped.txt").exists(),
            "也不能落在更外面"
        );
    }

    /// 符号链接：它指向包外的文件，解出来就等于给了插件一个任意读的入口。
    #[test]
    fn symlinks_are_rejected() {
        let root = temp_root("symlink");
        let zip_path = root.join("evil.zip");
        let manifest = manifest_bytes("evil");
        {
            let file = File::create(&zip_path).expect("建 zip");
            let mut writer = zip::ZipWriter::new(file);
            let stored = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            writer
                .start_file(MANIFEST_FILENAME, stored)
                .expect("写清单");
            writer.write_all(&manifest).expect("写清单内容");
            // `add_symlink` 会把条目标成 `S_IFLNK`（内容里存的是链接目标）。
            // 用 `start_file` + 手工权限位写不出合法条目。
            writer
                .add_symlink("evil", "/etc/passwd", stored)
                .expect("写符号链接条目");
            writer.finish().expect("收尾");
        }

        let error = unpack(&root, &zip_path, None).expect_err("符号链接该拒");
        assert_eq!(error.stage, InstallStage::Extract);
    }

    #[test]
    fn the_archive_size_limit_is_enforced() {
        let root = temp_root("zip-too-big");
        let zip_path = root.join("local.zip");
        valid_zip(&zip_path, "local");

        let limits = Limits {
            archive_bytes: 10, // 任何真实 zip 都超过
            ..Limits::default()
        };
        let error = unpack_with_limits(&root, &zip_path, None, &limits).expect_err("超上限该拒");
        assert_eq!(error.stage, InstallStage::Zip);
    }

    #[test]
    fn the_unpacked_size_limit_is_enforced() {
        let root = temp_root("unpacked-too-big");
        let zip_path = root.join("local.zip");
        let manifest = manifest_bytes("local");
        let big = vec![0_u8; 4096];
        make_zip(
            &zip_path,
            &[(MANIFEST_FILENAME, &manifest), ("local", &big)],
        );

        let limits = Limits {
            unpacked_bytes: 100, // 上面那个 4096 一定超
            ..Limits::default()
        };
        let error = unpack_with_limits(&root, &zip_path, None, &limits).expect_err("超上限该拒");
        assert_eq!(error.stage, InstallStage::Extract);
    }

    #[test]
    fn the_file_count_limit_is_enforced() {
        let root = temp_root("too-many-files");
        let zip_path = root.join("local.zip");
        let manifest = manifest_bytes("local");
        make_zip(
            &zip_path,
            &[
                (MANIFEST_FILENAME, &manifest),
                ("local", b"bin"),
                ("a", b"1"),
                ("b", b"2"),
            ],
        );

        let limits = Limits {
            files: 2, // 上面有 4 条
            ..Limits::default()
        };
        let error = unpack_with_limits(&root, &zip_path, None, &limits).expect_err("超条数该拒");
        assert_eq!(error.stage, InstallStage::Extract);
    }

    /// 重复安装同一个 id：旧暂存目录要先被清掉，否则上次的残留会混进来。
    #[test]
    fn a_previous_staging_directory_is_replaced() {
        let root = temp_root("restage");
        let zip_path = root.join("local.zip");
        valid_zip(&zip_path, "local");

        let (_, staging) = unpack(&root, &zip_path, None).expect("第一次");
        std::fs::write(staging.join("leftover.txt"), b"from the previous run").expect("写残留");

        let (_, staging_again) = unpack(&root, &zip_path, None).expect("第二次");
        assert_eq!(staging, staging_again);
        assert!(
            !staging_again.join("leftover.txt").exists(),
            "上一次的残留必须被清掉"
        );
    }

    /// 读**压缩**包（deflate）。上游生态里的包几乎都是 deflate 打的，
    /// 而本仓自己写包只用 STORED —— 所以这一条必须单独验。
    #[test]
    fn deflate_compressed_packages_are_readable() {
        let root = temp_root("deflate");
        let zip_path = root.join("local.zip");
        let manifest = manifest_bytes("local");
        {
            let file = File::create(&zip_path).expect("建 zip");
            let mut writer = zip::ZipWriter::new(file);
            let deflated = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            writer
                .start_file(MANIFEST_FILENAME, deflated)
                .expect("写清单");
            writer.write_all(&manifest).expect("写清单内容");
            writer.start_file("local", deflated).expect("写入口");
            writer
                .write_all(b"a repeated line\n".repeat(100).as_slice())
                .expect("写入口内容");
            writer.finish().expect("收尾");
        }

        let (manifest, staging) = unpack(&root, &zip_path, None).expect("deflate 包该能读");
        assert_eq!(manifest.plugin_id, "local");
        assert!(staging.join("local").is_file());
    }

    #[test]
    fn a_missing_zip_file_is_reported_as_a_zip_stage_failure() {
        let root = temp_root("missing");
        let error = unpack(&root, &root.join("nope.zip"), None).expect_err("不存在该拒");
        assert_eq!(error.stage, InstallStage::Zip);
        assert_eq!(error.code(), "plugin_install_zip_failed");
    }

    // ============================================================ 发布

    /// ★ 发布替换代码但**保留 `data/`**。
    ///
    /// 这条是升级路径的全部风险所在：第 2 步（搬 `data/`）若晚于第 3 步
    /// （删旧目录），用户的配置与缓存会**静默消失** —— 没有报错、没有日志，
    /// 表现为「升级一次插件就失忆了」。
    #[test]
    fn publishing_keeps_the_previous_data_directory() {
        let root = temp_root("publish-keep-data");
        let zip_path = root.join("local.zip");
        valid_zip(&zip_path, "local");

        // 第一次安装 + 宿主往 data/ 里写东西。
        let (_, staging) = unpack(&root, &zip_path, None).expect("首次解包");
        let target = publish(&root, &staging, "local").expect("首次发布");
        // `fs::write` 不建父目录，`data/` 得先自己建（宿主也会这么干）。
        std::fs::create_dir_all(target.join(DATA_DIR_NAME)).expect("建 data 目录");
        std::fs::write(target.join("data/state.json"), b"user data").expect("写用户数据");

        // 第二次安装（新代码）。
        let (_, staging2) = unpack(&root, &zip_path, None).expect("二次解包");
        let target2 = publish(&root, &staging2, "local").expect("二次发布");

        assert_eq!(target, target2);
        assert_eq!(
            std::fs::read(target2.join("data/state.json")).expect("data 必须还在"),
            b"user data"
        );
        assert!(
            !staging2.exists(),
            "暂存目录已经被 rename 成正式目录，不该还留在 .staging 下"
        );
    }

    /// ★ 包里自带的 `data/` 一律丢弃。
    ///
    /// 否则插件作者可以「发布一份带数据的包」，把上一次的用户数据覆盖掉 ——
    /// 而那看起来像「升级后配置被重置了」。
    #[test]
    fn publishing_discards_a_data_directory_that_came_from_the_package() {
        let root = temp_root("publish-drop-packaged-data");
        let zip_path = root.join("local.zip");
        let manifest = manifest_bytes("local");
        make_zip(
            &zip_path,
            &[
                (MANIFEST_FILENAME, &manifest),
                ("local", b"bin"),
                ("data/should-not-survive.json", b"packaged"),
            ],
        );

        let (_, staging) = unpack(&root, &zip_path, None).expect("解包");
        assert!(
            staging.join("data/should-not-survive.json").is_file(),
            "先确认包里确实带了 data/"
        );
        let target = publish(&root, &staging, "local").expect("发布");

        assert!(
            !target.join("data/should-not-survive.json").exists(),
            "包里带的 data/ 必须被丢弃"
        );
    }

    /// 发布后正式目录就是暂存目录的**原样搬运**（不是拷贝一份再留着暂存）。
    #[test]
    fn publishing_renames_the_staging_directory_into_place() {
        let root = temp_root("publish-rename");
        let zip_path = root.join("local.zip");
        valid_zip(&zip_path, "local");

        let (_, staging) = unpack(&root, &zip_path, None).expect("解包");
        let target = publish(&root, &staging, "local").expect("发布");

        assert_eq!(target, root.join("local"));
        assert!(target.join(MANIFEST_FILENAME).is_file());
        assert!(!staging.exists(), "暂存目录应当已被 rename 走");
    }

    /// 暂存目录不存在时报 `Package` 阶段的错，而不是 panic 或留下半个目标目录。
    #[test]
    fn publishing_a_missing_staging_directory_is_an_error_not_a_panic() {
        let root = temp_root("publish-missing-staging");
        std::fs::create_dir_all(&root).expect("建根");
        let error = publish(&root, &root.join(STAGING_DIR_NAME).join("local"), "local")
            .expect_err("暂存不存在该报错");
        assert_eq!(error.stage, InstallStage::Package);
        assert_eq!(error.plugin_id, "local");
        assert!(!root.join("local").exists(), "不该凭空建出目标目录");
    }
}
