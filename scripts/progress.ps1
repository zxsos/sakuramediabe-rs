# 重构进度基线 —— 唯一权威的数字来源。
#
# 在它之前每次汇报都是手数，口径每次不同：分母 126 错过三次（真实 177），
# 分子也报过 65 / 76 / 89 三个值（全局正则匹配 vs 只取 routes() 函数体）。
# 手数 = 每次新数字。固化后：
#
#   powershell -File scripts/progress.ps1         # 打印
#   powershell -File scripts/progress.ps1 -Write  # 写入 docs/progress-baseline.md
#   powershell -File scripts/progress.ps1 -Diff   # 与已提交基线对比，漂移则非零退出
#
# -Write 之后基线进 git，下次汇报直接引用基线而不用重数。

param(
    [switch]$Write,
    [switch]$Diff
)

$ErrorActionPreference = 'Continue'
$repo = Split-Path -Parent $PSScriptRoot
$routesDir = Join-Path $repo 'crates\sm-api\src\routes'
$upstreamRouters = Join-Path $repo 'upstream\sakuramediabe\src\api\routers'
$baselinePath = Join-Path $repo 'docs\progress-baseline.md'

# 计数口径的三处正则陷阱，改动前先读：
#   1. 方法名要同时允许 get( 与 axum::routing::put(。写成 \.(get|post|...)
#      会漏掉后者（clip_collections.rs:72 就是那种写法），写成 (?<![\w:])
#      会漏掉链式 .post(。下面这个形态两个坑都躲开。
#   2. 只取 routes() 的大括号函数体 —— 全局匹配会把其后辅助函数里的调用也算进去。
#   3. 匹配前必须剥注释，否则文档里的 get( 会被计入。
$MethodPattern = '(?<![\w:])(?:axum::routing::)?\.?(get|post|put|patch|delete)\s*\('

function Get-RouteBody([string]$Path) {
    $text = [System.IO.File]::ReadAllText($Path)
    $start = $text.IndexOf('pub fn routes()')
    if ($start -lt 0) { return $null }
    $brace = $text.IndexOf('{', $start)
    $depth = 0
    $end = $brace
    for ($k = $brace; $k -lt $text.Length; $k++) {
        if ($text[$k] -eq '{') { $depth++ }
        elseif ($text[$k] -eq '}') { $depth--; if ($depth -eq 0) { $end = $k; break } }
    }
    $body = $text.Substring($brace, $end - $brace + 1)
    $body = [regex]::Replace($body, '(?s)/\*.*?\*/', '')
    return [regex]::Replace($body, '(?m)//.*$', '')
}

function Normalize-Path([string]$P) {
    return (($P -replace '/+$', '') -replace '\{[^}]*\}', '{}')
}

function Count-Lines($Files) {
    $total = 0
    foreach ($f in $Files) { $total += ([System.IO.File]::ReadAllLines($f.FullName)).Count }
    return $total
}

function Pct([int]$part, [int]$whole) {
    if ($whole -eq 0) { return '0%' }
    return ([Math]::Round(100.0 * $part / $whole)).ToString() + '%'
}
# ---- 端点：Rust 侧 ----
$rustByFile = @{}
$rustMethods = 0
$rustPaths = New-Object System.Collections.ArrayList
Get-ChildItem $routesDir -Filter '*.rs' | ForEach-Object {
    $body = Get-RouteBody $_.FullName
    if ($null -eq $body) { return }
    $n = ([regex]::Matches($body, $MethodPattern)).Count
    $rustByFile[$_.Name] = $n
    $rustMethods += $n
    [regex]::Matches($body, '\.route\(\s*"([^"]*)"') | ForEach-Object {
        $null = $rustPaths.Add($_.Groups[1].Value)
    }
}
$rustNormPaths = $rustPaths | ForEach-Object { Normalize-Path $_ } | Sort-Object -Unique

# ---- 端点：上游侧 ----
$upMethods = 0
$upPaths = New-Object System.Collections.ArrayList
$upByDir = @{}
if (Test-Path $upstreamRouters) {
    Get-ChildItem $upstreamRouters -Recurse -File -Filter '*.py' | ForEach-Object {
        $text = [System.IO.File]::ReadAllText($_.FullName)
        $m = [regex]::Match($text, 'APIRouter\((?s).*?prefix\s*=\s*"([^"]*)"')
        $prefix = if ($m.Success) { $m.Groups[1].Value } else { '' }
        $dir = Split-Path $_.DirectoryName -Leaf
        [regex]::Matches($text, '@router\.(get|post|put|patch|delete)\(\s*"([^"]*)"') | ForEach-Object {
            $script:upMethods = $script:upMethods + 1
            if (-not $upByDir.ContainsKey($dir)) { $upByDir[$dir] = 0 }
            $upByDir[$dir] = $upByDir[$dir] + 1
            $null = $upPaths.Add($prefix + $_.Groups[2].Value)
        }
    }
}
$upNormPaths = $upPaths | ForEach-Object { Normalize-Path $_ } | Sort-Object -Unique
$missing = @($upNormPaths | Where-Object { $rustNormPaths -notcontains $_ })

