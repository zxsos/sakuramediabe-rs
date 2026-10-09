# 契约层拆出独立仓库（`sakuramedia-plugin-api`）

## 为什么拆

插件要放进**各自的仓库**（与上游一样开生态库），而插件只该依赖契约。若契约留在后端
仓库里，每个插件仓库都要依赖整个后端仓库 —— 改一次宿主实现，所有插件都被卷进去。
契约独立后：**宿主与插件各自按 tag 锁契约，除此之外互不依赖**。

## 新仓库的内容

```text
proto/   common.proto storage.proto plugin.proto host.proto
src/     lib.rs（生成代码入口 + ABI_MAJOR）+ provider.rs（插件侧默认实现层）
build.rs vendored protoc
Cargo.toml  README.md
```

**仓库根 = crate 根**，`proto/` 与 `src/` 平级。

## 拆的时候必须改的两处

1. **`Cargo.toml` 去 workspace 继承**：`edition` / `rust-version` / `license` /
   `publish` 写成字面值，依赖全部定版本（原文件里全是 `*.workspace = true`）。
2. **`build.rs` 的 proto 定位**：原写法是
   `CARGO_MANIFEST_DIR.ancestors().nth(2)`（crate 位于 `<workspace>/crates/<name>`
   时指向 workspace 根）。独立仓库里 crate 根就是仓库根，改成
   `CARGO_MANIFEST_DIR.join("proto")`。**漏了这处报的是「proto 找不到」**。

这两处已改好，`cargo check` 与自带 3 条测试在**脱离 workspace**的情况下通过。

## 依赖方式：锁 tag，不要锁 branch

```toml
sm-plugin-api = { git = "https://cnb.cool/zxsos1/sakuramedia-plugin-api.git", tag = "v0.1.0" }
```

用 branch 会让宿主与插件静默漂移到不同版本，症状是「插件按旧 proto 编译、宿主按新
proto 校验」—— 这种不一致**不报错**，只在运行时表现为「插件注册不上」。

## 版本约定

- 契约走 semver；不兼容变更递增版本并在 README 写明影响面。
- `ABI_MAJOR`（契约里的常量）是宿主据以拒绝加载的编号，与 Cargo 版本独立。
- 上游 `manifest.json` 的 `host_api_version: 6` 是 **Python 侧编号**，与
  `ABI_MAJOR` 不是同一套，**不要拿来比**。

## 本仓库这边的待办（等远端仓库建好后做）

- [ ] 删 `crates/sm-plugin-api/`，根 `Cargo.toml` 去掉对应的 `members` 与
      `[workspace.dependencies]` 行
- [ ] 4 个引用方改成按 tag 的 git 依赖：`sm-plugins` / `sm-server` /
      `plugin-ref-local`，外部插件仓库同理
- [ ] `Cargo.lock` 会整体变动；跑 `bash scripts/verify.sh`
