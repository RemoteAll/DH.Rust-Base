# DH.RustBase 与 DH.NCore 互操作校验
#
# 验证：
#   1. 配置互通：C# 写 → Rust 读；Rust 写 → C# 读（XML 与 JSON 两种格式）
#   2. Cron 求值一致：同一表达式与起点时间，双方下一次/前一次执行时间完全相同
#   3. 定时器冒烟：两侧 TimerX / Timer 均能按期触发
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\interop.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\interop.ps1 -SkipBuild

param(
    [string]$WorkDir = "",
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot

$rustExe = Join-Path $root "target\debug\examples\config_interop.exe"
$csDir = Join-Path $root "tools\csharp\DHRustDemo"
$csExe = Join-Path $csDir "bin\Debug\net10.0\DHRustDemo.exe"

if (-not $WorkDir) { $WorkDir = Join-Path $root "target\interop" }
New-Item -ItemType Directory -Force -Path $WorkDir | Out-Null

$script:Failures = 0

function Write-Step([string]$text) {
    Write-Host ""
    Write-Host "=== $text ===" -ForegroundColor Cyan
}

function Assert-Equal([string]$name, [string]$expected, [string]$actual) {
    if ($expected -ne $actual) {
        Write-Host "  [FAIL] $name" -ForegroundColor Red
        Write-Host "    expected: $expected"
        Write-Host "    actual:   $actual"
        $script:Failures++
    }
    else {
        Write-Host "  [OK] $name"
    }
}

function Assert-KeyValues([string]$name, [string]$text, [hashtable]$expected) {
    $map = @{}
    foreach ($line in ($text -split "`r?`n")) {
        if ($line.StartsWith("#") -or $line.Trim().Length -eq 0) { continue }
        $i = $line.IndexOf('=')
        if ($i -gt 0) { $map[$line.Substring(0, $i)] = $line.Substring($i + 1) }
    }

    $ok = $true
    foreach ($key in $expected.Keys) {
        if (-not $map.ContainsKey($key)) {
            Write-Host "  [FAIL] $name 缺少字段 $key" -ForegroundColor Red
            $ok = $false
        }
        elseif ($map[$key] -ne $expected[$key]) {
            Write-Host "  [FAIL] $name $key`: 期望 '$($expected[$key])' 实际 '$($map[$key])'" -ForegroundColor Red
            $ok = $false
        }
    }

    if ($ok) { Write-Host "  [OK] $name（13 个字段）" }
    else { $script:Failures++ }
}

function Invoke-Process([string]$exe, [string[]]$arguments) {
    $out = & $exe @arguments 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "执行失败 [$exe $($arguments -join ' ')] 退出码 $LASTEXITCODE`n$out"
    }
    return (($out | Out-String).Trim())
}

# ---------------------------------------------------------------- 构建

if (-not $SkipBuild) {
    Write-Step "构建 Rust 示例"
    Push-Location $root
    try {
        & cargo build --example config_interop
        if ($LASTEXITCODE -ne 0) { throw "cargo build 失败" }
    }
    finally { Pop-Location }

    Write-Step "构建 C# 工具（引用 DH.NCore 源码工程）"
    & dotnet build $csDir -v quiet -nologo
    if ($LASTEXITCODE -ne 0) { throw "dotnet build 失败" }
}

foreach ($exe in @($rustExe, $csExe)) {
    if (-not (Test-Path $exe)) { throw "缺少可执行文件：$exe（先不加 -SkipBuild 运行一次）" }
}

# ---------------------------------------------------------------- 固定样例

$expected = @{
    Debug            = "false"
    LogLevel         = "Warn"
    LogPath          = "Logs"
    LogFileMaxBytes  = "20"
    LogFileBackups   = "5"
    LogFileFormat    = "{0:yyyy_MM_dd}.log"
    LogLineFormat    = "Time|ThreadId|Kind|Name|Message"
    NetworkLog       = "udp://127.0.0.1:5514"
    DataPath         = "DataDir"
    BackupPath       = "BackupDir"
    PluginPath       = "PluginDir"
    PluginServer     = "http://plugins.example/"
    ServiceAddress   = "http://localhost:8080"
}

