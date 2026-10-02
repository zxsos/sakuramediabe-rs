
---

## Schema 一致性对拍

`parity/schema_contract.py` 从 Peewee 源码用 `ast` 提取 40 张表的契约，
`parity/compare_schema.py` 再与 Rust 结构体逐字段比对。

**为什么需要它**：本机 Docker engine 未运行，无法连真实 PostgreSQL
验证。静态对拍是在「不连库」的前提下发现漂移的唯一手段。

**首次运行抓到 9 处真实缺陷**（其余为工具误报，已修正）：

| 表 | 列 | 问题 |
|---|---|---|
| `background_task_run` | `progress_current` / `progress_total` | int4 映射成 i64 |
| `download_submission_record` | `client_id` / `task_id` | int4 映射成 i64 |
| `image_search_index_state` | `id` | int4 映射成 i64 |
| `image_search_session` | `page_size` | int4 映射成 i64 |
| `media_point` | `video_item_id` | int4 映射成 i64 |
| `system_notification` | `resource_id` / `related_resource_id` | int4 映射成 i64 |

这些列全是小整数（`page_size=20`、单例 `id=1`、计数与资源 id），
用 i64 读在 PostgreSQL 上是安全的 widening，不会出错——但**契约就是契约**，
放宽的类型会让未来的 schema 变更无声积累。已全部收敛为 i32。

### 工具自身的三个修正

首版报 30 处差异，其中 21 处是工具缺陷：

1. **`column_name` 只对外键生效** —— `DownloadTask.movie` 是
   `CharField(..., column_name="movie_number")`，列名与字段名不同。
   只在 FK 上读 column_name 会误报「多一列少一列」。
2. **`text/json` 未与 `text` 归一** —— `JsonTextField` 落的是 TEXT 列
   （只是内容是 JSON 文本），Rust 的 `Option<String>` 是正确映射。
   不归一会让 13 处 JsonTextField 全部误报。
3. **宏生成的类型解析不到** —— 三张合集表由 `macro_rules!` 生成，
   静态解析器看不到字段。已在 `collections::columns` 里显式导出
   `COLUMNS` 与 `TABLE_NAME`，让它们进入检查范围。

**分类处理**：`JsonbField`（真 JSONB 列，`Movie.metadata_source`、
`Movie.field_owners`、`Actor.field_owners`）与 `JsonTextField` 必须区分 ——
前者映射 `serde_json::Value`，后者映射 `Option<String>`。映射阶段已正确处理，
对拍工具也按此归类。
