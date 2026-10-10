# sm-server 本地启动。
#
# 前置：先跑 `scripts/dev-services.ps1 up` 把 PostgreSQL / Qdrant 拉起来。
# （2026-10-10 起这两个依赖跑在 WSL Alpine 里；Windows 侧靠 mirrored 网络
#  能直连 127.0.0.1:5433/6334，所以本脚本在 Windows 上跑服务依然成立。）
#
# # 为什么环境变量只填三项
#
# 配置分两层（见 crates/sm-server/src/config.rs）：域值字段有模式默认值，
# 但其中两项的默认值是**空**，而 validate() 要求非空 —— 不填就退出码 2：
#
#   | 变量 | 空值的后果 |
#   |---|---|
#   | SAKURAMEDIA_DATABASE_URL | ConfigError::Missing，服务不启动 |
#   | SAKURAMEDIA_JWT_SECRET   | 空密钥 = 所有人相同的公开密钥，宁可不启动 |
#
# 其余（host / port / 日志 / 调度器）都有可用默认，本脚本只在需要时覆盖。
#
# # 产物路径
#
# 用户级 ~/.cargo/config.toml 可能把 target-dir 指到仓库外（本机是
# D:/cargo-target，因为 C: 空间吃紧）。所以这里按 CARGO_TARGET_DIR ->
# 仓库 ./target -> D:/cargo-target 依次探测，而不是写死 ./target/debug。
#
# # 用法
#
#   powershell -File scripts/run-server.ps1
#   powershell -File scripts/run-server.ps1 -Port 8001 -SchedulerEnabled

param(
    [int]$Port = 8000,
    [switch]$SchedulerEnabled
)

$ErrorActionPreference = 'Stop'

$RepoRoot = Split-Path $PSScriptRoot -Parent

# 默认数据库指向 dev-services.ps1 起的那个库（postgres 自己的监听端口 5433）。
if (-not $env:SAKURAMEDIA_DATABASE_URL) {
    $env:SAKURAMEDIA_DATABASE_URL = 'postgresql://sakuramedia:sakuramedia@127.0.0.1:5433/sakuramedia_test'
}
# 本地开发用固定值即可；生产必须换成真随机密钥。
if (-not $env:SAKURAMEDIA_JWT_SECRET) {
    $env:SAKURAMEDIA_JWT_SECRET = 'dev-secret-please-change'
}
$env:SAKURAMEDIA_PORT = "$Port"
# 单跑 API（开发前端 / 手动验证）时不让调度器在背后入队。
$env:SAKURAMEDIA_SCHEDULER_ENABLED = if ($SchedulerEnabled) { '1' } else { '0' }

$candidates = @()
if ($env:CARGO_TARGET_DIR) { $candidates += $env:CARGO_TARGET_DIR }
$candidates += (Join-Path $RepoRoot 'target')
$candidates += 'D:/cargo-target'
$exe = $candidates |
    ForEach-Object { Join-Path $_ 'debug/sm-server.exe' } |
    Where-Object { Test-Path $_ } |
    Select-Object -First 1

if (-not $exe) {
    Write-Error '找不到 sm-server.exe，先在 backend/ 跑：cargo build -p sm-server'
    exit 1
}

Write-Host "启动 sm-server（$exe）"
Write-Host "  DATABASE_URL      = $env:SAKURAMEDIA_DATABASE_URL"
Write-Host "  监听              = 0.0.0.0:$Port"
Write-Host "  SCHEDULER_ENABLED = $env:SAKURAMEDIA_SCHEDULER_ENABLED"
& $exe