# ---------------------------------------------------------------- 配置互通

Write-Step "XML 配置互通（Config/Core.config）"

$csXml = Join-Path $WorkDir "csharp_core.config"
$null = Invoke-Process $csExe @("write-setting", $csXml)
$text = Invoke-Process $rustExe @("read-setting", $csXml)
Assert-KeyValues "C# 写 → Rust 读" $text $expected

$rustXml = Join-Path $WorkDir "rust_core.config"
$null = Invoke-Process $rustExe @("write-setting", $rustXml)
$text = Invoke-Process $csExe @("read-setting", $rustXml)
Assert-KeyValues "Rust 写 → C# 读" $text $expected

Write-Step "JSON 配置互通（Config/Core.json）"

$csJson = Join-Path $WorkDir "csharp_core.json"
$null = Invoke-Process $csExe @("write-setting", $csJson)
$text = Invoke-Process $rustExe @("read-setting", $csJson)
Assert-KeyValues "C# 写 → Rust 读（JSON）" $text $expected

$rustJson = Join-Path $WorkDir "rust_core.json"
$null = Invoke-Process $rustExe @("write-setting", $rustJson)
$text = Invoke-Process $csExe @("read-setting", $rustJson)
Assert-KeyValues "Rust 写 → C# 读（JSON）" $text $expected

# ---------------------------------------------------------------- Cron 求值

Write-Step "Cron 求值一致"

$cronCases = @(
    @("*/2", "2026-09-28 10:00:00"),
    @("5/20 * * * *", "2026-09-28 10:00:00"),
    @("5/20 * * * *", "2026-09-28 10:00:04"),
    @("0 0 0,12 * * *", "2026-09-28 10:00:00"),
    @("0 0 0 1 * *", "2010-08-01 00:00:00"),
    @("0 0 0 1 */3 *", "2010-08-01 00:00:00"),
    @("0 0 0 ? ? 3#2", "2023-03-01 00:00:00"),
    @("0 0 0 ? ? 3-5#L2", "2023-03-01 00:00:00"),
    @("0 0 0 ? ? 0-6#1", "2023-03-01 00:00:00"),
    @("0 0 16 * * ?", "2026-09-28 10:00:00"),
    @("* 1-10,13,5/20 * * *", "2026-09-28 10:00:00")
)

foreach ($case in $cronCases) {
    $expr = $case[0]
    $time = $case[1]

    $csNext = Invoke-Process $csExe @("cron-next", $expr, $time)
    $rsNext = Invoke-Process $rustExe @("cron-next", $expr, $time)
    Assert-Equal "cron-next [$expr] @ $time" $csNext $rsNext

    $csPrev = Invoke-Process $csExe @("cron-prev", $expr, $time)
    $rsPrev = Invoke-Process $rustExe @("cron-prev", $expr, $time)
    Assert-Equal "cron-prev [$expr] @ $time" $csPrev $rsPrev
}

# ---------------------------------------------------------------- 定时器冒烟

Write-Step "定时器冒烟（TimerX / Timer）"

$csTimer = Invoke-Process $csExe @("timer-demo")
$rsTimer = Invoke-Process $rustExe @("timer-demo")
Assert-Equal "C# TimerX 触发" "OK" ($csTimer.Split(' ')[0])
Assert-Equal "Rust Timer 触发" "OK" ($rsTimer.Split(' ')[0])
Write-Host "  C#: $csTimer"
Write-Host "  Rust: $rsTimer"

# ---------------------------------------------------------------- 汇总

Write-Step "结果"
if ($script:Failures -eq 0) {
    Write-Host "=== INTEROP PASSED ===" -ForegroundColor Green
    exit 0
}
else {
    Write-Host "=== INTEROP FAILED: $($script:Failures) 项 ===" -ForegroundColor Red
    exit 1
}
