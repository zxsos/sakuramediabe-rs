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
# 每个路由文件里还剩几个 `todo!()` —— 那些就是「注册了但没实现」的端点。
$rustTodoByFile = @{}
$rustMethods = 0
$rustPaths = New-Object System.Collections.ArrayList
# `路径|METHOD` 对 —— 只报「未实现路径」会漏掉「路径在、但少一个方法」的情形
# （实测就是 2 条：175/177）。所以要按对来比。
$rustPairs = New-Object System.Collections.ArrayList
Get-ChildItem $routesDir -Filter '*.rs' | ForEach-Object {
    $body = Get-RouteBody $_.FullName
    if ($null -eq $body) { return }
    $n = ([regex]::Matches($body, $MethodPattern)).Count
    $rustByFile[$_.Name] = $n
    $rustMethods += $n
    $full = [regex]::Replace([System.IO.File]::ReadAllText($_.FullName), '(?s)/\*.*?\*/', '')
    $full = [regex]::Replace($full, '(?m)//.*$', '')
    $rustTodoByFile[$_.Name] = ([regex]::Matches($full, 'todo!\(|unimplemented!\(')).Count
    # 每次 `.route("path", ...)` 取**路径字面量之后**、下一个 `.route(` 之前的那一段，
    # 把这一段里的方法名全收下 —— 链式 `get(x).post(y)` 也能两个都拿到
    # （只取 `.route("p", get(` 那种写法会漏掉第二个）。
    $routeMatches = [regex]::Matches($body, '\.route\(\s*"([^"]*)"')
    foreach ($rm in $routeMatches) {
        $path = $rm.Groups[1].Value
        $null = $rustPaths.Add($path)
        $from = $rm.Index + $rm.Length
        $nextRoute = $body.IndexOf('.route(', $from)
        $segment = if ($nextRoute -ge 0) { $body.Substring($from, $nextRoute - $from) }
        else { $body.Substring($from) }
        # 复用 $MethodPattern，**不要**在手写 `\.(get|...)` —— 路由表里多数写的是
        # `, get(handler)`（没有前导点），带点的正则会把它们全漏掉，
        # 于是「未注册端点」从 2 条虚报成 60 多条。
        foreach ($mm in [regex]::Matches($segment, $MethodPattern)) {
            $null = $rustPairs.Add((Normalize-Path $path) + '|' + $mm.Groups[1].Value.ToUpper())
        }
    }
}
$rustNormPaths = $rustPaths | ForEach-Object { Normalize-Path $_ } | Sort-Object -Unique
$rustPairSet = $rustPairs | Sort-Object -Unique

# ---- 端点：上游侧 ----
$upMethods = 0
$upPaths = New-Object System.Collections.ArrayList
$upPairs = New-Object System.Collections.ArrayList
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
            $full = $prefix + $_.Groups[2].Value
            $null = $upPaths.Add($full)
            $null = $upPairs.Add((Normalize-Path $full) + '|' + $_.Groups[1].Value.ToUpper())
        }
    }
}
$upNormPaths = $upPaths | ForEach-Object { Normalize-Path $_ } | Sort-Object -Unique
$upPairSet = $upPairs | Sort-Object -Unique
$missing = @($upNormPaths | Where-Object { $rustNormPaths -notcontains $_ })
$missingPairs = @($upPairSet | Where-Object { $rustPairSet -notcontains $_ })

# ---- service 层规模 ----
$serviceUpstream = Join-Path $repo 'upstream\sakuramediabe\src\service'
$serviceRows = @()
Get-ChildItem (Join-Path $repo 'crates\sm-service\src') -Directory | ForEach-Object {
    $rf = @(Get-ChildItem $_.FullName -Recurse -File -Filter '*.rs')
    if ($rf.Count -eq 0) { return }
    $udir = Join-Path $serviceUpstream $_.Name
    # **递归**数上游文件：`transfers` / `playback` / `system` 都有子目录
    # （`downloads/` `imports/` `shared/` …），只数顶层会把 23 个文件数成 1
    # （只剩 `__init__.py`）。基线里那三行的旧数字就是这么来的。
    $uf = if (Test-Path $udir) { @(Get-ChildItem $udir -Recurse -File -Filter '*.py') } else { @() }
    $serviceRows += [pscustomobject]@{
        Domain    = $_.Name
        RustFiles = $rf.Count
        RustLines = Count-Lines $rf
        UpFiles   = $uf.Count
        UpLines   = Count-Lines $uf
    }
}

