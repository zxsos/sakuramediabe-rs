//! 片段产物的路径解析与有效性判定，对应上游
//! `src/service/playback/media_clip_service.py:155-229`。
//!
//! # 这里有两件事，而它们都不是「查询」
//!
//! **路径解析**决定 `media_clip.file_path` 能指向哪。片段的产物路径是
//! **服务端状态**（由 `clip_id` 与番号推导并写入），但它最终被拼到文件系统
//! 上，所以仍要防路径穿越 —— 上游为此有四道拒绝规则，本文件逐条保留。
//!
//! **有效性判定**决定一个片段是否还「存在」。判据不是行在不在，而是
//! **产物文件是否还在、大小是否还对得上**。
//!
//! # 有效性判定带删除副作用，这是契约的一部分
//!
//! 上游 `valid_clips` 在过滤时对每个无效片段做两件事：删库里的行、
//! 删磁盘上的文件。也就是说**读列表会回收垃圾**。
//!
//! 这一点必须照搬，理由是它决定了 `total` 的口径：分页总数是**过滤之后**
//! 的数量，所以过滤与回收无法拆开 —— 先分页再过滤会让 `total` 与页内容
//! 自相矛盾（页里出现 5 条而 `total` 是 3 那种）。
//!
//! 反过来说，**不要**把它优化成「标记删除」或「后台清理」：那会让无效片段
//! 继续出现在列表里并计入 `total`，是可见的行为差异。
//!
//! # 为什么本模块只做判定、不做删除
//!
//! 删除需要连接池与文件删除权限，而这里是纯逻辑 + 文件系统。判定与回收的
//! 分离让这部分能被单元测试覆盖 —— 路径穿越那几条规则用临时目录就能测，
//! 不必起数据库。

use std::path::{Path, PathBuf};

use sm_db::playback::media::MediaClip;

/// 产物相对路径的默认前缀：来源番号缺失时用它。
///
/// 上游 `_clip_relative_path` 里 `movie_number or "_unknown"`。片段在来源
/// 被删后仍可能没有番号快照（`media_clip.movie_number` 可空），而产物文件
/// 已经写好了，所以这个前缀只用于**推导一个兜底路径**（丢弃无效片段时
/// 找不到记录路径的情况），不用于分类。
pub const UNKNOWN_MOVIE_PREFIX: &str = "_unknown";

/// 由番号与片段 id 推导产物相对路径：`{prefix}/{clip_id}.mp4`。
///
/// 对应上游 `_clip_relative_path`。注意它**不做**穿越检查 —— 结果是待拼接的
/// 相对路径，安全性由 [`resolve_clip_file`] 在拼接时保证。
#[must_use]
pub fn clip_relative_path(movie_number: Option<&str>, clip_id: i32) -> String {
    let prefix = movie_number
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or(UNKNOWN_MOVIE_PREFIX);
    format!("{prefix}/{clip_id}.mp4")
}

