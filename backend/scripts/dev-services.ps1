# 本地开发依赖（PostgreSQL / Qdrant）的起停与体检。
#
# 【2026-10-10：实现已从 Windows podman 容器迁到 WSL Alpine 原生二进制】
#
# 本文件现在是**薄转发**：真正的逻辑全在 Alpine 的 /root/*.sh。动作名
# （up / down / purge / doctor）刻意保持不变，所以 run-server.ps1、verify.ps1
# 与文档里的引用都不用改。
#
# 拓扑：
#   PostgreSQL  127.0.0.1:5433   apk 包 postgresql16（musl），数据 /var/lib/postgresql/data
#   Qdrant      127.0.0.1:6333 REST / 6334 gRPC   官方 musl 静态二进制
#
# 对应的 Alpine 脚本：
#   deps-up.sh      起 PG + Qdrant（含 fsync 调优、等就绪）
#   deps-down.sh    停 PG + Qdrant（保留数据）
#   deps-purge.sh   删数据目录后按 docker/schema.sql 重建（含种子用户 netdev）
#   dev-doctor.sh   体检（PG 是否在跑、fsync 是否已关、Qdrant 是否在跑）
#   pg-tuning.sh    关持久化（见下）
#
# # 为什么还要关持久化
#
# 591 个测试函数各建一次 40 张表再删掉，持久化开着就是几十万次 fsync。实测
# collection_integration 从 8.41s 变成 1.12s，activity_cleanup 从 11.2s 变成
# 0.68s。这项用 ALTER SYSTEM 写进数据目录，purge 重建后会由 deps-up /
# pg-setup 再打一遍，所以 doctor 每次都查。
#
# 关持久化是**这个可丢弃库**的正当设置，不是忘了打的生产配置。
# 生产库永远不要这么设。
#
# # 用法
#
#   powershell -File scripts/dev-services.ps1 up       # 起（已在跑就跳过）
#   powershell -File scripts/dev-services.ps1 doctor   # 只体检
#   powershell -File scripts/dev-services.ps1 down     # 停（保留数据）
#   powershell -File scripts/dev-services.ps1 purge    # 删数据（PG + Qdrant 全没，重建）

param(
    [ValidateSet('up', 'down', 'purge', 'doctor')]
    [string]$Action = 'doctor'
)

$ErrorActionPreference = 'Continue'
# Test-NetConnection 会给每个端口刷好几行进度条，压掉。
$ProgressPreference = 'SilentlyContinue'

$Distro = 'Alpine'

function Invoke-Wsl([string]$Script) {
    wsl -d $Distro -u root -- bash -lc $Script
}

function Invoke-Doctor {
    # Alpine 侧：PG / Qdrant 是否在跑 + fsync 是否已关（SLOW 提示）。
    # 退出码 1 表示 PG 不可用。
    Invoke-Wsl '/root/dev-doctor.sh'
    $broken = $LASTEXITCODE

    # Windows 侧：端口从宿主看是否可达。容器时代这一步能抓到「容器 Running
    # 但 podman machine 重启后端口转发断了」；换成原生实例后同样值得确认 ——
    # Alpine 被 WSL 回收时，里面三个进程会一起没。
    foreach ($port in 5433, 6334) {
        $ok = (Test-NetConnection -ComputerName 127.0.0.1 -Port $port -WarningAction SilentlyContinue).TcpTestSucceeded
        if (-not $ok) {
            Write-Host "  BROKEN   Windows 侧连不上 127.0.0.1:$port"
            $broken = 1
        }
    }
    if ($broken -ne 0) { exit 1 }
}

switch ($Action) {
    'up'     { Invoke-Wsl '/root/deps-up.sh' }
    'down'   { Invoke-Wsl '/root/deps-down.sh' }
    'purge'  { Invoke-Wsl '/root/deps-purge.sh' }
    'doctor' { Invoke-Doctor }
}
