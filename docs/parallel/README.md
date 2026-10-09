# 并行开发工作树（方案 A）

三条互不重叠的并行线，各自独立分支 + 独立 `target` 目录 + 独立任务书。

| 线 | 分支 | 工作树 | 任务书 | 触碰范围 |
|---|---|---|---|---|
| A 图像对拍 POC | `feat/image-parity` | `.worktrees/image-parity` | `TASK.md` | `parity/image/**` + `crates/svc-image/src/bin/**` |
| B 纯 Rust 媒体探测 | `feat/svc-probe` | `.worktrees/probe` | `TASK.md` | `crates/svc-probe/**` |
| C gRPC 参考插件 | `feat/grpc-ref-plugin` | `.worktrees/plugin` | `TASK.md` | `crates/plugin-ref-local/**` |

**主线（不在此目录）**：`sm-api` 骨架 —— 错误信封 `IntoResponse` + 鉴权中间件 +
签名 URL + 首个端点。它是所有域的前置，由主线独占。

## 开工模板（每个新 shell 都要）

```bash
cd /workspace/.worktrees/<name>
export PATH="$HOME/.cargo/bin:$PATH"
export CARGO_TARGET_DIR="$(pwd)/target"     # 独立 target，避免抢 cargo 的目录锁
```

## 三条硬规则

1. **禁止 `git add -A` / `git add .`** —— 各任务书里列了允许暂存的路径，其余一律不暂存。
2. **不碰主线文件** —— `sm-api` / `sm-service` / `sm-db` / `sm-core` 由主线独占；
   `Cargo.toml` 只允许加 `members` 与 `[workspace.dependencies]` 各一行。
3. **不要改阈值去凑绿** —— 对拍不一致是有效结论，按 ADR §7 写「建议触发撤销条件」。

## 合并顺序

先合主线（骨架）→ 其余三条逐条 rebase 后合 → **每合一条跑一次全量门禁**：

```bash
cargo test --workspace
cargo clippy --workspace --all-targets --all-features -- -D warnings
python3 parity/compare_schema.py && python3 parity/compare_core.py && python3 parity/compare.py
```

## 决策依据

`docs/adr/2026-10-04-tech-selection.md`（§2 决策表 / §3 反选论证 / §5 未闭环项 / §7 撤销条件）