# ---- 待实现的方法体（todo! / unimplemented!） ----
#
# 为什么要有这一节：**端点数是「注册了多少」，不是「能用了多少」** ——
# 一个已注册的 handler 完全可能是 `todo!()`（本轮之前有多条）。行数同理：
# 骨架期的签名 + 文档占了大头。所以「还剩多少没写」必须单独数。
#
# 口径：`todo!(` / `unimplemented!(` 的**出现次数**（剥掉注释后再数 ——
# 文档里写「其余三个 `todo!()`」的地方很多，不剥会虚高）。
$todoByCrate = @{}
$todoByDir = @{}
$todoByFile = @()
$todoTotal = 0
Get-ChildItem (Join-Path $repo 'crates') -Directory | ForEach-Object {
    $crate = $_.Name
    $src = Join-Path $_.FullName 'src'
    if (-not (Test-Path $src)) { return }
    foreach ($f in @(Get-ChildItem $src -Recurse -File -Filter '*.rs')) {
        $t = [System.IO.File]::ReadAllText($f.FullName)
        $t = [regex]::Replace($t, '(?s)/\*.*?\*/', '')
        $t = [regex]::Replace($t, '(?m)//.*$', '')
        $n = ([regex]::Matches($t, 'todo!\(|unimplemented!\(')).Count
        if ($n -le 0) { continue }
        $script:todoTotal += $n
        if (-not $todoByCrate.ContainsKey($crate)) { $todoByCrate[$crate] = 0 }
        $todoByCrate[$crate] = $todoByCrate[$crate] + $n
        $rel = $f.DirectoryName.Substring($src.Length).TrimStart('\')
        if ($rel -eq '') { $rel = '(根)' }
        $key = "$crate/$rel"
        if (-not $todoByDir.ContainsKey($key)) { $todoByDir[$key] = 0 }
        $todoByDir[$key] = $todoByDir[$key] + $n
        if ($crate -eq 'sm-service') {
            $script:todoByFile += [pscustomobject]@{
                File  = ($f.FullName.Substring($src.Length).TrimStart('\'))
                Count = $n
            }
        }
    }
}
# 路由文件里的 todo!() —— 那些就是「注册了但没实现的端点」（上一步已按文件数过）。
$routeTodos = 0
foreach ($v in $rustTodoByFile.Values) { $routeTodos += $v }

# 一句话进度所需的两个派生值：完成的域、待办最多的三个域。
$todoByDomain = @{}
foreach ($k in $todoByDir.Keys) {
    if ($k -like 'sm-service/*') {
        $todoByDomain[$k.Substring('sm-service/'.Length)] = $todoByDir[$k]
    }
}
$doneDomains = @()
$todoDomains = @()
foreach ($r in $serviceRows) {
    $dn = 0
    if ($todoByDomain.ContainsKey($r.Domain)) { $dn = $todoByDomain[$r.Domain] }
    if ($dn -eq 0) { $doneDomains += ('`' + $r.Domain + '`') }
    else { $todoDomains += [pscustomobject]@{ Domain = $r.Domain; Count = $dn } }
}
$doneLine = if ($doneDomains.Count -eq 0) { '（无）' }
else { ($doneDomains -join '、') + '（' + $doneDomains.Count + '/' + $serviceRows.Count + ' 个域）' }
$topLine = (($todoDomains | Sort-Object -Property @{ Expression = 'Count'; Descending = $true }, Domain |
        Select-Object -First 3 | ForEach-Object { '`' + $_.Domain + '` ' + $_.Count }) -join ' · ')

# ---- 调度 handler ----
$workerPath = Join-Path $repo 'crates\sm-scheduler\src\worker.rs'
$handlers = @()
if (Test-Path $workerPath) {
    $wtext = [System.IO.File]::ReadAllText($workerPath)
    # 不要匹配 `builtin_handlers()` —— 它后来多了参数
    # （`builtin_handlers(deps: HandlerDeps)`），带括号的匹配会**恒为 0**，
    # 于是「handler 已落地」长期显示 0。同上一条：正则必须对着真实签名写。
    $start = $wtext.IndexOf('pub fn builtin_handlers')
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
Emit '# 重构进度'
Emit ''
# ---- 最上面就是结论：一行一项，扫一眼够 ----
Emit '| 指标 | 现在 |'
Emit '|---|---|'
Emit ('| 未实现的方法体（`todo!()`） | **' + $todoTotal + '** 处（`sm-service` ' + $todoByCrate['sm-service'] + ' + 路由 ' + $routeTodos + '）|')
Emit ('| 端点（方法级） | ' + $rustMethods + ' / ' + $upMethods + ' 已注册，**其中 ' + $routeTodos + ' 条仍是 `todo!()`** |')
Emit ('| 端点（路径级） | ' + $rustNormPaths.Count + ' / ' + $upNormPaths.Count + '（未注册的方法级端点 ' + $missingPairs.Count + ' 条）|')
Emit ('| 完成的域 | ' + $doneLine + ' |')
Emit ('| 待办最多的域 | ' + $topLine + ' |')
Emit ('| worker handler | ' + $handlers.Count + ' / ' + $upJobs + ' |')
Emit ('| 基线提交 | `' + $commit + '`（生成时的 HEAD）|')
Emit ''
Emit '> 数字由 `pwsh -File scripts/progress.ps1 -Write` 生成（**不要手数**：手数三次错过'
Emit '> 分母，126 应为 177）。改完代码就跑 `-Write` 并提交本文件 —— 门禁里有 `-Diff`，'
Emit '> 漂移即失败。下面各节是明细。'
Emit ''
Emit '## 待实现的方法体（`todo!()`）'
Emit ''
Emit '口径：`todo!(` / `unimplemented!(` 出现次数（剥掉注释）。与「注册了多少」是两件事。'
Emit ''
Emit ('- 全仓合计：**' + $todoTotal + '** 处')
Emit ('- 其中 `crates/sm-api/src/routes/*.rs`：**' + $routeTodos + '** 处（= 已注册但**未实现**的端点 / 辅助函数）')
Emit ''
Emit '| crate | `todo!()` |'
Emit '|---|---|'
foreach ($k in ($todoByCrate.Keys | Sort-Object { -$todoByCrate[$_] }, { $_ })) {
    Emit ('| `' + $k + '` | ' + $todoByCrate[$k] + ' |')
}
Emit ''
Emit '### 按模块目录'
Emit ''
Emit '| 位置 | `todo!()` |'
Emit '|---|---|'
foreach ($k in ($todoByDir.Keys | Sort-Object { -$todoByDir[$_] }, { $_ })) {
    Emit ('| `' + $k + '` | ' + $todoByDir[$k] + ' |')
}
Emit ''
Emit '### `sm-service` 按文件（降序）'
Emit ''
Emit '| 文件 | `todo!()` |'
Emit '|---|---|'
foreach ($r in ($todoByFile | Sort-Object -Property @{Expression = 'Count'; Descending = $true }, File)) {
    Emit ('| `' + $r.File + '` | ' + $r.Count + ' |')
}
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
Emit ('未**注册**的方法级端点（`路径|方法`）：**' + $missingPairs.Count + '** 条')
Emit ''
Emit '> 只报「未实现路径」会漏掉「路径在、少一个方法」这一档。'
Emit ''
foreach ($m in $missingPairs) { Emit ('- `' + $m + '`') }
Emit ''
Emit ('⚠️ 注册 ≠ 能用：其中 **' + $routeTodos + '** 条的 handler 还是 `todo!()`。')
Emit ''
Emit '### 已注册端点（按文件）'
Emit ''
Emit '第三列 = 这个文件里还是 `todo!()` 的 handler 数；相减才是能用的端点数。'
Emit ''
Emit '| 文件 | 端点（已注册） | 其中仍是 `todo!()` |'
Emit '|---|---|---|'
foreach ($k in ($rustByFile.Keys | Sort-Object)) {
    $t = 0
    if ($rustTodoByFile.ContainsKey($k)) { $t = $rustTodoByFile[$k] }
    Emit ('| `' + $k + '` | ' + $rustByFile[$k] + ' | ' + $t + ' |')
}
Emit ''
Emit '### 上游端点（按子目录）'
Emit ''
Emit '| 子目录 | 端点 |'
Emit '|---|---|'
foreach ($k in ($upByDir.Keys | Sort-Object)) { Emit ('| ' + $k + ' | ' + $upByDir[$k] + ' |') }
Emit ''
Emit '## service 层'
Emit ''
Emit '| 域 | Rust 文件 | Rust 行 | 上游文件 | 上游行 |'
Emit '|---|---|---|---|---|'
foreach ($r in ($serviceRows | Sort-Object Domain)) {
    Emit ('| ' + $r.Domain + ' | ' + $r.RustFiles + ' | ' + $r.RustLines + ' | ' + $r.UpFiles + ' | ' + $r.UpLines + ' |')
}
Emit ('| **合计** | **' + (($serviceRows | Measure-Object -Property RustFiles -Sum).Sum) + '** | **' + (($serviceRows | Measure-Object -Property RustLines -Sum).Sum) + '** | **' + (($serviceRows | Measure-Object -Property UpFiles -Sum).Sum) + '** | **' + (($serviceRows | Measure-Object -Property UpLines -Sum).Sum) + '** |')
Emit ''
Emit '> ⚠️ **行数比不是完成度**（本仓注释占大头）；看上面的 `todo!()`。'
Emit ''
Emit '## 调度'
Emit ''
Emit ('- 上游内建任务：' + $upJobs)
Emit ('- worker handler 已落地：**' + $handlers.Count + '**')
foreach ($h in $handlers) { Emit ('  - `' + $h + '`') }
Emit ''
Emit '> handler 少 = 任务能被 cron 入队、但没人执行。'
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
    # 比对时**剥掉那一行 commit 哈希**：它按设计就是「上次生成时的 HEAD」，
    # 而基线是在提交**之前**生成的 —— 拿它参与比对会让门禁在**任何一次提交之后**
    # 必然漂移（旧正则是 `^- 提交：`，与第 292 行 `| 基线提交 | ... |` 的格式根本
    # 对不上，等于从没剥掉过；2026-10-07 修）。
    $strip = { param($t) (($t -split "`n") | Where-Object { $_ -notmatch '基线提交' }) -join "`n" }
    if ((& $strip $current) -eq (& $strip $report)) {
        Write-Host 'OK  baseline matches current code'
        exit 0
    }
    Write-Host 'DRIFT  baseline differs from code - run -Write and commit'
    exit 1
}
Write-Output $report