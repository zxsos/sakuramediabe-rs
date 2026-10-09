# 本地开发依赖（PostgreSQL / Qdrant）的起停与体检。
#
# # 为什么需要这个脚本
#
# 容器是逐个 podman run 手搓出来的，参数散在对话记录里。其中两条是
# **性能关键**的，漏掉就会让全量测试慢一个数量级：
#
# | 参数 | 漏掉的后果 |
# |---|---|
# | --network=host | netavark 要在 WSL2 里建 nftables 规则，失败 -> 容器起不来 |
# | fsync=off + synchronous_commit=off | 每次 COMMIT 强制刷盘 -> 集成测试慢 7-16 倍 |
#
# 第二条的实测：collection_integration 从 8.41s 变成 1.12s，
# activity_cleanup 从 11.2s 变成 0.68s。591 个测试函数各建一次 40 张表的
# schema 再删掉，持久化开着就是几十万次 fsync。
#
# 这两项是 ALTER SYSTEM 写进数据目录的，容器重启还在，**但卷被重建就没了**。
# 所以 doctor 每次都查一遍。
#
# 关持久化是**这个可丢弃库**的正当设置，不是忘了打的生产配置。
# 生产库永远不要这么设。
#
# # 用法
#
#   powershell -File scripts/dev-services.ps1 up       # 起（已存在就跳过）
#   powershell -File scripts/dev-services.ps1 doctor   # 只体检
#   powershell -File scripts/dev-services.ps1 down     # 停（保留卷）
#   powershell -File scripts/dev-services.ps1 purge    # 删容器与卷（数据全没）

param(
    [ValidateSet('up', 'down', 'purge', 'doctor')]
    [string]$Action = 'doctor'
)

$ErrorActionPreference = 'Continue'

$PgContainer = 'sakuramedia-rs-pg'
$QdrantContainer = 'sakuramedia-rs-qdrant'
$PgVolume = 'sakuramedia-rs-pgdata'
$QdrantVolume = 'sakuramedia-rs-qdrantdata'

# 时区与 max_locks_per_transaction 抄自仓库 docker-compose.yml：后者不抬的话，
# 并发跑集成测试会偶发 53200 out of shared memory。
$PgEnv = @(
    '-e', 'POSTGRES_USER=sakuramedia',
    '-e', 'POSTGRES_PASSWORD=sakuramedia',
    '-e', 'POSTGRES_DB=sakuramedia_test',
    '-e', 'TZ=UTC', '-e', 'PGTZ=UTC',
    '-v', "${PgVolume}:/var/lib/postgresql/data"
)
$PgCommand = @('postgres', '-c', 'timezone=UTC', '-c', 'max_locks_per_transaction=1024')

function Test-Running([string]$Name) {
    return (podman inspect -f '{{.State.Running}}' $Name 2>$null) -eq 'true'
}

function Test-Exists([string]$Name) {
    podman inspect $Name *> $null
    return $LASTEXITCODE -eq 0
}

function Write-Durability {
    foreach ($statement in @(
        'ALTER SYSTEM SET fsync = off',
        'ALTER SYSTEM SET synchronous_commit = off',
        'ALTER SYSTEM SET full_page_writes = off',
        "ALTER SYSTEM SET checkpoint_timeout = '30min'")) {
        podman exec $PgContainer psql -h 127.0.0.1 -p 5433 -U sakuramedia -d sakuramedia_test -tAc $statement *> $null
    }
    podman exec $PgContainer psql -h 127.0.0.1 -p 5433 -U sakuramedia -d sakuramedia_test -tAc 'select pg_reload_conf()' *> $null
}

function Invoke-Up {
    if (-not (Test-Exists $PgContainer)) {
        Write-Host '创建 PostgreSQL...'
        podman run -d --name $PgContainer --network=host @PgEnv postgres:16-alpine @PgCommand *> $null
    }
    elseif (-not (Test-Running $PgContainer)) {
        Write-Host '启动 PostgreSQL...'
        podman start $PgContainer *> $null
    }
    for ($i = 0; $i -lt 40; $i++) {
        Start-Sleep -Seconds 2
        if (podman exec $PgContainer pg_isready -h 127.0.0.1 -p 5433 -U sakuramedia 2>$null) { break }
    }
    Write-Durability

    if (-not (Test-Exists $QdrantContainer)) {
        Write-Host '创建 Qdrant...'
        podman run -d --name $QdrantContainer --network=host `
            -v "${QdrantVolume}:/qdrant/storage" qdrant/qdrant:latest *> $null
    }
    elseif (-not (Test-Running $QdrantContainer)) {
        Write-Host '启动 Qdrant...'
        podman start $QdrantContainer *> $null
    }
    Invoke-Doctor
}

function Invoke-Doctor {
    $broken = 0
    if (-not (Test-Exists $PgContainer)) {
        Write-Host "  MISSING  $PgContainer（跑 'up'）"; $broken++
    }
    elseif (-not (Test-Running $PgContainer)) {
        Write-Host "  STOPPED  $PgContainer"; $broken++
    }
    else {
        $fsync = podman exec $PgContainer psql -h 127.0.0.1 -p 5433 -U sakuramedia -d sakuramedia_test -tAc 'show fsync' 2>$null
        if ($fsync -eq 'on') {
            Write-Host '  SLOW     fsync=on（集成测试慢 7-16 倍，跑 up 修）'
        }
        else {
            Write-Host '  OK       PostgreSQL 就绪，持久化已关闭'
        }
    }

    if (-not (Test-Exists $QdrantContainer)) {
        Write-Host "  MISSING  $QdrantContainer（图搜与向量库测试会 SKIP）"
    }
    elseif (-not (Test-Running $QdrantContainer)) {
        Write-Host "  STOPPED  $QdrantContainer"
    }
    else {
        Write-Host '  OK       Qdrant 就绪'
    }

    # 端口从 Windows 侧看，而不是容器内部 —— 容器里能连不代表 WSL2 端口
    # 转发还活着（podman machine 重启会断，而容器本身仍是 Running）。
    if (-not (Test-NetConnection -ComputerName 127.0.0.1 -Port 5433 -WarningAction SilentlyContinue).TcpTestSucceeded) {
        Write-Host '  BROKEN   Windows 侧连不上 127.0.0.1:5433（podman machine 可能重启过，跑 up）'
        $broken++
    }
    if (-not (Test-NetConnection -ComputerName 127.0.0.1 -Port 6334 -WarningAction SilentlyContinue).TcpTestSucceeded) {
        Write-Host '  BROKEN   Windows 侧连不上 127.0.0.1:6334'
        $broken++
    }
    if ($broken -gt 0) { exit 1 }
}

switch ($Action) {
    'up' { Invoke-Up }
    'doctor' { Invoke-Doctor }
    'down' {
        podman stop $PgContainer $QdrantContainer *> $null
        Write-Host '已停（卷保留）'
    }
    'purge' {
        podman rm -f $PgContainer $QdrantContainer *> $null
        podman volume rm -f $PgVolume $QdrantVolume *> $null
        Write-Host '已删容器与卷，数据全没。下次 up 会重建并重新关掉持久化。'
    }
}