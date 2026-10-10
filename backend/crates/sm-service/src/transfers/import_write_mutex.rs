//! 宿主侧导入写互斥键（上游 `shared/write_mutex.py`，7 行）。
//!
//! # 整个文件只有一个函数，但它决定**导入会不会互相踩**
//!
//! 同一媒体库的两个导入任务如果同时写，provider 侧暂存的文件会互相覆盖，
//! 而宿主这边看到的是「两个都成功」—— 数据静默损坏。所以互斥键不是优化，
//! 是正确性要求。
//!
//! # 键的形状是 `library_import:{library.id}`
//!
//! 与 worker 的 `mutex_key`（`aps:` + `task_key`）**是两套东西**，别混：
//!
//! | | 本键 | worker 的 `mutex_key` |
//! |---|---|---|
//! | 粒度 | **按媒体库** | 按 `task_key`（全库一把锁） |
//! | 挡什么 | 两个导入任务抢同一个库 | 同一个任务被领两次 |
//! | 谁读 | 导入编排（provider 调用前后） | 调度器领任务时 |
//!
//! 也就是说 worker 的锁**挡不住**两个不同的 `library_import` 任务并发 ——
//! 它们的 `task_key` 相同会被串行化，但本键还要在导入服务内部再判一次，
//! 因为 TaskRun 可能在 worker 之外被触发（`POST /imports`）。
//!
//! **不要**把它写成 `format!("aps:library_import")` —— 那样就把按库并行
//! 退化成全局串行，同时看起来「有锁」。

/// 导入写互斥键。
///
/// 上游 `library_import_mutex_key(*, library)`。`library_id` 直接取自
/// `MediaLibrary.id`，**不含插件 id** —— 两个插件配置同一个库时它们本来就
/// 是同一个库，不该各拿一把锁。
pub fn library_import_mutex_key(library_id: i64) -> String {
    format!("library_import:{library_id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 键必须**只**由库 id 决定。
    ///
    /// 若实现里掺进了 task_run_id 或时间戳，两次导入同一个库会拿到不同的键
    /// —— 互斥静默失效，而这种失效表现为「偶发的数据错乱」，极难定位。
    #[test]
    fn the_key_depends_only_on_the_library() {
        assert_eq!(
            library_import_mutex_key(7),
            library_import_mutex_key(7),
            "同一个库必须拿到同一把锁"
        );
        assert_ne!(library_import_mutex_key(7), library_import_mutex_key(8));
    }

    /// 前缀固定为 `library_import:` —— 运维按前缀清理残留锁时依赖它。
    #[test]
    fn the_key_carries_the_documented_prefix() {
        assert!(library_import_mutex_key(1).starts_with("library_import:"));
    }
}
