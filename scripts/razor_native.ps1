# Razor F014 原生编译校验：8 个互操作用例 → 生成 Rust → 编译 dll → 渲染 → 与期望逐字节比对
#
# 用法：
#   powershell -ExecutionPolicy Bypass -File scripts\razor_native.ps1 [-SkipBuild]

param([switch]$SkipBuild)

# 注意：PowerShell 5.1 会把原生命令（cargo）的 stderr 视为错误记录，
# 因此不使用 Stop 策略；所有失败均通过显式检查 $LASTEXITCODE / throw 处理。
$ErrorActionPreference = "Continue"
$root = Split-Path -Parent $PSScriptRoot

$tool = Join-Path $root "tools\razor-native\target\release\razor-native.exe"
$casesDir = Join-Path $root "tests\razor_cases"
$outDir = Join-Path $root "target\native\cases"
New-Item -ItemType Directory -Force -Path $outDir | Out-Null

function Write-Step([string]$text) {
    Write-Host ""
    Write-Host "=== $text ===" -ForegroundColor Cyan
}

if (-not $SkipBuild) {
    Write-Step "构建 razor-native 工具（Release）"
    & cargo build --release --manifest-path (Join-Path $root "tools\razor-native\Cargo.toml") 2>&1 | Select-Object -Last 1 | ForEach-Object { Write-Host "  $_" }
    if ($LASTEXITCODE -ne 0) { throw "工具构建失败" }
}
if (-not (Test-Path $tool)) { throw "缺少工具：$tool（请去掉 -SkipBuild 重新构建）" }

Write-Step "逐用例：原生编译 + 渲染 + 比对"
$cases = Get-ChildItem $casesDir -Directory | Sort-Object Name
$pass = 0
$fail = 0
foreach ($c in $cases) {
    $tpl = Join-Path $c.FullName "template.cshtml"
    $data = Join-Path $c.FullName "data.json"
    $expected = Join-Path $c.FullName "expected.html"
    $dll = Join-Path $outDir "$($c.Name).dll"
    $actual = Join-Path $outDir "$($c.Name).native.html"

    & $tool compile $tpl --name "case_$($c.Name)" -o $dll 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "  [FAIL] $($c.Name)（编译失败）" -ForegroundColor Red
        $fail++
        continue
    }
    & $tool render $dll $data -o $actual 2>&1 | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Host "  [FAIL] $($c.Name)（渲染失败）" -ForegroundColor Red
        $fail++
        continue
    }
    $a = [System.IO.File]::ReadAllBytes($actual)
    $e = [System.IO.File]::ReadAllBytes($expected)
    $same = $a.Length -eq $e.Length
    if ($same) {
        for ($i = 0; $i -lt $a.Length; $i++) {
            if ($a[$i] -ne $e[$i]) { $same = $false; break }
        }
    }
    if ($same) {
        Write-Host "  [OK] $($c.Name)（$($e.Length)B）" -ForegroundColor Green
        $pass++
    } else {
        Write-Host "  [FAIL] $($c.Name)（原生=$($a.Length)B / 期望=$($e.Length)B）" -ForegroundColor Red
        $fail++
    }
}

Write-Host ""
if ($fail -eq 0) {
    Write-Host "RAZOR NATIVE PASSED（$pass/$($cases.Count)）" -ForegroundColor Green
    exit 0
}
Write-Host "RAZOR NATIVE FAILED（$fail 个不一致）" -ForegroundColor Red
exit 1