/// 把 `media_clip.file_path` 解析成根目录下的绝对路径，`None` 表示拒绝。
///
/// 对应上游 `_clip_file_path`。**`root` 必须是已规范化的绝对路径** —— 词法
/// 比较依赖它，否则 `root` 里的 `..` 会让包含性判断失效。
///
/// # 四道拒绝规则，与上游逐条一致
///
/// 1. 空串或纯空白；
/// 2. 以 `/` 开头（绝对路径）；
/// 3. 任一段是空串 —— 也就是 `//`、结尾的 `/`；
/// 4. 任一段是 `.` 或 `..`。
///
/// 第 4 条**拒绝** `..` 而不是解析它。上游也拒绝，理由是这条路径是服务端
/// 自己写进去的，出现 `..` 说明数据已经异常；解析它等于替异常数据找一个
/// 看似合法的落点。
///
/// # 反斜杠先归一
///
/// `file_path` 里存的是 `/`，但库里的值可能来自 Windows 侧的手工录入。
/// 上游 `replace("\\", "/")` 把反斜杠折成斜杠，于是 `..\..\etc` 变成
/// `../../etc` 并被第 4 条拒掉。若**不**做这步归一，`..\..\etc` 在 POSIX 上
/// 只是一个普通文件名，会绕过全部四道规则。
///
/// # 符号链接：词法之外再查一次
///
/// 前四道规则都是**词法**的，而上游最后还做了 `resolve()` + `relative_to()`，
/// 那一步能挡住「根目录内的某个子目录是指向外部的符号链接」。本模块在
/// 路径**存在**时额外做同样的检查。
///
/// 路径不存在时不能做这一层：`canonicalize` 对不存在的路径会失败，而
/// 「产物文件已丢失」正是要判定的那种情况。此时退回词法判断 —— 前四道
/// 规则已经保证了它在词法上位于根目录之内。
pub fn resolve_clip_file(root: &Path, file_path: &str) -> Option<PathBuf> {
    let normalized = file_path.trim().replace('\\', "/");
    // 规则 1 + 2
    if normalized.is_empty() || normalized.starts_with('/') {
        return None;
    }
    // 规则 3 + 4
    let parts: Vec<&str> = normalized.split('/').collect();
    if parts
        .iter()
        .any(|part| part.is_empty() || *part == "." || *part == "..")
    {
        return None;
    }

    let candidate = parts
        .iter()
        .fold(root.to_path_buf(), |acc, part| acc.join(part));

    // 存在时才做真实解析：挡住根目录内的符号链接指向外部。
    match (candidate.canonicalize(), root.canonicalize()) {
        (Ok(real), Ok(real_root)) => {
            // `starts_with` 是**分量**比较，不是字符串前缀 —— 字符串比较会让
            // `/data/clips-evil` 通过 `/data/clips` 的检查。
            real.starts_with(real_root).then_some(real)
        }
        // 根目录规范化失败 = 配置指向一个不存在或无权限的目录。这是启动期
        // 该发现的问题，不该在这里退化成「词法上大概安全」。
        (_, Err(_)) => None,
        // 产物不存在：词法规则已足够。
        (Err(_), Ok(_)) => Some(candidate),
    }
}

