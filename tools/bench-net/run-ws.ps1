# WebSocket 性能对比编排：依次启动 3 个回显服务端并施加同一压测器
# 用法： powershell -ExecutionPolicy Bypass -File run-ws.ps1 [-Conns 64] [-Msgs 2000] [-PayloadSize 256] [-Loaders 1]
param(
    [int]$Conns = 64,
    [int]$Msgs = 2000,
    [int]$PayloadSize = 256,
    [int]$Loaders = 1
)

$ErrorActionPreference = "Stop"
$root = $PSScriptRoot
Push-Location $root
try {
    Write-Host "== cargo build --release ==" -ForegroundColor Cyan
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }

    $servers = @(
        [pscustomobject]@{ name = "ws_raw";         port = 18091 },
        [pscustomobject]@{ name = "ws_tungstenite"; port = 18092 },
        [pscustomobject]@{ name = "ws_fast";        port = 18093 },
        [pscustomobject]@{ name = "ws_dhrust";      port = 18094 }
    )

    $results = @()
    foreach ($s in $servers) {
        $exe = Join-Path $root "target\release\$($s.name).exe"
        $loadExe = Join-Path $root "target\release\ws_load.exe"
        $addr = "127.0.0.1:$($s.port)"
        Write-Host ""
        Write-Host "== $($s.name) @ $addr ==" -ForegroundColor Cyan
        $proc = Start-Process -FilePath $exe -ArgumentList $addr -PassThru -WindowStyle Hidden
        try {
            $ok = $false
            for ($i = 0; $i -lt 100; $i++) {
                try {
                    $c = New-Object System.Net.Sockets.TcpClient
                    $c.Connect("127.0.0.1", $s.port)
                    $c.Close()
                    $ok = $true
                    break
                }
                catch { Start-Sleep -Milliseconds 100 }
            }
            if (-not $ok) { throw "server $($s.name) not ready" }
            Start-Sleep -Milliseconds 300

            $sumMps = 0.0
            $lines = @()
            $procs = @()
            $files = @()
            for ($j = 0; $j -lt $Loaders; $j++) {
                $outFile = Join-Path $root "load-$($s.name)-$j.txt"
                $files += $outFile
                $procs += Start-Process -FilePath $loadExe -ArgumentList @($addr, $Conns, $Msgs, $PayloadSize) -PassThru -RedirectStandardOutput $outFile -WindowStyle Hidden
            }
            foreach ($lp in $procs) { $lp.WaitForExit() }
            foreach ($f in $files) {
                $t = (Get-Content $f -Raw).Trim()
                Write-Host $t -ForegroundColor Green
                $lines += $t
                if ($t -match 'mps=(\d+)') { $sumMps += [double]$Matches[1] }
            }
            if ($Loaders -gt 1) { Write-Host ("aggregate mps = {0:N0}" -f $sumMps) -ForegroundColor Green }
            $results += [pscustomobject]@{ server = $s.name; result = ($lines -join ' | '); agg = $sumMps }
        }
        finally {
            if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
        }
    }

    Write-Host ""
    Write-Host "== SUMMARY ==" -ForegroundColor Yellow
    foreach ($r in $results) {
        Write-Host ("{0,-16} agg={1:N0}  {2}" -f $r.server, $r.agg, $r.result)
    }
}
finally {
    Pop-Location
}
