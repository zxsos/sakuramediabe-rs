# sakuramediabe-rs

**SakuraMedia monorepo** —— 后端与全部插件合并在同一个仓库里。
（仓库名沿用了原来后端的名字 `sakuramediabe-rs`。）

> 本仓库由 11 个独立仓库合并而成，**各自完整历史均保留**（`git log --follow` 可用）。
> 原来的 11 个仓库已于 2026-10-10 **删除**，内容与历史全部并入本库，本库是唯一来源。

## 布局

```
sakuramediabe-rs/
├── backend/                 # 后端（Rust workspace，GPL-3.0）
└── plugins/
    ├── plugin-api/          # 插件 gRPC 契约（sm-plugin-api）
    ├── plugin-ref-local/    # 参考插件：本地目录 StorageProvider
    ├── 115-provider/        # 115 网盘 StorageProvider
    ├── actor-metadata/      # 演员元数据（JavDB / MinnanoAV）
    ├── javbus-metadata/     # JavBus 元数据补缺
    ├── javdb-ranking/       # JavDB 排行榜
    ├── judge-collection/    # 合集判定
    ├── more-movies/         # 更多影片源
    ├── scrape-translate/    # 抓取翻译
    └── subtitlecat/         # SubtitleCat 字幕下载
```

## 来源

合并方式：对每个源仓库执行 subtree merge
（`git merge -s ours --no-commit --allow-unrelated-histories` +
`git read-tree --prefix=<dir>/ -u <ref>`），因此历史与内容完整保留。

| 目录 | 原仓库（已删除） |
|---|---|
| `backend/` | `zxsos/sakuramediabe-rs` |
| `plugins/plugin-api/` | `zxsos/sakuramedia-plugin-api` |
| `plugins/plugin-ref-local/` | `zxsos/sakuramedia-plugin-ref-local` |
| `plugins/115-provider/` | `zxsos/sakuramedia-115-provider` |
| `plugins/actor-metadata/` | `zxsos/sakuramedia-actor-metadata` |
| `plugins/javbus-metadata/` | `zxsos/sakuramedia-javbus-metadata` |
| `plugins/javdb-ranking/` | `zxsos/sakuramedia-javdb-ranking` |
| `plugins/judge-collection/` | `zxsos/sakuramedia-judge-collection` |
| `plugins/more-movies/` | `zxsos/sakuramedia-more-movies` |
| `plugins/scrape-translate/` | `zxsos/sakuramedia-scrape-translate` |
| `plugins/subtitlecat/` | `zxsos/sakuramedia-subtitlecat` |

## ⚠️ 已知注意

- **CI 需要重建**：各子项目原来的 `.github/workflows` 现在位于各自子目录里，
  而 GitHub Actions 只读取仓库根的 `.github/workflows` —— 所以这些配置**当前不会触发**。
  需按 monorepo 结构在根目录重建（用 `paths:` 做路径过滤）。
- **历史 release tag 未保留**：原仓库的 tag（`v0.1.x` 等）随仓库删除已不存在，
  需要的话在本库重新打。
- **插件存在两份**：`backend/crates/plugin-*` 是插件的**裁剪内联版**（in-process，省内存）：
  `src/` 源码与 `plugins/*` 一致，但去掉了 CI / 测试 / `src/bin`，且 crate 名不同
  （原来的 `plugin-115` → 内联 `plugin-115-provider`）。**改插件要两边同步**。
- **许可**：后端为 **GPL-3.0**（见 `backend/LICENSE`）。
