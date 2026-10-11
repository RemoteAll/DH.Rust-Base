<#
.SYNOPSIS
    跨仓一键提交：读取 Pek.Rust.code-workspace 的仓库列表，逐仓 git add/commit（可选推送）。
.DESCRIPTION
    遍历工作区文件中登记的每个仓库：
    - 有变更 → git add -A + git commit -m <消息>（默认随后 git push；-NoPush 跳过推送）；
    - 无变更 → 跳过并在汇总中列出；
    - 任一仓库提交/推送失败 → 不影响其他仓库，汇总中标注需处理。
    用法示例：
        powershell -ExecutionPolicy Bypass -File tools/continuity/commit-all.ps1 -Message "feat: xxx"
        powershell -ExecutionPolicy Bypass -File tools/continuity/commit-all.ps1 -Message "feat: xxx" -NoPush
#>
param(
    [Parameter(Mandatory = $true)]
    [string]$Message,
    [switch]$NoPush,
    [string]$Workspace
)

$ErrorActionPreference = 'Continue'

if (-not $Workspace) {
    # 默认：本脚本位于 <盘>:\Code\Rust\DH.RustBase\tools\continuity\ → 上溯三级 = <盘>:\Code\Rust
    $root = Split-Path (Split-Path (Split-Path $PSScriptRoot -Parent) -Parent) -Parent
    $Workspace = Join-Path $root 'Pek.Rust.code-workspace'
}
if (-not (Test-Path $Workspace)) {
    Write-Host "找不到工作区文件：$Workspace（可用 -Workspace 指定）"
    exit 1
}

$ws = Get-Content $Workspace -Raw -Encoding UTF8 | ConvertFrom-Json
# 解析每个登记目录所属 git 仓库根（支持“父仓库中的子目录”型项目，如 DHDeploy.Agent.Rust 属于 DHDeploy 仓库），并按仓库根去重
$tops = @()
foreach ($r in @($ws.folders | ForEach-Object { $_.path })) {
    $top = git -C $r rev-parse --show-toplevel 2>$null
    if ($LASTEXITCODE -eq 0 -and $top) { $tops += $top }
}
$repos = $tops | Sort-Object -Unique

$doneList = @()
$cleanList = @()
$failList = @()
foreach ($r in $repos) {
    $name = Split-Path $r -Leaf
    $status = git -C $r status --porcelain 2>$null
    if (-not $status) { $cleanList += $name; continue }
    git -C $r add -A | Out-Null
    $out = git -C $r commit -m $Message 2>&1
    if ($LASTEXITCODE -ne 0) {
        $failList += "$name（提交失败：$($out | Select-Object -Last 1)）"
        continue
    }
    if ($NoPush) {
        $doneList += "$name 已提交"
    } else {
        $p = git -C $r push 2>&1
        if ($LASTEXITCODE -ne 0) {
            $failList += "$name（已提交、推送失败：$($p | Select-Object -Last 1)）"
        } else {
            $doneList += "$name 已提交并推送"
        }
    }
}

Write-Host ("提交完成：" + ($(if ($doneList) { $doneList -join '；' } else { '（无）' })))
if ($cleanList) { Write-Host ("无变更跳过：" + ($cleanList -join '、')) }
if ($failList) { Write-Host ("需处理：" + ($failList -join '；')); exit 2 }