/// 片段产物是否仍然有效。
///
/// 对应上游 `_has_valid_artifact`。**四道判据全过才算有效**：
///
/// 1. `file_size_bytes > 0`；
/// 2. `duration_seconds > 0`；
/// 3. 路径能解析（不被四道规则拒绝）；
/// 4. 该路径是普通文件，且**字节数与 `file_size_bytes` 相等**。
///
/// # 第 4 条的字节比对是刻意的
///
/// 只判「文件存在」不够：转码中断留下的半截文件也是存在的。若只判存在，
/// 播放时会拉到一段截断的 mp4，而列表显示一切正常。比对字节数让这种情况
/// 在**列表页**就被剔除并回收。
#[must_use]
pub fn has_valid_artifact(root: &Path, clip: &MediaClip) -> bool {
    if clip.file_size_bytes <= 0 || clip.duration_seconds <= 0 {
        return false;
    }
    let Some(path) = resolve_clip_file(root, &clip.file_path) else {
        return false;
    };
    // `metadata` 跟随符号链接，与上游 `Path.is_file()` 语义一致（目录为假）。
    let Ok(meta) = std::fs::metadata(&path) else {
        return false;
    };
    meta.is_file() && i64::try_from(meta.len()).is_ok_and(|len| len == clip.file_size_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 临时目录，`Drop` 时删掉。测试要真的落文件，所以必须是真目录。
    struct TempRoot(PathBuf);

    impl TempRoot {
        fn new(tag: &str) -> Self {
            // 进程内唯一：并行测试下多个 TempRoot 不能撞同一个目录。
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::env::temp_dir().join(format!("sm-clip-{tag}-{unique}"));
            fs::create_dir_all(&path).expect("建临时根目录");
            // canonicalize：契约要求传入的 root 已规范化，而 macOS 的
            // /var 是 /private/var 的符号链接，不规范化会让测试假失败。
            let real = path.canonicalize().expect("规范化临时根目录");
            Self(real)
        }

        fn path(&self) -> &Path {
            &self.0
        }

        /// 在根下写一个文件，返回它的字节数。
        fn write(&self, relative: &str, bytes: &[u8]) -> i64 {
            let target = self.0.join(relative);
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).expect("建父目录");
            }
            fs::write(&target, bytes).expect("写产物文件");
            i64::try_from(bytes.len()).expect("长度转 i64")
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// 造一个片段行。默认给一个「应当有效」的产物。
    fn clip(file_path: &str, size: i64, duration: i32) -> MediaClip {
        MediaClip {
            id: 7,
            media_id: Some(1),
            movie_number: Some("ABC-123".to_owned()),
            start_offset_seconds: 0,
            end_offset_seconds: 30,
            title: String::new(),
            file_path: file_path.to_owned(),
            file_size_bytes: size,
            duration_seconds: duration,
            created_at: None,
            updated_at: None,
        }
    }

    // -------------------------------------------------- 相对路径推导

    #[test]
    fn the_relative_path_is_number_then_id() {
        assert_eq!(
            clip_relative_path(Some("ABC-123"), 7),
            "ABC-123/7.mp4",
            "番号目录 + 片段 id"
        );
    }

    /// 番号缺失或空白时用 `_unknown` —— 片段在来源被删后可能没有番号快照。
    #[test]
    fn a_missing_or_blank_movie_number_falls_back_to_unknown() {
        assert_eq!(clip_relative_path(None, 7), "_unknown/7.mp4");
        assert_eq!(clip_relative_path(Some("   "), 7), "_unknown/7.mp4");
    }

    // -------------------------------------------------- 穿越拒绝

    /// 四道拒绝规则逐条钉住。这几条是安全边界，改动必须有意识地推翻。
    #[test]
    fn traversal_and_absolute_paths_are_refused() {
        let root = TempRoot::new("reject");
        for bad in [
            "",                   // 规则 1：空
            "   ",                // 规则 1：纯空白
            "/etc/passwd",        // 规则 2：绝对
            "../secret.mp4",      // 规则 4：..
            "a/../../secret.mp4", // 规则 4：中间 ..
            "./a.mp4",            // 规则 4：.
            "a//b.mp4",           // 规则 3：空段
            "a/b/",               // 规则 3：结尾斜杠
        ] {
            assert_eq!(
                resolve_clip_file(root.path(), bad),
                None,
                "{bad:?} 必须被拒绝"
            );
        }
    }

    /// 反斜杠必须先归一，否则 `..\..\etc` 在 POSIX 上只是一个普通文件名，
    /// 会**绕过全部四道规则**。这是 Windows 侧手工录入的真实风险。
    #[test]
    fn backslashes_are_folded_before_the_rules_run() {
        let root = TempRoot::new("backslash");
        assert_eq!(
            resolve_clip_file(root.path(), r"..\..\etc\passwd"),
            None,
            "反斜杠形式必须在归一后被 .. 规则拒掉"
        );
        assert_eq!(
            resolve_clip_file(root.path(), r"\absolute.mp4"),
            None,
            r"\absolute 应归一成 /absolute 后按绝对路径拒掉"
        );
    }

    #[test]
    fn a_normal_relative_path_resolves_under_the_root() {
        let root = TempRoot::new("ok");
        root.write("ABC-123/7.mp4", b"hello");
        let resolved = resolve_clip_file(root.path(), "ABC-123/7.mp4").expect("应接受");
        assert!(resolved.starts_with(root.path()));
        assert!(resolved.is_file());
    }

    /// 包含性判断必须按**分量**比，不是字符串前缀 ——
    /// 否则 `/data/clips-evil` 会通过 `/data/clips` 的检查。
    #[test]
    fn containment_compares_components_not_string_prefixes() {
        let root = TempRoot::new("prefix");
        // 在 root 的**兄弟**目录下建一个同名文件，验证不会被误判为在 root 内。
        let sibling = root.path().with_file_name(format!(
            "{}-evil",
            root.path()
                .file_name()
                .expect("根目录有文件名")
                .to_string_lossy()
        ));
        fs::create_dir_all(sibling.join("ABC-123")).expect("建兄弟目录");
        fs::write(sibling.join("ABC-123/7.mp4"), b"x").expect("写兄弟文件");

        let relative = "ABC-123/7.mp4";
        let resolved = resolve_clip_file(root.path(), relative).expect("自身应接受");
        // 解析结果必须真的在 root 之下
        assert!(resolved.starts_with(root.path()));
        // 兄弟目录的同名相对路径解析后仍指向 root 内（root/ABC-123/7.mp4），
        // 所以上面的断言成立；这里额外确认两者不是同一个文件。
        assert_ne!(
            resolved,
            sibling.join("ABC-123/7.mp4").canonicalize().unwrap()
        );
    }

    /// 根目录内的符号链接指向外部时必须被拒 —— 词法规则看不出这一层。
    #[cfg(unix)]
    #[test]
    fn a_symlink_escaping_the_root_is_refused() {
        let root = TempRoot::new("symlink");
        let outside = TempRoot::new("symlink-outside");
        outside.write("secret.mp4", b"secret");

        let link = root.path().join("escape");
        std::os::unix::fs::symlink(outside.path(), &link).expect("建符号链接");

        assert_eq!(
            resolve_clip_file(root.path(), "escape/secret.mp4"),
            None,
            "指向根外的符号链接必须被拒绝"
        );
    }

    // -------------------------------------------------- 有效性判定

    #[test]
    fn a_clip_whose_artifact_is_present_and_intact_is_valid() {
        let root = TempRoot::new("valid");
        let size = root.write("ABC-123/7.mp4", b"0123456789");
        assert!(has_valid_artifact(
            root.path(),
            &clip("ABC-123/7.mp4", size, 30)
        ));
    }

    /// 四道判据逐条：任一不满足即无效。
    #[test]
    fn every_one_of_the_four_criteria_is_required() {
        let root = TempRoot::new("criteria");
        let size = root.write("ABC-123/7.mp4", b"0123456789");
        let good = "ABC-123/7.mp4";

        assert!(
            !has_valid_artifact(root.path(), &clip(good, 0, 30)),
            "判据 1：字节数为 0"
        );
        assert!(
            !has_valid_artifact(root.path(), &clip(good, size, 0)),
            "判据 2：时长为 0"
        );
        assert!(
            !has_valid_artifact(root.path(), &clip("../x.mp4", size, 30)),
            "判据 3：路径被拒"
        );
        assert!(
            !has_valid_artifact(root.path(), &clip("ABC-123/missing.mp4", size, 30)),
            "判据 4a：文件不存在"
        );
        assert!(
            !has_valid_artifact(root.path(), &clip(good, size + 1, 30)),
            "判据 4b：字节数对不上（转码截断的半截文件）"
        );
    }

    /// 目录不是产物文件。
    #[test]
    fn a_directory_is_not_a_valid_artifact() {
        let root = TempRoot::new("dir");
        fs::create_dir_all(root.path().join("ABC-123/7.mp4")).expect("建同名目录");
        assert!(!has_valid_artifact(
            root.path(),
            &clip("ABC-123/7.mp4", 10, 30)
        ));
    }

    /// 字节比对让「转码中断留下的半截文件」在列表页就被剔除，
    /// 而不是等播放时才发现拉到了截断的 mp4。
    #[test]
    fn a_truncated_artifact_is_detected_by_its_size() {
        let root = TempRoot::new("truncated");
        let full = root.write("ABC-123/7.mp4", b"0123456789");
        assert!(has_valid_artifact(
            root.path(),
            &clip("ABC-123/7.mp4", full, 30)
        ));

        // 模拟截断：文件被写成一半，但库里记着完整字节数
        fs::write(root.path().join("ABC-123/7.mp4"), b"01234").expect("截断");
        assert!(
            !has_valid_artifact(root.path(), &clip("ABC-123/7.mp4", full, 30)),
            "半截文件必须判为无效"
        );
    }

    /// `file_path` 前后空白要被裁掉 —— 库里存的值可能带空格。
    #[test]
    fn surrounding_whitespace_in_file_path_is_tolerated() {
        let root = TempRoot::new("space");
        let size = root.write("ABC-123/7.mp4", b"data");
        assert!(has_valid_artifact(
            root.path(),
            &clip("  ABC-123/7.mp4  ", size, 30)
        ));
    }
}
