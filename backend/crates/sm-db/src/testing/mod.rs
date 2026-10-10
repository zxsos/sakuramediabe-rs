//! 集成测试辅助。
//!
//! # 为什么不用 `#[sqlx::test]`
//!
//! `#[sqlx::test]` 在找不到 `DATABASE_URL` 时会让测试**失败**，而本机
//! 是否起了 PostgreSQL 取决于开发者当时在干什么。硬要求是
//! 「无 `DATABASE_URL` 时全部 skip，不破坏 `cargo test`」—— 仓库里
//! 已有 224 个不依赖数据库的测试，不能因为没起 PG 就变红。
//!
//! 所以用 `#[tokio::test]` + [`maybe_pool`]：拿不到连接就提前 return，
//! 测试**通过**（跳过）。
//!
//! # 测试隔离
//!
//! 每个测试在**独立 schema** 里跑（`smdb_test_<随机>`），用完即弃。
//! 只需 `CREATE SCHEMA` 权限，不需要 `CREATE DATABASE`，也不会让测试
//! 之间互相污染。

pub mod db;

pub use db::{apply_schema, maybe_pool, test_pool, TestDb};
