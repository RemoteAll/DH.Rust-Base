<#
版本升号守卫（打包脚本共用；2026-10-09 建立，解决「同一版本号打包出不同内容」问题）。

用途：打包发布前调用。把「打包内容指纹」（源码/资源文件哈希汇总）与版本号一起记录到
dist/.pack-guard.json；若同一版本号再次打包且内容指纹已变化 → 默认拒绝并提示升版本号。

  - 新版本（未记录）        → 放行并记录指纹；
  - 同版本 + 指纹未变化     → 放行（重复打包同内容，允许）；
  - 同版本 + 指纹已变化     → 拒绝（throw），除非 -Force；
  - -Bump / -BumpMinor      → 打包前自动递升版本（写 Cargo.toml + cargo check 更新 Cargo.lock）。

用法（各项目 build-release.ps1 中，$root 为本仓库根）：
  $depPath = (Select-String -Path (Join-Path $root 'Cargo.toml') `
              -Pattern 'dhrust\s*=\s*\{\s*path\s*=\s*"([^"]+)"').Matches[0].Groups[1].Value
  . (Join-Path (Resolve-Path (Join-Path $root $depPath)).Path 'tools\version-guard.ps1')
  Assert-VersionGuard -RepoRoot $root -Name 'xxx' -Force:$Force -Bump:$Bump -BumpMinor:$BumpMinor
#>

# 计算打包内容指纹：白名单目录（存在的才纳入）内全部文件，按相对路径排序后逐文件 SHA-256 汇总
function Get-SourceFingerprint {
    param([Parameter(Mandatory)][string]$RepoRoot)

    $rootFull = $RepoRoot.TrimEnd('\', '/')
    $include = @('Cargo.toml', 'Cargo.lock', 'src', 'web', 'res', 'Entity', 'Views', 'plugins-src', 'third_party')
    $files = @()
    foreach ($item in $include) {
        $p = Join-Path $rootFull $item
        if (Test-Path $p) {
            $f = Get-Item $p
            if ($f.PSIsContainer) {
                $files += Get-ChildItem $p -Recurse -File -ErrorAction SilentlyContinue |
                          Where-Object { $_.FullName -notmatch '\\(target|dist|node_modules|\.git)\\' }
            }
            else { $files += $f }
        }
    }

    $sb = New-Object System.Text.StringBuilder
    $files | Sort-Object { $_.FullName.ToLowerInvariant() } | ForEach-Object {
        $rel = $_.FullName.Substring($rootFull.Length).TrimStart('\', '/').Replace('\', '/').ToLowerInvariant()
        $h = (Get-FileHash $_.FullName -Algorithm SHA256).Hash
        [void]$sb.Append($rel).Append('=').Append($h).Append("`n")
    }
    $sha = [System.Security.Cryptography.SHA256]::Create()
    $bytes = $sha.ComputeHash([Text.Encoding]::UTF8.GetBytes($sb.ToString()))
    return (($bytes | ForEach-Object { $_.ToString('x2') }) -join '')
}

# 读取 Cargo.toml 包版本（[package] 段的 ^version = "x.y.z"）
function Get-CargoPackageVersion {
    param([Parameter(Mandatory)][string]$RepoRoot)
    $line = Get-Content (Join-Path $RepoRoot 'Cargo.toml') -Encoding UTF8 |
            Select-String -Pattern '^version\s*=\s*"(\d+\.\d+\.\d+)"' | Select-Object -First 1
    if (-not $line) { throw '未在 Cargo.toml 中找到包版本行（^version = "x.y.z"）' }
    return $line.Matches[0].Groups[1].Value
}

# 递升 Cargo.toml 包版本（patch 或 minor；minor 时 patch 归零）；返回新版本号
function Step-CargoPackageVersion {
    param(
        [Parameter(Mandatory)][string]$RepoRoot,
        [ValidateSet('patch', 'minor')][string]$Kind = 'patch'
    )
    $file = Join-Path $RepoRoot 'Cargo.toml'
    $text = [IO.File]::ReadAllText($file)
    $m = [regex]::Match($text, '(?m)^version\s*=\s*"(\d+)\.(\d+)\.(\d+)"')
    if (-not $m.Success) { throw '未在 Cargo.toml 中找到包版本行' }
    $major = [int]$m.Groups[1].Value
    $minor = [int]$m.Groups[2].Value
    $patch = [int]$m.Groups[3].Value
    if ($Kind -eq 'minor') { $minor += 1; $patch = 0 } else { $patch += 1 }
    $newVer = "$major.$minor.$patch"
    $text = $text.Remove($m.Index, $m.Length).Insert($m.Index, ('version = "' + $newVer + '"'))
    [IO.File]::WriteAllText($file, $text, (New-Object Text.UTF8Encoding($false)))
    return $newVer
}

