# 插件进程生命周期：缺口与候选最小协议

> **状态：已按第 3 节的候选协议落地**（`sm-plugins::supervisor` + 参考插件可执行文件）。
> 上游与 proto 都没有可对照的版本（见第 1 节），所以它是**本仓库自定的**协议 ——
> 改环境变量名或传参方式的成本现在最低，等真有第二个插件之前定死即可。

## 1. 查证结论：这件事目前没有任何约定

| 参照物 | 结论 |
|---|---|
| 上游 `src/plugins/loader.py` | 用 `importlib.util.spec_from_file_location` **进程内 import**。插件是进程内 Python 包，根本没有子进程、端口、看门狗这回事 |
| `proto/plugin.proto` | `RegisterRequest` 只写「进程启动时由宿主注入 `plugin_id`」，**没说注入方式**；`data_plane_endpoint` 是插件回给宿主的，控制面方向相反，没有对应字段 |
| `docs/parallel/grpc-plugin-report.md` §4 | P1/P2 共 8 条缺口，无一条涉及进程启动或地址发现 |
| `crates/plugin-ref-local` | 是**库**（`spawn()` 供测试起服务），没有二进制、没有 env / argv 约定 |
| 上游 `manifest.json` | 只有 `plugin_id` / `version` / `host_api_version` / `dependencies`，**没有 entry point** |

也就是说：ADR 决策 A 选了 gRPC 作控制面，这**暗示**插件是独立进程；但「怎么把它拉起来、地址怎么约好」谁都没写。

## 2. 必须定下来的四件事

1. **启动命令从哪来** —— manifest 新增字段？配置 `plugins.<id>.command`？还是约定插件目录下的可执行文件？
2. **控制面地址谁定、怎么传给插件** —— 环境变量 / argv / 插件自选后回报（后者需要第二条通道）。
3. **就绪怎么判定** —— 探活 `Register` 的超时与重试；插件起了但不响应算什么错。
4. **崩溃发现与重启** —— 退避策略与上限；重启后 provider / 任务 / 扩展点三张注册表怎么刷新；正在跑的任务怎么收场（`RunJob` 的取消语义是「宿主直接断开流」，重启天然满足）。

## 3. 候选最小协议（**待确认，不是已生效**）

- 宿主 bind `127.0.0.1:0` 拿到随机端口，以环境变量 `SAKURAMEDIA_PLUGIN_GRPC_ADDR` 传给子进程。
  走环境变量而不是 argv，与「宿主注入 `plugin_id`」同一条路：插件不必解析命令行。
- **数据目录与配置同一条路**：
  - `SAKURAMEDIA_PLUGIN_DATA_DIR`（必给）—— proto 承诺「宿主保证可读写且重装插件时保留」，
    `RunJobRequest.data_dir` 也是同一个意思，只是 `Register` 阶段拿不到；
  - `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`（可选，插件没声明 `settings_schema` 就不给）——
    宿主每次拉起**重写** `settings.json`，插件在 `Register` 时读。
  `RegisterRequest` 只有 `plugin_id` 与 `abi_major`，settings 的**值**和 `data_dir`
  都没有通道；与其为它们各加一个 rpc，不如让宿主备好、插件来拿（与端口同一条路）。
- 插件 bind 该地址，serve `PluginControl` + 两个扩展点服务（同一个进程、同一个端口 ——
  proto 没有为扩展点声明另一个端口，只有数据面有 `data_plane_endpoint`）。
- 宿主以 `Register` 探活，超时视为加载失败，错误码沿用既有的 `plugin_unreachable`。
- 子进程退出即崩溃：指数退避重启，超过上限则停用该插件并告警；重启后重新 `Register`
  并**重建**三张注册表（不增量合并：插件重启后声明可能变）。
- **不做**「插件自选端口 + 回报」：那要第二条通道（stdout 或临时文件），比宿主分配更脆，
  且多一处要约定格式。

## 4. 已落地的部分

| 环节 | 落点 |
|---|---|
| 端口分配 + 注入 | `supervisor::reserve_addr` → `SAKURAMEDIA_PLUGIN_GRPC_ADDR` / `SAKURAMEDIA_PLUGIN_ID` |
| 拉起 + 探活 | `supervisor::launch`，探活就是 `loader::register`（顺带验回显 / ABI / 能力） |
| 数据目录 | `SAKURAMEDIA_PLUGIN_DATA_DIR`，宿主 `create_dir_all` 后注入 |
| 配置注入 | `SAKURAMEDIA_PLUGIN_SETTINGS_FILE`，宿主每次拉起重写 `settings.json` |
| 崩溃发现 | `PluginProcess::wait`；句柄被丢弃时连带杀进程（否则野进程占着端口） |
| 重启退避 | `supervisor::restart_backoff`（纯函数） |
| 插件侧 | `plugin-ref-local` 的可执行文件：bind 宿主给的地址，serve `PluginControl` + `StorageProvider` |

**看门狗循环本身没做**：重启要连带重建 provider / 任务 / 扩展点三张注册表，那是组合根
（`sm-server`）的职责，做到它才有意义。这里只给齐零件。

验证方式：`cargo test -p plugin-ref-local --test lifecycle` —— 宿主真的 `spawn` 参考插件
可执行文件并走完 `Register`。

## 5. 组合根已经接上了

| 环节 | 落点 |
|---|---|
| 可执行文件位置 | 约定 `<root_dir>/<plugin_id>/<plugin_id>` —— 不加配置字段（`plugins` 节与上游逐字段对齐，加字段要动那张表与对齐测试） |
| 配置来源 | `ConfigService::snapshot()` 读只读的 `plugins` 节：`root_dir` / `enabled` / `settings` / `job_crons` |
| 装配顺序 | 路由 → **插件** → 调度器（插件任务要并进调度表，反了就漏） |
| cron 覆盖 | `plugins.job_crons[plugin_id][task_key]` 优先于 `default_cron`，与上游 `resolve_job_cron_expr` 同规则 |
| 坏插件 | 记 warn 后跳过，不拖垮启动（上游 `PLUGIN_LOAD_ERRORS` 同义） |
| 关停 | 先停看门狗再停调度器；插件进程由 `PluginProcess` 的 `Drop` 杀掉 |

## 6. 定了之后的落点

- `sm-plugins/src/supervisor.rs`（进程表 + 重启退避），`loader.rs` 从它拿 endpoint。
- 组合根 `sm-server` 才第一次依赖 `sm-plugins`；同一次改动里把插件任务并进调度器
  （`scheduler_specs()` 拼到 `builtin_jobs()` 后面 —— PR #16 留下的那一步）。
- 这条定下来之前，插件任务只能由集成测试驱动，`sm-server` 也继续保持不依赖 `sm-plugins`。
