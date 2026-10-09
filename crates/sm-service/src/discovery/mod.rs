//! `discovery` 域的 service 层。
//!
//! # 现状：本包只做了推理客户端，Qdrant 存储层还没写
//!
//! 上游 `src/service/discovery/` 有 16 个文件，按是否碰向量库分成两半：
//!
//! | | 文件数 | 体积 | 依赖 |
//! |---|---|---|---|
//! | 需要 Qdrant | 11 | ~138 KB | `qdrant_client`（gRPC）+ 推理服务 |
//! | **不需要** | 5 | ~37 KB | 只用 PostgreSQL / PIL |
//!
//! 本包属于**第一半里唯一不需要向量库的那一个** —— `embedding.py` 只发
//! HTTP。另一半（`qdrant_thumbnail_store` / `qdrant_movie_similarity_store` /
//! `qdrant_plot_image_store` 与依赖它们的 6 个 service）还没搬。
//!
//! # 那一半其实已经不卡了（曾被误记为「缺 Qdrant 客户端」）
//!
//! 查过 `qdrant-client` 1.19.0 的依赖：**15 个里 14 个已在 `Cargo.lock` 中**
//! —— `tonic` 0.14.6 与 `tonic-prost` 0.14.6 **精确一致**、`prost` 0.14.4、
//! `reqwest` 0.13.5、`parking_lot` 0.12.5、`semver` 1.0.28 全部满足，
//! 且 lock 里 tonic/prost **各只有一个版本**（不会编两套 gRPC）。
//! 真正的增量只有 `derive_builder` 一个 derive 宏 crate。
//!
//! 原先「与零依赖原则冲突」的说法也不成立：`README.md` 的「零外部依赖」一节
//! 限定的是 `hashing` / `media-file-hash` / `svc-hash` 三个库自己实现
//! SHA-1/Base32/bencode，**不是全仓禁用第三方 crate**（实际已有 46 个直接
//! 依赖、382 个 crate），而 tonic/prost 本来就是插件系统带进来的。
//!
//! 真正还没着落的是**推理服务本身**（哪个模型、哪个地址）—— 那不是代码问题。

pub mod embedding;
pub mod qdrant;

pub use embedding::{EmbeddingClient, EmbeddingSpace};