# 版本升号守卫主体（打包脚本调用；失败 throw 终止打包）
function Assert-VersionGuard {
    param(
        [Parameter(Mandatory)][string]$RepoRoot,
        [string]$Name = 'package',
        [string]$StateFile,           # 默认 {RepoRoot}/dist/.pack-guard.json
        [switch]$Force,               # 内容变化时也放行（仅告警）
        [switch]$Bump,                # 打包前自动递升 patch 版本
        [switch]$BumpMinor            # 打包前自动递升 minor 版本
    )
    $root = $RepoRoot.TrimEnd('\', '/')
    if (-not $StateFile) { $StateFile = Join-Path $root 'dist\.pack-guard.json' }

    # 0) 可选：自动递升版本（写 Cargo.toml + cargo check 更新 Cargo.lock）
    if ($Bump -or $BumpMinor) {
        $kind = if ($BumpMinor) { 'minor' } else { 'patch' }
        $oldVer = Get-CargoPackageVersion -RepoRoot $root
        $newVer = Step-CargoPackageVersion -RepoRoot $root -Kind $kind
        Push-Location $root
        try {
            cargo check --quiet
            if ($LASTEXITCODE -ne 0) { throw "版本递升后 cargo check 失败（Cargo.lock 未更新，打包会因 --locked 失败）：$newVer" }
        }
        finally { Pop-Location }
        Write-Host "== 版本已自动递升（$kind）：$oldVer -> $newVer ==" -ForegroundColor Cyan
    }

    # 1) 当前版本与内容指纹
    $ver = Get-CargoPackageVersion -RepoRoot $root
    Write-Host "== 版本守卫：$Name 当前版本 $ver，计算打包内容指纹… =="
    $fp = Get-SourceFingerprint -RepoRoot $root

    # 2) 读取历史记录（版本 -> {fp, at}）
    $state = @{}
    if (Test-Path $StateFile) {
        $raw = [IO.File]::ReadAllText($StateFile, [Text.Encoding]::UTF8)
        if ($raw.Trim()) {
            $obj = $raw | ConvertFrom-Json
            foreach ($p in $obj.PSObject.Properties) { $state[$p.Name] = @{ fp = $p.Value.fp; at = $p.Value.at } }
        }
    }

    # 3) 判定
    $needSave = $false
    if ($state.ContainsKey($ver)) {
        if ($state[$ver].fp -eq $fp) {
            Write-Host "== 版本守卫通过：$ver 内容未变化（重复打包，允许） ==" -ForegroundColor Green
        }
        elseif ($Force) {
            Write-Warning "同版本 $ver 内容已变化，但指定 -Force：放行并更新内容记录"
            $state[$ver] = @{ fp = $fp; at = (Get-Date -Format 'yyyy-MM-dd HH:mm:ss') }
            $needSave = $true
        }
        else {
            throw ("检测到「同版本号（$ver）打包内容变化」：请先升版本号后再打包" +
                   "（可加 -Bump 自动递升 / -BumpMinor 递升次版本；确需强制重打加 -Force）。`n" +
                   "  已记录指纹：$($state[$ver].fp.Substring(0, 16))…（$($state[$ver].at)）`n" +
                   "  当前指纹：  $($fp.Substring(0, 16))…")
        }
    }
    else {
        Write-Host "== 版本守卫通过：$ver 首次打包（记录内容指纹） ==" -ForegroundColor Green
        $state[$ver] = @{ fp = $fp; at = (Get-Date -Format 'yyyy-MM-dd HH:mm:ss') }
        $needSave = $true
    }

    # 4) 持久化记录
    if ($needSave) {
        $out = @{}
        foreach ($k in $state.Keys) { $out[$k] = [pscustomobject]@{ fp = $state[$k].fp; at = $state[$k].at } }
        $dir = Split-Path $StateFile -Parent
        if (-not (Test-Path $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
        [IO.File]::WriteAllText($StateFile, ($out | ConvertTo-Json -Depth 4), (New-Object Text.UTF8Encoding($false)))
    }
}
