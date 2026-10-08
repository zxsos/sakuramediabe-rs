# sakuramedia-plugin-api

SakuraMedia 插件体系的**契约层**：`proto/` 下四个文件经 prost/tonic 生成的
Rust 类型，外加一层给插件用的默认实现。

从 [`sakuramediabe-rs`](https://github.com/zxsos/sakuramediabe-rs) 拆出来的原因在
那个仓库的 `docs/plugin-api-split.md`：插件要各自成仓，而插件只该依赖契约；
契约若留在后端仓库里，每个插件仓都得依赖整个后端。拆出来之后，

> **宿主与插件各自按 tag 锁契约，除此之外互不依赖。**

## 内容

| 文件 | 作用 |
|---|---|
| `proto/common.proto` | 共享类型（句柄、结果、枚举、错误） |
| `proto/storage.proto` | 存储与下载 Provider（30 个方法全覆盖） |
| `proto/plugin.proto` | 插件生命周期、任务、扩展点 |
| `proto/host.proto` | 宿主提供给插件的能力（`PluginContext`） |
| `src/lib.rs` | 生成代码入口 + `PACKAGE` + `ABI_MAJOR` |
| `src/provider.rs` | 插件侧默认实现层（37 个 rpc 全部有默认体） |

统一 package `sakuramedia.v1` 而非分包，是为了让 prost 生成同包引用 —— 跨包
引用会要求调用方额外提供一层模块层级，而这一层在 `build.rs` 里无法可靠表达。

## 依赖方式

```toml
sm-plugin-api = { git = "https://github.com/zxsos/sakuramedia-plugin-api.git", tag = "v0.2.0" }
```

**按 tag，不要按 branch**：用 branch 会让宿主与插件静默漂移到不同版本的契约，
症状是「插件按旧 proto 编译、宿主按新 proto 校验」—— 这种不一致**不报错**，
只在运行时表现为插件注册不上。

## 版本约定

- 契约本身走 semver；不兼容变更递增版本并在 README 写明影响面。
- `ABI_MAJOR`（契约里的常量）是宿主据以拒绝加载的编号，与 Cargo 版本相互独立。
- 上游 Python 插件 `manifest.json` 里的 `host_api_version: 6` 是 **Python 侧
  编号**，与 `ABI_MAJOR` 不是同一套，不要拿来比。

## 构建与测试

```bash
cargo test          # 3 条默认实现层的单测
```

`build.rs` 用 vendored protoc，不需要本机安装 protoc。

## 许可证

GPL-3.0-or-later（见 `LICENSE`）。
