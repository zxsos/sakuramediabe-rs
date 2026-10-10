//! 媒体缩略图子系统（上游 `playback/thumbnails/`，4 文件 / 794 行）。
//!
//! 上游把它从 `media_thumbnail_service.py`（16 行的**纯门面**）里拆出来，
//! 本仓跟��拆 —— 四件事（契约 / 心跳 / 产物 / 任务）互相依赖方向清晰，
//! 塞在一个文件里会出现「任务服务引用产物校验、产物又引用任务的常量」。
//!
//! | 模块 | 上游 | 职责 |
//! |---|---|---|
//! | [`contracts`] | `contracts.py`(23) | 延迟态契约（**唯一会无限 pending 的那个坑**） |
//! | [`progress`] | `progress.py`(57) | provider 调用期间的心跳 |
//! | [`artifacts`] | `artifacts.py`(205) | 产物校验与落盘（**信任边界**） |
//! | [`task_service`] | `task_service.py`(509) | 三条重试轨道 |

pub mod artifacts;
pub mod contracts;
pub mod progress;
pub mod task_service;
