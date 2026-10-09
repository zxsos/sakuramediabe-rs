//! 插件卸载的清理（上游 `system/plugin_removal_service.py`）。
//!
//! # 卸载一个插件要做**四件事**，缺一件都会留下不一致
//!
//! ```text
//!   1. 停插件进程          （看门狗会重启它，见 ADR-2026-10-05）
//!   2. 释放它占用的字段主权  （★ 最容易漏）
//!   3. 清它的扩展点数据      （ranking_source / metadata_source）
//!   4. 删它的配置行
//! ```
//!
//! # ★ 第 2 步漏掉的后果是**用户看不到的永久损坏**
//!
//! `movie.field_owners` / `actor.field_owners` 里记着 `plugin:{id}`。插件卸载后
//! 那些字段**永远显示「已被占用」**，而宿主自动规则写不进去 —— 用户会看到
//! 「元数据再也刷不出来」，且没有任何报错、没有日志线索。
//!
//! 对应 [`MovieOwnershipGateway::release_plugin_owners`](sm_db::repo::MovieOwnershipGateway::release_plugin_owners)
//! 与 [`ActorOwnershipGateway::release_plugin_owners`](sm_db::repo::ActorOwnershipGateway::release_plugin_owners)。
//!
//! # 为什么放在 `system` 域而不是插件宿主
//!
//! 它要同时碰**插件生命周期**（`sm-plugins`）与**四个业务域**的表。放在
//! `system` 是因为「系统级联清理」本身就是 system 域的职责。
//!
//! # 顺序：**先停进程，再清数据**
//!
//! 反过来（先清数据再停进程）会有一个窗口：插件仍活着、可能正在写，而宿主
//! 已经在删它的字段归属。

use crate::error::ServiceError;

/// 卸载清理报告。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PluginRemovalReport {
    pub plugin_id: String,
    /// 是否成功停掉了进程。`false` = 进程本来就没在跑。
    pub process_stopped: bool,
    /// 释放的影片字段数。
    pub released_movie_fields: u64,
    /// 释放的演员字段数。
    pub released_actor_fields: u64,
    /// 删除的榜单来源数。
    pub removed_ranking_sources: u64,
    /// 删除的 `metadata_source` 记录数。
    pub removed_metadata_records: u64,
    /// ★ **未能**完成的清理项。**非空时用户应看到告警** ——
    /// 那一项会留下不一致（见模块文档第 2 步）。
    pub incomplete: Vec<String>,
}

impl PluginRemovalReport {
    /// 是否**完全**清理干净。
    pub fn is_clean(&self) -> bool {
        self.incomplete.is_empty()
    }

    /// 记一条未完成项。**纯函数**。
    pub fn mark_incomplete(&mut self, step: &str) {
        self.incomplete.push(step.to_owned());
    }
}

/// 插件卸载清理服务。
pub struct PluginRemovalService;

impl PluginRemovalService {
    /// ★ 卸载一个插件并清理它的所有痕迹。
    ///
    /// 四步见模块文档。**单步失败不中断**后续步骤，但要记进
    /// [`PluginRemovalReport::incomplete`]。
    pub async fn remove(plugin_id: &str) -> Result<PluginRemovalReport, ServiceError> {
        let _ = plugin_id;
        todo!("骨架：停进程 -> 释放影片字段主权 -> 释放演员字段主权 -> 清扩展点数据 -> 删配置行")
    }

    /// 该插件是否**仍在使用**（有未完成的任务或仍被库引用）。
    ///
    /// 卸载前检查。返回使用原因列表（空 = 可安全卸载）。
    pub async fn usage_report(plugin_id: &str) -> Result<Vec<String>, ServiceError> {
        let _ = plugin_id;
        todo!("骨架：查该插件被哪些库/任务/绑定引用；返回人类可读的原因列表")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ★ 字段主权未释放 = **永久损坏**，必须出现在 `incomplete` 里。
    #[test]
    fn unreleased_field_ownership_shows_up_as_incomplete() {
        let mut report = PluginRemovalReport {
            plugin_id: "local".to_owned(),
            ..PluginRemovalReport::default()
        };
        assert!(report.is_clean());
        report.mark_incomplete("release_movie_fields");
        assert!(!report.is_clean(), "字段主权没释放就不算干净");
    }

    /// 单步失败**不阻止**其它步骤 —— 报告里逐项记录。
    #[test]
    fn one_failed_step_does_not_hide_the_others() {
        let mut report = PluginRemovalReport {
            plugin_id: "115".to_owned(),
            process_stopped: true,
            released_movie_fields: 12,
            released_actor_fields: 0,
            removed_ranking_sources: 3,
            removed_metadata_records: 0,
            incomplete: Vec::new(),
        };
        report.mark_incomplete("release_actor_fields");
        // 其它步骤的成果仍然保留 —— 便于重试时只补缺的那一项。
        assert_eq!(report.released_movie_fields, 12);
        assert_eq!(report.removed_ranking_sources, 3);
        assert_eq!(report.incomplete.len(), 1);
        assert!(!report.is_clean());
    }
}