# ---- service 层规模 ----
$serviceUpstream = Join-Path $repo 'upstream\sakuramediabe\src\service'
$serviceRows = @()
Get-ChildItem (Join-Path $repo 'crates\sm-service\src') -Directory | ForEach-Object {
    $rf = @(Get-ChildItem $_.FullName -Recurse -File -Filter '*.rs')
    if ($rf.Count -eq 0) { return }
    $udir = Join-Path $serviceUpstream $_.Name
    $uf = if (Test-Path $udir) { @(Get-ChildItem $udir -File -Filter '*.py') } else { @() }
    $serviceRows += [pscustomobject]@{
        Domain    = $_.Name
        RustFiles = $rf.Count
        RustLines = Count-Lines $rf
        UpFiles   = $uf.Count
    }
}

# ---- 调度 handler ----
$workerPath = Join-Path $repo 'crates\sm-scheduler\src\worker.rs'
$handlers = @()
if (Test-Path $workerPath) {
    $wtext = [System.IO.File]::ReadAllText($workerPath)
    $start = $wtext.IndexOf('pub fn builtin_handlers()')
    if ($start -ge 0) {
        $brace = $wtext.IndexOf('{', $start)
        $depth = 0
        for ($k = $brace; $k -lt $wtext.Length; $k++) {
            if ($wtext[$k] -eq '{') { $depth++ }
            elseif ($wtext[$k] -eq '}') { $depth--; if ($depth -eq 0) { $wend = $k; break } }
        }
        $wbody = $wtext.Substring($brace, $wend - $brace + 1)
        $handlers = @([regex]::Matches($wbody, 'register\(\s*"([a-z_]+)"') |
            ForEach-Object { $_.Groups[1].Value })
    }
}
# 上游内建任务 = BUILTIN_JOB_REGISTRY（registry.py）+ QUEUE_TASK_REGISTRY
# （queue_tasks.py）。**两个都要数** —— 只数前者会漏掉 2 个 producer 入队的
# 任务（library_import / media_storage_transfer），而它们同样需要 handler。
#
# 数 `task_key=` 出现次数，**不是**只匹配 `task_key="字面量"` —— 19 个条目里
# 有 5 个写的是 `task_key=SomeService.TASK_KEY`（类常量），只匹配字面量会
# 少数 5 个。这与端点计数踩的是同一类坑：正则必须对着真实结构写。#
# 早先这里用 `^\s{4}"[a-z_]+":` 匹配，恒为 0：条目形态是
# `JobDefinition(task_key="...")`，不是字典键。教训是**计数正则必须先看
# 真实结构**，别照着记忆写。
$upJobs = 0
$registryPath = Join-Path $repo 'upstream\sakuramediabe\src\scheduler\registry.py'
if (Test-Path $registryPath) {
    $rtext = [System.IO.File]::ReadAllText($registryPath)
    $blockStart = $rtext.IndexOf('BUILTIN_JOB_REGISTRY')
    if ($blockStart -ge 0) {
        $assign = $rtext.IndexOf('=', $blockStart); $bracket = $rtext.IndexOf('[', $assign)
        $depth = 0
        for ($k = $bracket; $k -lt $rtext.Length; $k++) {
            if ($rtext[$k] -eq '[') { $depth++ }
            elseif ($rtext[$k] -eq ']') { $depth--; if ($depth -eq 0) { $rend = $k; break } }
        }
        $block = $rtext.Substring($bracket, $rend - $bracket + 1)
        $upJobs += ([regex]::Matches($block, 'task_key=')).Count
    }
}
$queuePath = Join-Path $repo 'upstream\sakuramediabe\src\scheduler\queue_tasks.py'
if (Test-Path $queuePath) {
    $qtext = [System.IO.File]::ReadAllText($queuePath)
    $qStart = $qtext.IndexOf('QUEUE_TASK_REGISTRY')
    if ($qStart -ge 0) {
        # QUEUE_TASK_REGISTRY 是 **dict**（花括号），不是 list —— 早先按方括号
        # 去配对导致整块没被圈进，2 个任务被漏掉。这已是本脚本里第三次
        # 「正则/配对没对上真实结构」，所以规则写在这里而不是只写进 commit。
        $assign = $qtext.IndexOf('=', $qStart)
        $brace = $qtext.IndexOf('{', $assign)
        $depth = 0
        for ($k = $brace; $k -lt $qtext.Length; $k++) {
            if ($qtext[$k] -eq '{') { $depth++ }
            elseif ($qtext[$k] -eq '}') { $depth--; if ($depth -eq 0) { $qend = $k; break } }
        }
        $block = $qtext.Substring($brace, $qend - $brace + 1)
        $upJobs += ([regex]::Matches($block, 'task_key=')).Count
    }
}
# ---- 输出 ----
$lines = New-Object System.Collections.ArrayList
function Emit([string]$s) { $null = $script:lines.Add($s) }

