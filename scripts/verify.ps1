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
# Usage:  powershell -File scripts/verify.ps1
param(
    [switch]$SkipTests
)

$ErrorActionPreference = 'Continue'
$manifest = Join-Path $PSScriptRoot '..\Cargo.toml'
$parity = Join-Path $PSScriptRoot '..\parity'

# Integration tests need a real database. Without this they skip rather
# than fail, so a green run locally does not imply they executed.
$env:SMDB_TEST_DATABASE_URL = 'postgres://sakuramedia@127.0.0.1:5433/sakuramedia_test'

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

if (-not $SkipTests) {
    Write-Host 'test'
    Step 'cargo test --offline' { cargo test --manifest-path $manifest --offline }
}

Write-Host 'parity'
Step 'schema' { python (Join-Path $parity 'compare_schema.py') }
Step 'compare' { python (Join-Path $parity 'compare.py') }
Step 'core' { python (Join-Path $parity 'compare_core.py') }

# 第七道门。存在的原因见 parity/check_paged_wrappers.py 的文档字符串：
# paged_list! 自己会生成整个方法（含 page 参数），在外面再手写一层包装
# 会让函数体返回 ()。编译器会报，但指向宏展开处而不是真正的错误位置。
# 这个错在本次重构里犯了五次，每一次都要等编译失败才发现。
Step 'paged wrappers' { python (Join-Path $parity 'check_paged_wrappers.py') }

Write-Host ''
if ($failed.Count -gt 0) {
    Write-Host ('FAILED: ' + ($failed -join ', '))
    exit 1
}
Write-Host 'all gates passed'
