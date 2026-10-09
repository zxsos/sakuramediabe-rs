# Pre-commit gates. Run this instead of trusting that you remembered.
#
# Why it exists: three consecutive commits shipped with the CI
# Documentation step failing while every local check reported green. The
# checks themselves were sound -- $LASTEXITCODE does survive a
# Select-String pipe, which I verified -- but on two of those three
# commits the docs were edited *after* the last doc build, and nothing
# re-ran it. A check you have to remember to run after your final edit is
# not a check.
#
# Every gate below captures the exit code immediately after the command,
# with no pipeline in between. Pipelines reset $LASTEXITCODE, which is
# easy to reintroduce by accident: the first version of this script had
# `& $Body 2>&1 | Out-String` and reported "exit " with a blank code for
# all six gates.
#
# Labels are ASCII on purpose: PowerShell 5.1 reads a BOM-less file as
# ANSI, and CJK in the output turned into mojibake.
#
# Usage:
#   powershell -File scripts/verify.ps1 -Tier fast   # 秒级：fmt + lint + 单元测试
#   powershell -File scripts/verify.ps1 -Tier db     # 加数据库集成测试
#   powershell -File scripts/verify.ps1              # 提交前：全量 + parity
param(
    [switch]$SkipTests,
    # fast = 不碰数据库的循环；db = 加集成测试；full = 全部（默认）。
    # 分层的理由：全量一次要几分钟，而绝大多数提交只需要「我改的东西没坏」。
    # 顺序即代价递增，所以默认仍是 full —— 便宜的全跑不能替代贵的。
    [ValidateSet('fast', 'db', 'full')]
    [string]$Tier = 'full'
)

$ErrorActionPreference = 'Continue'
$manifest = Join-Path $PSScriptRoot '..\Cargo.toml'
$parity = Join-Path $PSScriptRoot '..\parity'

# Integration tests need a real database. Without this they skip rather
# than fail, so a green run locally does not imply they executed.
$env:SMDB_TEST_DATABASE_URL = 'postgres://sakuramedia:sakuramedia@127.0.0.1:5433/sakuramedia_test'
# Qdrant tests skip without this too. Port 6334 is gRPC (what the client
# speaks); the 6333 default in config_schema is REST and will not work.
$env:SMVEC_TEST_QDRANT_URL = 'http://127.0.0.1:6334'

$failed = New-Object System.Collections.ArrayList

function Step {
    param([string]$Name, [scriptblock]$Body)
    # Capture first, format second. Never put the command in a pipeline.
    $captured = & $Body 2>&1
    $code = $LASTEXITCODE
    if ($code -eq 0) {
        Write-Host ('  PASS  ' + $Name)
        return
    }
    Write-Host ('  FAIL  ' + $Name + '  (exit ' + $code + ')')
    $null = $failed.Add($Name)
    $text = ($captured | Out-String)
    # Errors live at the end; the head is progress bars and cargo chatter.
    $text.Trim() -split "`r?`n" |
        Select-Object -Last 12 |
        ForEach-Object { Write-Host ('        ' + $_.Trim()) }
}

Write-Host 'fmt'
$captured = cargo fmt --manifest-path $manifest --all 2>&1
if ($LASTEXITCODE -eq 0) {
    Write-Host '  PASS  cargo fmt'
}
else {
    Write-Host '  FAIL  cargo fmt'
    $null = $failed.Add('cargo fmt')
}

Write-Host 'doc'
$env:RUSTDOCFLAGS = '-D warnings'
Step 'cargo doc --workspace -D warnings' {
    cargo doc --manifest-path $manifest --workspace --no-deps
}
$env:RUSTDOCFLAGS = ''

Write-Host 'lint'
Step 'clippy --all-targets --all-features' {
    cargo clippy --manifest-path $manifest --workspace --all-targets --all-features -- -D warnings
}

# The test tiers. `fast` shrinks the expensive steps rather than silently
# skipping them, so the label always says what actually ran.
$runIntegration = $Tier -ne 'fast'
$runQdrant = $Tier -eq 'full'
$runParity = $Tier -eq 'full'

if (-not $SkipTests) {
    Write-Host 'test'
    # `--lib` only: the in-crate unit tests, no database, no linking of the
    # 57 integration binaries. That linking is most of the wall clock on a
    # cold target dir, and it is wasted work for a fast loop.
    Step 'cargo test --lib (unit only)' {
        cargo test --manifest-path $manifest --offline --workspace --lib
    }
    if ($runIntegration) {
        # --tests picks up every tests/ target, which is where TestDb lives.
        Step 'cargo test --tests (integration, needs PostgreSQL)' {
            cargo test --manifest-path $manifest --offline --workspace --tests
        }
    }
    if ($runQdrant) {
        # Named explicitly: these skip silently when SMVEC_TEST_QDRANT_URL is
        # unset, and a skip looks exactly like a pass in the summary.
        Step 'qdrant (needs Qdrant on :6334)' {
            cargo test --manifest-path $manifest --offline -p sm-service --test qdrant_dense
        }
    }
}

if ($runParity) {
    Write-Host 'parity'
    Step 'schema' { python (Join-Path $parity 'compare_schema.py') }
    Step 'compare' { python (Join-Path $parity 'compare.py') }
    Step 'core' { python (Join-Path $parity 'compare_core.py') }

    # 第七道门。存在的原因见 parity/check_paged_wrappers.py 的文档字符串：
    # paged_list! 自己会生成整个方法（含 page 参数），在外面再手写一层包装
    # 会让函数体返回 ()。编译器会报，但指向宏展开处而不是真正的错误位置。
    # 这个错在本次重构里犯了五次，每一次都要等编译失败才发现。
    Step 'paged wrappers' { python (Join-Path $parity 'check_paged_wrappers.py') }

    # 第八道门：**进度基线漂移**。改了代码却没重跑 `scripts/progress.ps1 -Write`
    # 就会在这里失败 —— 这正是本仓库对「检查」的一贯要求：一个要靠人记得跑的
    # 检查不是检查（见本文件开头那段）。
    #
    # 用 pwsh/powershell **子进程**跑，而不是 `& progress.ps1`：那个脚本在漂移时
    # `exit 1`，同进程调用会把整个 verify 一起干掉。
    Write-Host 'progress'
    $progressHost = if (Get-Command pwsh -ErrorAction SilentlyContinue) { 'pwsh' }
    else { 'powershell' }
    Step 'progress baseline (-Diff)' {
        & $progressHost -NoProfile -File (Join-Path $PSScriptRoot 'progress.ps1') -Diff
    }
}
else {
    Write-Host 'parity'
    Write-Host '  SKIP  parity (needs -Tier full)'
}

Write-Host ''
if ($failed.Count -gt 0) {
    Write-Host ('FAILED: ' + ($failed -join ', '))
    exit 1
}
Write-Host ('all gates passed (tier: ' + $Tier + ')')
