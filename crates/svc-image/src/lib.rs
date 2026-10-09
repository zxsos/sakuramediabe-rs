//! 宿主侧图像处理原语 —— 替代 Pillow / OpenCV 在**宿主进程**里的用法。
//!
//! # 迁移背景
//!
//! 上游 `src/service/` 里用到图像库的只有两处，且都在宿主进程内：
//!
//! | 位置 | 用途 | 原依赖 |
//! |---|---|---|
//! | `service/catalog/movie_image_service.py:99-146` | 竖封面书脊检测（Sobel 列梯度） | OpenCV + NumPy |
//! | `service/discovery/image_search_input.py:7-21` | 上传图归一化（EXIF + 无损 WebP） | Pillow |
//! | `service/catalog/actor_service.py:701` | 头像转码（有损 WebP q=90 method=6） | Pillow |
//! | `service/videos/video_cover_service.py:66` | 视频封面（有损 WebP q=80） | Pillow(PyAV 抽帧) |
//!
//! 缩略图**不在这里** —— 它由 provider 的 `generate_thumbnails` 产生，
//! 宿主只做校验与打包（`playback/thumbnails/artifacts.py`）。
//!
//! # 为什么不引 `imageproc`
//!
//! `imageproc` 0.27 直接依赖 13 个 crate，含 `nalgebra`（线性代数）、
//! `rayon`、`rustdct`。本模块需要的全部算子是「一个 3×3 Sobel + 一次列求和 +
//! 一次 argmax」，自写约 60 行。为一 60 行的卷积拖进整套线性代数栈不划算。
//!
//! # 未闭环：有损 WebP
//!
//! `image-webp` 0.2.4 **只实现 VP8L 无损编码**（其 `WebPEncoder::new` 文档原文
//! "Only supports \"VP8L\" lossless encoding"）。上游两处有损 WebP（头像、视频
//! 封面）因此**没有纯 Rust 实现**，见 [`webp`] 模块的「有损编码是缺口，不是遗漏」
//! 一节与 `docs/adr/2026-10-04-tech-selection.md`。
//!
//! 本模块**刻意不提供** `encode_lossy` 占位函数：与其让它在运行期 panic 或静默
//! 降级为无损，不如让调用点在编译期就看见这个缺口。
//!
//! # 未闭环：灰度系数需对拍
//!
//! [`cover_split::to_gray`] 用的是 OpenCV **文档**公式（0.299/0.587/0.114），
//! 而 OpenCV 内部是 8 位定点实现。两者在个别像素上可能差 1。列梯度最后会按
//! 最大值归一化，差异被压到 0.4% 以下，分割点通常不受影响 —— 但这条
//! **必须用真实封面与 OpenCV 逐像素对拍确认**，当前环境无 pip，尚未验证。

#![forbid(unsafe_code)]

pub mod paths;
pub mod store;

pub mod cover_split;
pub mod webp;

pub use cover_split::{column_gradient, detect_split_points, to_gray, DEFAULT_CENTER_RANGE};
pub use webp::{decode, encode_lossless, is_webp};
