# memoria 一键重启脚本 — 始终用这个重启，不要手动杀进程
# 正确设置 MEMORIA_DB_PATH 等环境变量，避免看护器死后失忆
# ASCII only (PS5.1 + UTF8-no-BOM trap)

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
