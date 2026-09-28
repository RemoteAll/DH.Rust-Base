# Razor 子集引擎双端互操作校验（F011）
#
# 遍历 tests/razor_cases，Rust 引擎（examples/razor_render）与 C# 原生 Razor
# （tools/csharp/RazorInterop，RazorEngineCore 免宿主编译 + HtmlEncoder.Default）
# 双端渲染，逐字节比对；全部一致时输出 RAZOR INTEROP PASSED。
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\razor_interop.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\razor_interop.ps1 -SkipBuild

param(
    [switch]$SkipBuild
)

# 注意：PowerShell 5.1 会把原生命令（cargo/dotnet）的 stderr 视为错误记录，
# 因此不使用 Stop 策略；所有失败均通过显式检查 $LASTEXITCODE 处理。
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot

$rustExe = Join-Path $root "target\debug\examples\razor_render.exe"
$csDir = Join-Path $root "tools\csharp\RazorInterop"
$csExe = Join-Path $csDir "bin\Debug\net10.0\RazorInterop.exe"
$casesDir = Join-Path $root "tests\razor_cases"
$workDir = Join-Path $root "target\interop\razor"

$script:Failures = 0

function Write-Step([string]$text) {
    Write-Host ""
    Write-Host "=== $text ===" -ForegroundColor Cyan
}

function Write-Fail([string]$name, [string]$detail) {
    Write-Host "  [FAIL] $name" -ForegroundColor Red
    if ($detail) { Write-Host "    $detail" }
    $script:Failures++
}

function Compare-Bytes([string]$name, [string]$p1, [string]$p2) {
    $a = [System.IO.File]::ReadAllBytes($p1)
    $b = [System.IO.File]::ReadAllBytes($p2)
    if ($a.Length -eq $b.Length) {
        $same = $true
        for ($i = 0; $i -lt $a.Length; $i++) {
            if ($a[$i] -ne $b[$i]) { $same = $false; break }
        }
        if ($same) {
            Write-Host "  [OK] $name（$($a.Length)B）"
            return
        }
    }

    $ta = [System.Text.Encoding]::UTF8.GetString($a)
    $tb = [System.Text.Encoding]::UTF8.GetString($b)
    $detail = "字节数：Rust=$($a.Length) C#=$($b.Length)"
    $min = [Math]::Min($ta.Length, $tb.Length)
    for ($i = 0; $i -lt $min; $i++) {
        if ($ta[$i] -ne $tb[$i]) {
            $from = [Math]::Max(0, $i - 16)
            $len = [Math]::Min(48, $min - $from)
            $detail += "`n      首个差异 @字符 $i"
            $detail += "`n        Rust: $($ta.Substring($from, $len))"
            $detail += "`n        C#  : $($tb.Substring($from, $len))"
            break
        }
    }
    Write-Fail $name $detail
}

if (-not $SkipBuild) {
    Write-Step "构建 Rust 渲染示例（cargo build --features razor --example razor_render）"
    Push-Location $root
    & cargo build --features razor --example razor_render 2>&1 | Select-Object -Last 3 | ForEach-Object { Write-Host "  $_" }
    $rustExit = $LASTEXITCODE
    Pop-Location
    if ($rustExit -ne 0) { throw "Rust 示例构建失败（exit $rustExit）" }

    Write-Step "构建 C# RazorInterop（dotnet build）"
    & dotnet build $csDir --nologo 2>&1 | Select-Object -Last 5 | ForEach-Object { Write-Host "  $_" }
    if ($LASTEXITCODE -ne 0) { throw "C# 工具构建失败（exit $LASTEXITCODE）" }
}

foreach ($exe in @($rustExe, $csExe)) {
    if (-not (Test-Path $exe)) { throw "缺少可执行文件：$exe（请去掉 -SkipBuild 重新构建）" }
}

New-Item -ItemType Directory -Force -Path $workDir | Out-Null

$cases = @(Get-ChildItem $casesDir -Directory | Sort-Object Name)
Write-Step "双端渲染比对（$($cases.Count) 个用例）"

foreach ($case in $cases) {
    $tpl = Join-Path $case.FullName "template.cshtml"
    $data = Join-Path $case.FullName "data.json"
    $rustOut = Join-Path $workDir "$($case.Name).rust.html"
    $csOut = Join-Path $workDir "$($case.Name).cs.html"

    & $rustExe $tpl $data -o $rustOut
    if ($LASTEXITCODE -ne 0) {
        Write-Fail $case.Name "Rust 渲染失败（exit $LASTEXITCODE）"
        continue
    }

    & $csExe render $tpl $data -o $csOut
    if ($LASTEXITCODE -ne 0) {
        Write-Fail $case.Name "C# 渲染失败（exit $LASTEXITCODE）"
        continue
    }

    Compare-Bytes $case.Name $rustOut $csOut
}

Write-Step "汇总"
if ($script:Failures -eq 0) {
    Write-Host "RAZOR INTEROP PASSED（$($cases.Count)/$($cases.Count)）" -ForegroundColor Green
    exit 0
}

Write-Host "$($script:Failures) 项失败（详见上方 [FAIL]）" -ForegroundColor Red
exit 1
