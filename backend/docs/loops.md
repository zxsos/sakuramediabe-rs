# 本仓的 Loop 工作流（适配 `jwangkun/loops`）

通用 Loop 提示词装在 **`~/.loops/prompts/zh/`**（100 个，`README` §5.7 的
「直接复制提示词」方式）。那些模板的检查命令是 `npm test` 之类，**照搬没有
意义** —— 本仓是 Rust，而且门禁比"测试绿"严得多。

本文件是把它们**适配到本仓**的那一份。**每轮循环都按这里的检查命令走。**

## 为什么需要单独适配

通用模板的退出条件只到「lint + 测试通过」。而本仓已经出现过这类失败：

- 测试写着但**从未被执行**过（无库环境静默跳过）→ 曾经让 6 个缺陷积累下来
- 数字**手数**导致分母写错（126 应为 177）→ 进度基线因此必须脚本生成
- 凭印象写字段名 / 数值 → 与上游对不上，且不报错

所以本仓的循环把「数字必须实测」「改动必须真跑过」也列为**退出条件**，
而不是可选建议。

## 检查命令（每轮结束必跑）

```powershell
$env:SMDB_TEST_DATABASE_URL='postgres://sakuramedia:sakuramedia@127.0.0.1:5433/sakuramedia_test'
$env:SMVEC_TEST_QDRANT_URL='http://127.0.0.1:6334'
pwsh -File scripts/verify.ps1 -Tier full
```

覆盖 12 项：fmt / rustdoc `-D warnings` / clippy / 单测 / 真库集成 / Qdrant /
schema 对拍 / compare / core / paged wrappers / **两仓契约同步** / 进度基线。

## 退出条件（全部满足才算一轮完成）

1. `verify.ps1 -Tier full` **12 项全绿**
2. 本轮新增的测试**真跑过**（不是只编译过）
3. `docs/progress-baseline.md` 已 `-Write` 并与代码一致
4. 新增的上游字段 / 数值 / 错误码**带上游行号**（`docs/handoff.md` §四）
5. 交付前 `handoff.md` 快照已更新（数字、卡点、新增决定）

未达标 → 回到循环起点；连续两轮同一根因 → **停止上报**（不是原样重试）。

## 本仓在用的三个 Loop

| Loop（通用名） | 本仓用法 | 检查命令 |
|---|---|---|
| `refactor-until-clean` | 改既有代码、消除坏味道 | 上面的 full gate |
| `test-until-green` | 修失败测试 | `cargo test --workspace` |
| `autoloop-tdd` | 新写一条端点/服务方法 | 先写测试 → 实现 → full gate |

通用模板里「收敛保护 / 预算纪律 / 终态明确」三条**直接沿用**：
循环只能以 **成功 / 受阻 / 耗尽** 三种终态结束，
**停滞或预算耗尽绝不能报成成功**。

## ⚠️ Loop 解决不了的部分

循环是「检查 → 修最小根因 → 再检查」。下面几类是**先决条件**，循环只能
在它们有答案之后才开始收敛 —— 不要拿循环去硬啃：

| 阻塞 | 为什么循环没用 |
|---|---|
| provider ABI（`MediaLibraryRegistry` 全仓无实现） | 需要先有实现，不是修 bug |
| `javdb.host` 来源未定 | 需要先做的**配置口径决定** |
| `get_movie_detail` 缺 3 个零件（演员资源 / 标签装配 / 播放候选） | 需要先补零件 |
| 插件 Rust 化（`local_provider` 172KB、`115_provider` 251KB） | 是移植，不是迭代 |

遇到这些 → 记进 `handoff.md` 卡点表，转去下一个可收敛的目标，
**不要在同一处空转迭代**。
