# sakuramedia

**SakuraMedia monorepo** —— 后端与全部插件合并到同一个仓库。

> 本仓库由 11 个独立仓库合并而成，**各自完整历史均保留**（`git log --follow` 可用）。

## 布局

```
sakuramedia/
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
源仓库仍作为 git remote 保留（名字见下），可随时 `git pull -s subtree <name> main` 再同步。

| 目录 | 源仓库 | remote |
|---|---|---|
| `backend/` | `zxsos/sakuramediabe-rs` | `backend` |
| `plugins/plugin-api/` | `zxsos/sakuramedia-plugin-api` | `plugin-api` |
| `plugins/plugin-ref-local/` | `zxsos/sakuramedia-plugin-ref-local` | `plugin-ref-local` |
| `plugins/115-provider/` | `zxsos/sakuramedia-115-provider` | `p115` |
| `plugins/actor-metadata/` | `zxsos/sakuramedia-actor-metadata` | `actor-metadata` |
| `plugins/javbus-metadata/` | `zxsos/sakuramedia-javbus-metadata` | `javbus-metadata` |
| `plugins/javdb-ranking/` | `zxsos/sakuramedia-javdb-ranking` | `javdb-ranking` |
| `plugins/judge-collection/` | `zxsos/sakuramedia-judge-collection` | `judge-collection` |
| `plugins/more-movies/` | `zxsos/sakuramedia-more-movies` | `more-movies` |
| `plugins/scrape-translate/` | `zxsos/sakuramedia-scrape-translate` | `scrape-translate` |
| `plugins/subtitlecat/` | `zxsos/sakuramedia-subtitlecat` | `subtitlecat` |

> 这些源仓库已**归档（archive，只读）**，作为历史留存。

## ⚠️ 已知注意

- **CI 需要重写**：各子项目原来的 `.github/workflows` 现在位于各自子目录里，
  而 GitHub Actions 只读取仓库根的 `.github/workflows` —— 所以这些配置**当前不会触发**。
  按 monorepo 结构重写（用 `paths:` 过滤）后放回根目录。
- **历史 tag 未并入**：各源仓库的 release tag（`v0.1.x` 等）没有带进本仓库。
- **插件存在两份**：`backend/crates/plugin-*` 是插件的**裁剪内联版**（in-process，省内存）：
  `src/` 源码与 `plugins/*` 一致，但去掉了 CI / 测试 / `src/bin`，且 crate 名不同
  （独立仓库 `plugin-115` → 内联 `plugin-115-provider`）。**改插件要两边同步**。
- **许可**：后端为 **GPL-3.0**（见 `backend/LICENSE`）。
