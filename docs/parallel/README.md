# 并行开发工作树（方案 A）

> **当前状态：线 C 已交付，其余两条未开工。**
>
> | 线 | 分支 | 状态 |
> |---|---|---|
> | C gRPC 参考插件 | `feat/grpc-ref-plugin` | **已合入 main**（`plugin-ref-local`，+2033 行），报告见 [grpc-plugin-report.md](grpc-plugin-report.md) |
> | A 图像对拍 POC | `feat/image-parity` | 未开工。结论已落进 `svc-image` 与 ADR §3.4，不再需要独立分支 |
> | B 纯 Rust 媒体探测 | `feat/svc-probe` | 未开工。ADR §3.3 已定「默认 `ffprobe` CLI，缺失时切 `symphonia`」 |
>
> 三条分支都**不在本地**（`git worktree list` 只有仓库本身）——本轮走的是单线
> 推进，`feat/grpc-ref-plugin` 也已用 `cherry-pick` 重放进 main，因此 CNB 上
> 那条 MR 的 head SHA 不再是 main 的祖先，**需要手动关闭**。
>
> 方案本身仍然成立：线 C 的实测结论（控制面 0.4ms 可接受、字节搬运必须走
> `data_plane_endpoint`、4 条 P1 proto 缺口）已经改变了后续优先级 —— 见报告
> §5「给主线的下一步建议」。

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
bash scripts/verify.sh
```

（`scripts/verify.sh` 就是上面那串命令的固化版本，另加 `fmt` 与 `doc` 两道。）

## 决策依据

`docs/adr/2026-10-04-tech-selection.md`（§2 决策表 / §3 反选论证 / §5 未闭环项 / §7 撤销条件）
