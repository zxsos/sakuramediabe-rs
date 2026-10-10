# 任务：JavBus 元数据插件 Rust 化

被移植的对象：`upstream/sakuramedia_javbus_metadata/`（Python，324 行：
`plugin.py` 注册、`javbus.py` 抓取与解析、`settings.py` 配置、`manifest.json`）。

目标：一个 Rust 插件 crate，宿主能按生命周期协议拉起它，它作为
`catalog.metadata_source` 扩展点提供 `FetchMovie`。

---

## 一、宿主侧已经齐了 —— 不要再实现一遍

| 能力 | 在哪 |
|---|---|
| 拉起进程、注入 `plugin_id` / 地址 / 数据目录 / 配置 | `crates/sm-plugins/src/supervisor.rs` |
| 扩展点声明的收集与校验 | `crates/sm-plugins/src/extensions.rs` |
| `FetchMovie` / `FetchRanking` 的 rpc 调用面 | `crates/sm-plugins/src/extension_calls.rs` |
| 交付校验（图片落在 `delivery_dir` 内、日期严格、时长为正）+ 清理 | `crates/sm-plugins/src/movie_delivery.rs` |
| 组合根装配（按 `plugins.enabled` 拉起 + 并进调度表 + 看门狗） | `crates/sm-server/src/plugins.rs` |
| 协议全文 | `docs/adr/2026-10-05-plugin-lifecycle.md` |

**可运行的样板**：`crates/plugin-ref-local/src/bin/plugin-ref-local.rs` —— 读环境变量 →
bind 宿主给的地址 → serve `PluginControl` + 数据面 service。生命周期的真服务测试在
`crates/plugin-ref-local/tests/lifecycle.rs`。

## 二、还没做 —— 不要碰，也不要因为它停下

**入库路径没有**：catalog 域缺「插件元数据 → 入库」的服务，所以拿到校验过的结果也
没处写。本任务**只到交付校验为止**，不写导入逻辑、不写库表、不改 `sm-db`。

## 三、照上游做这些

- `register`：回显宿主注入的 `plugin_id`，`abi_major` 取 `sm_plugin_api::ABI_MAJOR`
  （**不要**抄 manifest 里的 `host_api_version: 6`，那是 Python 侧的版本号）。
- 声明扩展点：`Extension.key = "catalog.metadata_source"`，载荷
  `MetadataSourceExtension`。
  **同时必须声明 capability `EXTENSION_CATALOG_METADATA_SOURCE`（40）** ——
  `collect_extensions` 对没声明能力的扩展点一律不收（`extensions.rs` 有测试锁着）。
- `PluginControl::run_job` 必须实现（生成的 trait 没有默认体），返回
  `Status::unimplemented` 即可。
- `fetch_movie`：
  1. 番号为空 → `found = false`；
  2. 抓详情页（404 → `found = false`）；
  3. 解析；番号压平后与请求不一致 → `found = false`；
  4. 封面与剧照下载到**请求目录**，返回路径；
  5. 单张剧照下载失败就跳过那张（`javbus.py:243`），整体失败则清掉整个请求目录再报错。
- 配置：`timeout_seconds`（1..120，默认 20）。在 `RegisterResponse.settings_schema`
  里声明，让宿主渲染表单；值从 `SAKURAMEDIA_PLUGIN_SETTINGS_FILE` 读。

## 四、三处与上游**不同**，别照抄

1. **图片落点**。上游写 `<插件 data_dir>/metadata-tmp/<uuid>/`，而 proto 写的是
   「元数据图片必须落在 `FetchMovieRequest.delivery_dir` 内」，且必须再深一层：
   `<delivery_dir>/<uuid>/<file>`。直接躺在 `delivery_dir` 根下会被判
   `movie_delivery_missing_request_dir`，写到外面是 `movie_delivery_path_escape`。
2. **「没收录」的表达**。上游返回 `None`；gRPC 里是 `found = false` 的**正常响应**，
   `Err` 只表示「调用失败」—— 混起来会让宿主的兜底链路在第一个来源就停下
   （`extension_calls.rs` 的 `MovieLookup` 就是为此存在的）。
3. **配置来源**。上游从 `context.settings` 拿；这里是宿主每次拉起**重写**的 JSON 文件
   （`SAKURAMEDIA_PLUGIN_SETTINGS_FILE`）。插件只读，不写回。

## 五、落点与验证

- crate：`crates/plugin-javbus-metadata`，加进根 `Cargo.toml` 的 `members`；
  `[[bin]]` 名取 `sakuramedia_javbus_metadata`（宿主按约定找
  `<plugins.root_dir>/<plugin_id>/<plugin_id>`）。
- 依赖：`sm-plugin-api`（契约）、`reqwest`（HTTP，替代 httpx）、`tokio`、`serde_json`。
- 单测：HTML 解析（`parse_movie_page` 的各个字段）、`_clean_title`、番号压平比较、
  未收录分支。用上游 `tests/` 里的 HTML fixture 作输入。
- 集成测试（照 `plugin-ref-local/tests/lifecycle.rs`）：用
  `sm_plugins::supervisor::launch` 真拉起自己的可执行文件 →
  `FetchMovie` → `sm_plugins::movie_delivery::validate_movie_delivery` 通过 →
  `cleanup_delivery`。
- **测试不许联网**。把页面与图片交给本地假服务（`wiremock`，`sm-service` 已在用），
  为此给 Settings 加一个 `base_url` 字段（默认上游那个）—— 这一条是对上游的偏离，
  写在 `settings_schema` 里并注明「供测试与镜像站使用」。

## 六、纪律（照 `docs/handoff.md`）

- 先读跨文件依赖再开工；上游是唯一参照物，proto 是契约，不要发明协议。
- 一主题一分支一 PR；不确定的地方（例如 `base_url` 该不该暴露）先问，不要猜。
- 注释写「为什么」，不写作者日期需求编号。
- 改完跑 `bash scripts/verify.sh`（需要 PostgreSQL：`docker compose up -d`，
  并设 `SMDB_TEST_DATABASE_URL=postgres://sakuramedia:sakuramedia@127.0.0.1:5433/sakuramedia_test`）。