$commit = (& git -C $repo rev-parse --short HEAD 2>$null | Select-Object -First 1)
Emit '# 重构进度基线'
Emit ''
Emit '生成方式：`powershell -File scripts/progress.ps1 -Write`。**不要手数** —— 手数口径每次不同，'
Emit '历史上分母错过三次（126 应为 177）。改动后跑 `-Diff` 确认基线是否需要更新。'
Emit ''
Emit ('- 提交：`' + $commit + '`')
Emit ''
Emit '## 端点'
Emit ''
Emit '| 口径 | 上游 | Rust | 完成 |'
Emit '|---|---|---|---|'
Emit ('| 方法级（path+method 组合） | ' + $upMethods + ' | ' + $rustMethods + ' | ' + (Pct $rustMethods $upMethods) + ' |')
Emit ('| 唯一路径级（参数归一后） | ' + $upNormPaths.Count + ' | ' + $rustNormPaths.Count + ' | ' + (Pct $rustNormPaths.Count $upNormPaths.Count) + ' |')
Emit ''
Emit ('未实现路径：**' + $missing.Count + '** 条')
Emit ''
foreach ($m in $missing) { Emit ('- `' + $m + '`') }
Emit ''
Emit '### 已完成端点（按文件）'
Emit ''
Emit '| 文件 | 端点 |'
Emit '|---|---|'
foreach ($k in ($rustByFile.Keys | Sort-Object)) { Emit ('| `' + $k + '` | ' + $rustByFile[$k] + ' |') }
Emit ''
Emit '### 上游端点（按子目录）'
Emit ''
Emit '| 子目录 | 端点 |'
Emit '|---|---|'
foreach ($k in ($upByDir.Keys | Sort-Object)) { Emit ('| ' + $k + ' | ' + $upByDir[$k] + ' |') }
Emit ''
Emit '## service 层'
Emit ''
Emit '| 域 | Rust 文件 | Rust 行 | 上游文件 |'
Emit '|---|---|---|---|'
foreach ($r in ($serviceRows | Sort-Object Domain)) {
    Emit ('| ' + $r.Domain + ' | ' + $r.RustFiles + ' | ' + $r.RustLines + ' | ' + $r.UpFiles + ' |')
}
Emit ''
Emit '## 调度'
Emit ''
Emit ('- 上游内建任务：' + $upJobs)
Emit ('- worker handler 已落地：**' + $handlers.Count + '**')
foreach ($h in $handlers) { Emit ('  - `' + $h + '`') }
Emit ''
Emit '> handler 数远少于任务数是当前最大的空白：任务能被 cron 触发入队，'
Emit '> 但没有 handler 去执行。'
Emit ''

$report = $lines -join "`n"

if ($Write) {
    [System.IO.File]::WriteAllText($baselinePath, $report, (New-Object System.Text.UTF8Encoding $false))
    Write-Host "written: $baselinePath"
    exit 0
}
if ($Diff) {
    if (-not (Test-Path $baselinePath)) {
        Write-Host 'MISSING  baseline not found; run -Write first'
        exit 1
    }
    $current = [System.IO.File]::ReadAllText($baselinePath)
    $strip = { param($t) (($t -split "`n") | Where-Object { $_ -notmatch '^- 提交：' }) -join "`n" }
    if ((& $strip $current) -eq (& $strip $report)) {
        Write-Host 'OK  baseline matches current code'
        exit 0
    }
    Write-Host 'DRIFT  baseline differs from code - run -Write and commit'
    exit 1
}
Write-Output $report