# Password interop gate: dhrust::sign::password  <->  C# NewLife.Security.SaltPasswordProvider
#
# Checks:
#   1. Fixed-salt hash equality for md5 / sha1 / sha512 / md5+sha1 / md5+sha512
#   2. C# produces hash  -> Rust verifies
#   3. Rust produces hash -> C# verifies
#   4. Legacy values (C# uppercase MD5, PHP lowercase MD5, plaintext) -> Rust verifies
#
# Usage:
#   powershell -ExecutionPolicy Bypass -File scripts\password-interop.ps1
#   powershell -ExecutionPolicy Bypass -File scripts\password-interop.ps1 -SkipBuild

param(
    [switch]$SkipBuild
)

$ErrorActionPreference = "Stop"
$root = Split-Path -Parent $PSScriptRoot
$csProj = Join-Path $root "tools\csharp\DHRustDemo\DHRustDemo.csproj"
$csExe = Join-Path $root "tools\csharp\DHRustDemo\bin\Debug\net10.0\DHRustDemo.exe"
$rsExe = Join-Path $root "target\debug\examples\password_tool.exe"

$script:Failures = 0
$algorithms = @("md5", "sha1", "sha512", "md5+sha1", "md5+sha512")
$password = "New#life"
$salt = "abCD12./xyZ9012Q"

function Write-Step([string]$text) {
    Write-Host ""
    Write-Host "=== $text ===" -ForegroundColor Cyan
}

function Assert-True([string]$name, [bool]$ok, [string]$detail = "") {
    if ($ok) {
        Write-Host "  [OK] $name"
    }
    else {
        Write-Host "  [FAIL] $name $detail" -ForegroundColor Red
        $script:Failures++
    }
}

function Assert-Equal([string]$name, [string]$expected, [string]$actual) {
    if ($expected -ceq $actual) {
        Write-Host "  [OK] $name"
    }
    else {
        Write-Host "  [FAIL] $name" -ForegroundColor Red
        Write-Host "    expected: $expected"
        Write-Host "    actual:   $actual"
        $script:Failures++
    }
}

if (-not $SkipBuild) {
    Write-Step "Build"
    Push-Location $root
    & cargo build --example password_tool
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    Pop-Location

    & dotnet build $csProj -v q --nologo
    if ($LASTEXITCODE -ne 0) { throw "dotnet build failed" }
}

if (-not (Test-Path $rsExe)) { throw "missing $rsExe (run without -SkipBuild)" }
if (-not (Test-Path $csExe)) { throw "missing $csExe (run without -SkipBuild)" }

Write-Step "1. Fixed-salt hash equality"
foreach ($alg in $algorithms) {
    $csHash = (& $csExe password-hash-fixed $alg $password $salt).Trim()
    $rsHash = (& $rsExe hash-fixed $alg $password $salt).Trim()
    Assert-Equal "$alg hash" $csHash $rsHash
}

Write-Step "2. C# hash -> Rust verify"
foreach ($alg in $algorithms) {
    $hash = (& $csExe password-hash $alg $password).Trim()
    $verdict = (& $rsExe verify-alg $alg $password $hash).Trim()
    Assert-Equal "$alg verify" "OK" $verdict
}

Write-Step "3. Rust hash -> C# verify"
foreach ($alg in $algorithms) {
    $hash = (& $rsExe hash $alg $password).Trim()
    $verdict = (& $csExe password-verify-alg $alg $password $hash).Trim()
    Assert-Equal "$alg verify" "OK" $verdict
}

Write-Step "4. Legacy (no `$ prefix) values"
$csMd5 = (& $csExe password-legacy-md5 $password).Trim()
$phpMd5 = $csMd5.ToLowerInvariant()
Assert-Equal "C# MD5 is uppercase" $csMd5.ToUpperInvariant() $csMd5
Assert-Equal "uppercase MD5 verify" "OK" (& $rsExe verify $password $csMd5).Trim()
Assert-Equal "lowercase MD5 verify" "OK" (& $rsExe verify $password $phpMd5).Trim()
Assert-Equal "plaintext verify" "OK" (& $rsExe verify $password $password).Trim()
Assert-Equal "wrong password rejected" "FAIL" (& $rsExe verify "wrong" $phpMd5).Trim()

Write-Host ""
if ($script:Failures -gt 0) {
    Write-Host "=== PASSWORD INTEROP FAILED (${script:Failures} failures) ===" -ForegroundColor Red
    exit 1
}

Write-Host "=== PASSWORD INTEROP PASSED ===" -ForegroundColor Green
exit 0
