# HTTP 性能对比编排：依次启动 3 个服务端并施加同一压测器
# 用法： powershell -ExecutionPolicy Bypass -File run-http.ps1 [-Conns 64] [-Reqs 2000] [-Mode get|post] [-BodySize 1024]
param(
    [int]$Conns = 64,
    [int]$Reqs = 2000,
    [ValidateSet("get", "post")][string]$Mode = "get",
    [int]$BodySize = 1024,
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
        [pscustomobject]@{ name = "http_raw";   port = 18081 },
        [pscustomobject]@{ name = "http_hyper"; port = 18082 },
        [pscustomobject]@{ name = "http_axum";  port = 18083 }
    )

    $results = @()
    foreach ($s in $servers) {
        $exe = Join-Path $root "target\release\$($s.name).exe"
        $loadExe = Join-Path $root "target\release\http_load.exe"
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

            $sumRps = 0.0
            $lines = @()
            $procs = @()
            $files = @()
            for ($j = 0; $j -lt $Loaders; $j++) {
                $outFile = Join-Path $root "load-$($s.name)-$j.txt"
                $files += $outFile
                $procs += Start-Process -FilePath $loadExe -ArgumentList @($addr, $Conns, $Reqs, $Mode, $BodySize) -PassThru -RedirectStandardOutput $outFile -WindowStyle Hidden
            }
            foreach ($lp in $procs) { $lp.WaitForExit() }
            foreach ($f in $files) {
                $t = (Get-Content $f -Raw).Trim()
                Write-Host $t -ForegroundColor Green
                $lines += $t
                if ($t -match 'rps=(\d+)') { $sumRps += [double]$Matches[1] }
            }
            if ($Loaders -gt 1) { Write-Host ("aggregate rps = {0:N0}" -f $sumRps) -ForegroundColor Green }
            $results += [pscustomobject]@{ server = $s.name; result = ($lines -join ' | '); agg = $sumRps }
        }
        finally {
            if ($proc -and -not $proc.HasExited) { Stop-Process -Id $proc.Id -Force }
        }
    }

    Write-Host ""
    Write-Host "== SUMMARY ==" -ForegroundColor Yellow
    foreach ($r in $results) {
        Write-Host ("{0,-12} agg={1:N0}  {2}" -f $r.server, $r.agg, $r.result)
    }
}
finally {
    Pop-Location
}
