[CmdletBinding()]
param(
    [ValidateSet("release", "debug")]
    [string]$Configuration = "release",
    [switch]$SkipFrontend,
    [switch]$SkipZip,
    [string]$WebView2RuntimeDir = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest

$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).Path
$tauriRoot = Join-Path $projectRoot "src-tauri"
$webRoot = Join-Path $projectRoot "web"
$webOutput = Join-Path $tauriRoot "resources\web"
$artifactRoot = Join-Path $projectRoot "artifacts\portable"
$archiveRoot = Join-Path $projectRoot "artifacts"
$stageRoot = Join-Path $artifactRoot "DeepTutor"
$isWindowsHost = [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
    [System.Runtime.InteropServices.OSPlatform]::Windows
)

function Assert-ChildPath {
    param(
        [Parameter(Mandatory = $true)][string]$Parent,
        [Parameter(Mandatory = $true)][string]$Child
    )

    $parentFull = [System.IO.Path]::GetFullPath($Parent).TrimEnd('\', '/')
    $childFull = [System.IO.Path]::GetFullPath($Child)
    if (-not $childFull.StartsWith(
        $parentFull + [System.IO.Path]::DirectorySeparatorChar,
        [System.StringComparison]::OrdinalIgnoreCase
    )) {
        throw "拒绝操作构建目录之外的路径：$childFull"
    }
}

function Invoke-Checked {
    param(
        [Parameter(Mandatory = $true)][string]$Command,
        [Parameter(Mandatory = $true)][string[]]$Arguments,
        [Parameter(Mandatory = $true)][string]$WorkingDirectory
    )

    Write-Host "> $Command $($Arguments -join ' ')"
    Push-Location $WorkingDirectory
    try {
        & $Command @Arguments
        if ($LASTEXITCODE -ne 0) {
            throw "命令失败（退出码 $LASTEXITCODE）：$Command"
        }
    }
    finally {
        Pop-Location
    }
}

if (-not $SkipFrontend) {
    if (-not (Test-Path -LiteralPath (Join-Path $webRoot "node_modules"))) {
        Invoke-Checked -Command "npm" -Arguments @("ci", "--legacy-peer-deps") -WorkingDirectory $webRoot
    }
    Invoke-Checked -Command "npm" -Arguments @("run", "build:desktop") -WorkingDirectory $webRoot
}

if (-not (Test-Path -LiteralPath (Join-Path $webOutput "index.html"))) {
    throw "缺少桌面前端资源。请先运行 npm run build:desktop。"
}

Invoke-Checked -Command "cargo" -Arguments @(
    "build",
    "--manifest-path",
    (Join-Path $tauriRoot "Cargo.toml"),
    "--locked",
    "--$Configuration"
) -WorkingDirectory $projectRoot

$targetRoot = Join-Path $tauriRoot "target"
$profileDirectory = if ($Configuration -eq "release") { "release" } else { "debug" }
$executable = if ($isWindowsHost) {
    Join-Path $targetRoot "$profileDirectory\deeptutor-desktop.exe"
}
else {
    Join-Path $targetRoot "$profileDirectory/deeptutor-desktop"
}
if (-not (Test-Path -LiteralPath $executable)) {
    throw "未找到 Rust 可执行文件：$executable"
}

New-Item -ItemType Directory -Path $artifactRoot -Force | Out-Null
if (Test-Path -LiteralPath $stageRoot) {
    Assert-ChildPath -Parent $artifactRoot -Child $stageRoot
    Remove-Item -LiteralPath $stageRoot -Recurse -Force
}
New-Item -ItemType Directory -Path $stageRoot -Force | Out-Null

$portableExecutable = if ($isWindowsHost) {
    Join-Path $stageRoot "DeepTutor.exe"
}
else {
    Join-Path $stageRoot "DeepTutor"
}
Copy-Item -LiteralPath $executable -Destination $portableExecutable

$resourcesTarget = Join-Path $stageRoot "resources"
New-Item -ItemType Directory -Path $resourcesTarget -Force | Out-Null
Copy-Item -LiteralPath $webOutput -Destination $resourcesTarget -Recurse -Force

if ($WebView2RuntimeDir) {
    $runtimeSource = (Resolve-Path -LiteralPath $WebView2RuntimeDir).Path
    if (-not (Test-Path -LiteralPath (Join-Path $runtimeSource "msedgewebview2.exe"))) {
        throw "WebView2 固定运行时目录缺少 msedgewebview2.exe：$runtimeSource"
    }
    Copy-Item -LiteralPath $runtimeSource `
        -Destination (Join-Path $resourcesTarget "webview2-runtime") `
        -Recurse -Force
}

foreach ($relative in @("LICENSE", "THIRD_PARTY_NOTICES.md")) {
    $source = Join-Path $projectRoot $relative
    if (Test-Path -LiteralPath $source) {
        Copy-Item -LiteralPath $source -Destination $stageRoot
    }
}

$versionText = Get-Content -LiteralPath (Join-Path $tauriRoot "Cargo.toml") -Raw
$versionMatch = [regex]::Match($versionText, '(?m)^version\s*=\s*"([^"]+)"')
if (-not $versionMatch.Success) {
    throw "无法读取 Cargo.toml 中的应用版本。"
}
$version = $versionMatch.Groups[1].Value

$portableReadme = @"
DeepTutor 纯 Rust 便携桌面版 $version
====================================

1. 双击 DeepTutor.exe 启动。
2. 首次启动会在当前目录创建 data、cache、temp、logs 和 home。
3. 迁移到其他目录或电脑时，复制整个 DeepTutor 文件夹即可。
4. 程序不依赖 Python、Node.js 或 node_modules，也不会把应用数据写入 C 盘用户目录。
5. 升级时仅替换 DeepTutor.exe 和 resources\web，保留 data、cache、temp、logs、home。
6. 如果系统没有 WebView2，可在构建时使用 -WebView2RuntimeDir 打入固定版本运行时。

诊断日志：logs\desktop.log
"@
Set-Content -LiteralPath (Join-Path $stageRoot "PORTABLE-README.txt") `
    -Value $portableReadme -Encoding utf8

$manifest = [ordered]@{
    product = "DeepTutor"
    version = $version
    runtime = "rust"
    architecture = [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
    built_at = (Get-Date).ToUniversalTime().ToString("o")
    portable = $true
    python_required = $false
    node_required = $false
    webview2_fixed_runtime = [bool]$WebView2RuntimeDir
    data_layout = @("data", "cache", "temp", "logs", "home")
}
$manifest | ConvertTo-Json -Depth 4 |
    Set-Content -LiteralPath (Join-Path $stageRoot "portable-manifest.json") -Encoding utf8

$forbiddenRuntimeNames = @(
    "node.exe",
    "python.exe",
    "python3",
    "node_modules",
    "next",
    "remotion"
)
$forbiddenExtensions = @(".py", ".pyc", ".pyd")
$forbiddenPathSegments = @(
    "runtime\python",
    "runtime\python\",
    "runtime/node",
    "runtime/node/",
    "runtime\remotion",
    "runtime/remotion/",
    "web\.next",
    "web/.next",
    "\data\",
    "/data/",
    "\cache\",
    "/cache/",
    "\temp\",
    "/temp/",
    "\logs\",
    "/logs/",
    "\home\",
    "/home/"
)
$forbidden = Get-ChildItem -LiteralPath $stageRoot -Recurse -Force | Where-Object {
    $relative = $_.FullName.Substring($stageRoot.Length).Replace('\', '/')
    $pathMatches = @($forbiddenPathSegments | Where-Object {
        $relative.Contains($_.Replace('\', '/'), [System.StringComparison]::OrdinalIgnoreCase)
    })
    $_.Name -in $forbiddenRuntimeNames -or
    $_.Extension -in $forbiddenExtensions -or
    $pathMatches.Count -gt 0
}
if ($forbidden) {
    $paths = ($forbidden | ForEach-Object FullName) -join [Environment]::NewLine
    throw "便携包包含被禁止的运行时或用户数据：$([Environment]::NewLine)$paths"
}

if (-not $SkipZip) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    $zipPath = Join-Path $archiveRoot "DeepTutor-$version-portable-$($manifest.architecture).zip"
    if (Test-Path -LiteralPath $zipPath) {
        Remove-Item -LiteralPath $zipPath -Force
    }
    [System.IO.Compression.ZipFile]::CreateFromDirectory(
        $artifactRoot,
        $zipPath,
        [System.IO.Compression.CompressionLevel]::Optimal,
        $true
    )
    $hash = (Get-FileHash -LiteralPath $zipPath -Algorithm SHA256).Hash.ToLowerInvariant()
    Set-Content -LiteralPath "$zipPath.sha256" -Value "$hash  $(Split-Path $zipPath -Leaf)" -Encoding ascii
    Write-Host "便携包：$zipPath"
}

Write-Host "便携目录：$stageRoot"
