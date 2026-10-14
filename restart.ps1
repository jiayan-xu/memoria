# memoria 一键重启脚本 — 始终用这个重启，不要手动杀进程
# 正确设置 MEMORIA_DB_PATH 等环境变量，避免看护器死后失忆
# ASCII only (PS5.1 + UTF8-no-BOM trap)

param(
    # 逃生舱：嵌入服务计划内维护时用它跳过探活（会打印醒目告警）
    [switch]$SkipEmbedCheck
)

$exe = "C:\services\memoria-open\target\release\memoria-server.exe"
$cwd = "C:\services\memoria-open"

# kill existing
Get-Process -Name memoria-server -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2

# set env from secrets
$envFile = "$env:USERPROFILE\.svc-secrets\agent-core.env"
if (Test-Path $envFile) {
    Get-Content $envFile -Encoding UTF8 | ForEach-Object {
        $l = $_.Trim()
        if ($l -and -not $l.StartsWith('#') -and $l.Contains('=')) {
            $eq = $l.IndexOf('='); $k = $l.Substring(0, $eq).Trim(); $v = $l.Substring($eq + 1).Trim()
            Set-Item -Path ("env:" + $k) -Value $v
        }
    }
}

# always set the correct DB path (2026-10-04: 与托盘看门狗 start_both_tray.ps1 的
# Start-Svc("mem") 对齐——服务实际运行于 C:\services\memoria-open，DB 在 C:\services\memoria\data)
$env:MEMORIA_DB_PATH = "C:\services\memoria\data\memoria.db"

# 2026-10-13: embed 端口 8778 必须显式注入（exe 默认 8777 是死端口，10-02 事故同款坑）；
# rerank 显式关闭（10-02 实测裁决；默认虽 off，但实验启动器 start_memoria_* 会开它）
$env:MEMORIA_EMBEDDING_URL = "http://127.0.0.1:8778/embed"
$env:MEMORIA_RERANK_ENABLED = "0"

# ── 启动自检（R0）：起服务前先确认语义通道真的在线 ────────────────────────
# 背景：10-02 / 10-13 两次事故是同一个模式——服务起来了、health 也 200，
# 但语义通道是死的：exe 内嵌默认 MEMORIA_EMBEDDING_URL 指向 :8777（死端口），
# 实验启动器 start_memoria_* 又普遍开着 RERANK_ENABLED=1 且指向 :8777/rerank。
# 只靠默认值或残留环境启动，服务照样能起、照样能探活，语义检索却静默失效，
# 而且没有日志——所以把断言前移：**不满足就拒启**（宁可起不来，不要带病上线）。
# 另：启动时打印生效配置，事后归因「当时到底带了什么环境」不再靠猜。
$embUrl = $env:MEMORIA_EMBEDDING_URL
$rerank = $env:MEMORIA_RERANK_ENABLED
Write-Host "[restart] 生效配置: EMBEDDING_URL=$embUrl / RERANK_ENABLED=$rerank / DB=$env:MEMORIA_DB_PATH"

$canStart = $true
if (-not $SkipEmbedCheck) {
    if ([string]::IsNullOrWhiteSpace($embUrl)) {
        Write-Host "[restart] REFUSE: MEMORIA_EMBEDDING_URL 为空（语义通道未配置）"
        $canStart = $false
    } else {
        # /embed -> /health 探活（embed_local_server.py 提供 /health）
        $healthUrl = $embUrl -replace '/embed\s*$', '/health'
        $embOk = $false
        try {
            $r = Invoke-WebRequest -Uri $healthUrl -UseBasicParsing -TimeoutSec 5
            if ($r.StatusCode -eq 200) { $embOk = $true }
            else { Write-Host "[restart] embed 探活返回 HTTP $($r.StatusCode): $healthUrl" }
        } catch {
            Write-Host "[restart] embed 探活失败: $healthUrl"
        }
        if (-not $embOk) {
            Write-Host "[restart] REFUSE: 嵌入服务不可达——先拉起 embed_local_server.py (:8778)"
            $canStart = $false
        }
    }
} else {
    Write-Host "[restart] !! SkipEmbedCheck 已启用：跳过嵌入服务探活（仅限计划内维护）"
}
if ($rerank -eq "1") {
    Write-Host "[restart] WARN: RERANK_ENABLED=1（实验配置），生产应为 0 —— 确认是有意为之"
}
if (-not $canStart) {
    Write-Host "[restart] 启动中止：修正环境后重跑本脚本。"
    exit 2
}

Start-Process -FilePath $exe -WorkingDirectory $cwd -WindowStyle Hidden

# wait for health
$ok = $false
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep -Seconds 10
    try {
        $r = Invoke-WebRequest -Uri "http://127.0.0.1:9003/health" -UseBasicParsing -TimeoutSec 3
        if ($r.StatusCode -eq 200) { $ok = $true; break }
    } catch { }
}
if ($ok) {
    Write-Host "memoria UP (took $(($i + 1) * 10)s)"
} else {
    Write-Host "memoria FAILED to start within 300s"
}
