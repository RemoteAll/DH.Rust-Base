# Razor 视图渲染基准（Rust 引擎 vs C# 原生 Razor，编译缓存同口径）
#
# 流程：Release 构建两端 → 夹具双端逐字节一致性 sanity → 各自跑基准 → 汇总对比。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\bench_view.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\bench_view.ps1 -Iterations 50000 -SkipBuild

param(
    [int]$Iterations = 20000,
    [int]$Warmup = 1000,
    [switch]$SkipBuild
)

# 注意：PowerShell 5.1 会把原生命令（cargo/dotnet）的 stderr 视为错误记录，
# 因此不使用 Stop 策略；所有失败均通过显式检查 $LASTEXITCODE / throw 处理。
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot

$fixtureDir = Join-Path $root "tools\bench-view\fixtures"
$tpl = Join-Path $fixtureDir "template.cshtml"
$data = Join-Path $fixtureDir "data.json"

$rustExample = Join-Path $root "target\release\examples\razor_render.exe"
$rustBench = Join-Path $root "tools\bench-view\target\release\bench-view.exe"
$csDir = Join-Path $root "tools\csharp\RazorInterop"
$csExe = Join-Path $csDir "bin\Release\net10.0\RazorInterop.exe"
$workDir = Join-Path $root "target\interop\bench"
New-Item -ItemType Directory -Force -Path $workDir | Out-Null

function Write-Step([string]$text) {
    Write-Host ""
    Write-Host "=== $text ===" -ForegroundColor Cyan
}

function Get-Metric($lines, [string]$key) {
    $line = $lines | Where-Object { $_ -like "$key=*" } | Select-Object -First 1
    if (-not $line) { return [double]::NaN }
    return [double]($line.Substring($key.Length + 1))
}

if (-not $SkipBuild) {
    Write-Step "构建 Rust（release：示例 + bench-view）"
    & cargo build --release --features razor --example razor_render --manifest-path (Join-Path $root "Cargo.toml") 2>&1 | Select-Object -Last 2 | ForEach-Object { Write-Host "  $_" }
    if ($LASTEXITCODE -ne 0) { throw "Rust 示例构建失败" }
    & cargo build --release --manifest-path (Join-Path $root "tools\bench-view\Cargo.toml") 2>&1 | Select-Object -Last 2 | ForEach-Object { Write-Host "  $_" }
    if ($LASTEXITCODE -ne 0) { throw "bench-view 构建失败" }

    Write-Step "构建 C# RazorInterop（Release）"
    & dotnet build $csDir -c Release --nologo 2>&1 | Select-Object -Last 3 | ForEach-Object { Write-Host "  $_" }
    if ($LASTEXITCODE -ne 0) { throw "C# 工具构建失败" }
}

foreach ($exe in @($rustExample, $rustBench, $csExe)) {
    if (-not (Test-Path $exe)) { throw "缺少可执行文件：$exe（请去掉 -SkipBuild 重新构建）" }
}

Write-Step "一致性 sanity（双端渲染夹具并比对字节）"
$rustOut = Join-Path $workDir "sanity.rust.html"
$csOut = Join-Path $workDir "sanity.cs.html"
& $rustExample $tpl $data -o $rustOut
if ($LASTEXITCODE -ne 0) { throw "Rust 渲染失败（exit $LASTEXITCODE）" }
& $csExe render $tpl $data -o $csOut
if ($LASTEXITCODE -ne 0) { throw "C# 渲染失败（exit $LASTEXITCODE）" }
$a = [System.IO.File]::ReadAllBytes($rustOut)
$b = [System.IO.File]::ReadAllBytes($csOut)
$same = $a.Length -eq $b.Length
if ($same) {
    for ($i = 0; $i -lt $a.Length; $i++) {
        if ($a[$i] -ne $b[$i]) { $same = $false; break }
    }
}
if (-not $same) {
    throw "夹具双端渲染不一致（Rust=$($a.Length)B / C#=$($b.Length)B），请先修复引擎差异"
}
Write-Host "  [OK] 双端输出一致（$($a.Length) 字节）"

Write-Step "Rust 引擎基准（iterations=$Iterations, warmup=$Warmup，Release）"
$rustLines = & $rustBench $tpl $data $Iterations $Warmup
$rustExit = $LASTEXITCODE
$rustLines | ForEach-Object { Write-Host "  $_" }
if ($rustExit -ne 0) { throw "Rust 基准失败（exit $rustExit）" }

Write-Step "C# Razor 基准（同模板同数据同口径）"
$csLines = & $csExe bench $tpl $data $Iterations $Warmup
$csExit = $LASTEXITCODE
$csLines | ForEach-Object { Write-Host "  $_" }
if ($csExit -ne 0) { throw "C# 基准失败（exit $csExit）" }

Write-Step "汇总"
$rOps = Get-Metric $rustLines "ops_per_sec"
$cOps = Get-Metric $csLines "ops_per_sec"
$rP50 = Get-Metric $rustLines "p50_us"
$cP50 = Get-Metric $csLines "p50_us"
$rP90 = Get-Metric $rustLines "p90_us"
$cP90 = Get-Metric $csLines "p90_us"
$rP99 = Get-Metric $rustLines "p99_us"
$cP99 = Get-Metric $csLines "p99_us"

"{0,-12} {1,14} {2,14}" -f "指标", "Rust", "C#" | Write-Host
"{0,-12} {1,14:N0} {2,14:N0}" -f "ops/sec", $rOps, $cOps | Write-Host
"{0,-12} {1,14:N2} {2,14:N2}" -f "p50(us)", $rP50, $cP50 | Write-Host
"{0,-12} {1,14:N2} {2,14:N2}" -f "p90(us)", $rP90, $cP90 | Write-Host
"{0,-12} {1,14:N2} {2,14:N2}" -f "p99(us)", $rP99, $cP99 | Write-Host

$ratio = $rOps / $cOps
Write-Host ""
Write-Host ("Rust/C# 吞吐比 = {0:N2}x" -f $ratio) -ForegroundColor Cyan
if ($rOps -ge $cOps) {
    Write-Host "VIEW BENCH PASSED（Rust 吞吐 >= C# 同口径）" -ForegroundColor Green
    exit 0
}
Write-Host "VIEW BENCH FAILED（Rust 吞吐低于 C#，需按 F012 差距分析 / F014 评估）" -ForegroundColor Red
exit 1
