[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)]
    [string]$StageRoot,
    [string]$ZipPath = "",
    [switch]$SmokeTest,
    [ValidateRange(5, 120)]
    [int]$SmokeTimeoutSeconds = 30,
    [string]$SmokeWorkspace = ""
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version Latest
Add-Type -AssemblyName System.IO.Compression.FileSystem

$projectRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot "..")).Path
$stageRootFull = (Resolve-Path -LiteralPath $StageRoot).Path
$stageName = Split-Path -Leaf $stageRootFull
$report = [System.Collections.Generic.List[string]]::new()

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
        throw "拒绝操作验证目录之外的路径：$childFull"
    }
}

function Read-PeSummary {
    param([Parameter(Mandatory = $true)][string]$Path)

    $stream = [System.IO.File]::OpenRead($Path)
    $reader = [System.IO.BinaryReader]::new($stream)
    try {
        if ($reader.ReadUInt16() -ne 0x5A4D) {
            throw "不是有效的 PE 文件：$Path"
        }
        $stream.Position = 0x3C
        $peOffset = $reader.ReadInt32()
        $stream.Position = $peOffset
        if ($reader.ReadUInt32() -ne 0x00004550) {
            throw "PE 签名无效：$Path"
        }

        $machineValue = $reader.ReadUInt16()
        $sectionCount = $reader.ReadUInt16()
        $stream.Position += 12
        $optionalHeaderSize = $reader.ReadUInt16()
        $stream.Position += 2
        $optionalHeaderOffset = $stream.Position
        $magic = $reader.ReadUInt16()
        $dataDirectoryOffset = $optionalHeaderOffset + $(if ($magic -eq 0x20B) { 112 } else { 96 })
        $stream.Position = $dataDirectoryOffset + 8
        $importRva = $reader.ReadUInt32()
        $stream.Position = $optionalHeaderOffset + $optionalHeaderSize
        $sectionTableOffset = $stream.Position

        $sections = @()
        for ($index = 0; $index -lt $sectionCount; $index++) {
            $stream.Position = $sectionTableOffset + ($index * 40)
            $stream.Position += 8
            $sections += [pscustomobject]@{
                VirtualSize = $reader.ReadUInt32()
                VirtualAddress = $reader.ReadUInt32()
                RawSize = $reader.ReadUInt32()
                RawOffset = $reader.ReadUInt32()
            }
        }

        $machine = switch ($machineValue) {
            0x014C { "X86" }
            0x8664 { "X64" }
            0xAA64 { "ARM64" }
            default { "0x{0:X4}" -f $machineValue }
        }

        return [pscustomobject]@{
            Machine = $machine
            ImportRva = $importRva
            Sections = $sections
            Reader = $reader
            Stream = $stream
        }
    }
    catch {
        $reader.Dispose()
        $stream.Dispose()
        throw
    }
}

function Convert-RvaToOffset {
    param(
        [Parameter(Mandatory = $true)]$Pe,
        [Parameter(Mandatory = $true)][uint32]$Rva
    )

    foreach ($section in $Pe.Sections) {
        $extent = [Math]::Max($section.VirtualSize, $section.RawSize)
        if ($Rva -ge $section.VirtualAddress -and $Rva -lt ($section.VirtualAddress + $extent)) {
            return [uint32]($section.RawOffset + ($Rva - $section.VirtualAddress))
        }
    }
    throw "无法把 RVA 0x$($Rva.ToString('X8')) 映射到 PE 文件偏移。"
}

function Read-NullTerminatedAscii {
    param(
        [Parameter(Mandatory = $true)]$Reader,
        [Parameter(Mandatory = $true)][uint32]$Offset
    )

    $Reader.BaseStream.Position = $Offset
    $bytes = [System.Collections.Generic.List[byte]]::new()
    while ($Reader.BaseStream.Position -lt $Reader.BaseStream.Length) {
        $value = $Reader.ReadByte()
        if ($value -eq 0) {
            return [System.Text.Encoding]::ASCII.GetString($bytes.ToArray())
        }
        $bytes.Add($value)
    }
    throw "PE 导入名称未正确结束。"
}

function Get-PeImports {
    param([Parameter(Mandatory = $true)][string]$Path)

    $pe = Read-PeSummary -Path $Path
    try {
        if ($pe.ImportRva -eq 0) {
            return @()
        }
        $descriptorOffset = Convert-RvaToOffset -Pe $pe -Rva $pe.ImportRva
        $imports = [System.Collections.Generic.List[string]]::new()
        for ($index = 0; ; $index++) {
            $pe.Stream.Position = $descriptorOffset + ($index * 20) + 12
            $nameRva = $pe.Reader.ReadUInt32()
            if ($nameRva -eq 0) {
                break
            }
            $nameOffset = Convert-RvaToOffset -Pe $pe -Rva $nameRva
            $imports.Add((Read-NullTerminatedAscii -Reader $pe.Reader -Offset $nameOffset))
        }
        return @($imports | Sort-Object -Unique)
    }
    finally {
        $pe.Reader.Dispose()
        $pe.Stream.Dispose()
    }
}

function Test-ForbiddenPath {
    param([Parameter(Mandatory = $true)][string]$RelativePath)

    return $RelativePath -match '(?i)(^|/)(runtime/(python|node|remotion)|web/\.next|data|cache|temp|logs|home)(/|$)' -or
        $RelativePath -match '(?i)\.(py|pyc|pyd)$'
}

$requiredFiles = @(
    "DeepTutor.exe",
    "WebView2Loader.dll",
    "portable-manifest.json",
    "resources/web/index.html"
)
foreach ($relative in $requiredFiles) {
    $path = Join-Path $stageRootFull $relative
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "便携目录缺少必需文件：$relative"
    }
}
$report.Add("必需文件：通过")

$manifestPath = Join-Path $stageRootFull "portable-manifest.json"
$manifest = Get-Content -LiteralPath $manifestPath -Raw | ConvertFrom-Json
if ($manifest.runtime -ne "rust" -or
    $manifest.python_required -ne $false -or
    $manifest.node_required -ne $false) {
    throw "便携 manifest 的运行时声明不是独立 Rust 客户端。"
}

$executablePath = Join-Path $stageRootFull "DeepTutor.exe"
$loaderPath = Join-Path $stageRootFull "WebView2Loader.dll"
$executablePe = Read-PeSummary -Path $executablePath
$loaderPe = Read-PeSummary -Path $loaderPath
try {
    if ($executablePe.Machine -ne $loaderPe.Machine -or
        $executablePe.Machine -ne $manifest.architecture) {
        throw "EXE、WebView2Loader.dll 与 manifest 架构不一致：$($executablePe.Machine)/$($loaderPe.Machine)/$($manifest.architecture)。"
    }
}
finally {
    $executablePe.Reader.Dispose()
    $executablePe.Stream.Dispose()
    $loaderPe.Reader.Dispose()
    $loaderPe.Stream.Dispose()
}
$report.Add("PE 架构：$($executablePe.Machine)，通过")

$exeImports = @(Get-PeImports -Path $executablePath)
$loaderImports = @(Get-PeImports -Path $loaderPath)
if ($exeImports -notcontains "WebView2Loader.dll") {
    throw "DeepTutor.exe 导入表未声明 WebView2Loader.dll。"
}
foreach ($importGroup in @(
    @{ Name = "DeepTutor.exe"; Imports = $exeImports },
    @{ Name = "WebView2Loader.dll"; Imports = $loaderImports }
)) {
    foreach ($import in $importGroup.Imports) {
        $isApiSet = $import -match '^(api|ext)-ms-win-'
        $isLocal = Test-Path -LiteralPath (Join-Path $stageRootFull $import) -PathType Leaf
        $isSystem = Test-Path -LiteralPath (Join-Path $env:SystemRoot "System32\$import") -PathType Leaf
        $isSystemWow64 = Test-Path -LiteralPath (Join-Path $env:SystemRoot "SysWOW64\$import") -PathType Leaf
        if (-not ($isApiSet -or $isLocal -or $isSystem -or $isSystemWow64)) {
            throw "$($importGroup.Name) 缺少原生依赖：$import"
        }
    }
}
$report.Add("EXE 与 DLL 导入依赖：$($exeImports.Count + $loaderImports.Count) 项，通过")

$stageFiles = @(Get-ChildItem -LiteralPath $stageRootFull -File -Recurse -Force)
$forbidden = @($stageFiles | Where-Object {
    Test-ForbiddenPath -RelativePath $_.FullName.Substring($stageRootFull.Length).Replace('\', '/').TrimStart('/')
})
if ($forbidden.Count -gt 0) {
    throw "便携目录包含禁入文件：$($forbidden.FullName -join [Environment]::NewLine)"
}
$report.Add("便携目录禁入项：0")

if ($ZipPath) {
    $zipFull = (Resolve-Path -LiteralPath $ZipPath).Path
    $archive = [System.IO.Compression.ZipFile]::OpenRead($zipFull)
    try {
        $zipFiles = @($archive.Entries | Where-Object { -not $_.FullName.EndsWith('/') })
        $zipForbidden = @($zipFiles | Where-Object {
            Test-ForbiddenPath -RelativePath $_.FullName.Replace('\', '/').TrimStart('/')
        })
        if ($zipForbidden.Count -gt 0) {
            throw "ZIP 包含禁入文件：$($zipForbidden.FullName -join [Environment]::NewLine)"
        }

        $firstEntry = $zipFiles[0].FullName.Replace('\', '/')
        $prefixMatch = [regex]::Match($firstEntry, "^(?:.*\/)?$( [regex]::Escape($stageName) )/")
        if (-not $prefixMatch.Success) {
            throw "无法识别 ZIP 中的便携目录前缀。"
        }
        $zipPrefix = $prefixMatch.Value
        $zipRelative = @($zipFiles | ForEach-Object {
            $entry = $_.FullName.Replace('\', '/')
            if (-not $entry.StartsWith($zipPrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
                throw "ZIP 中存在便携目录之外的条目：$entry"
            }
            $entry.Substring($zipPrefix.Length)
        } | Sort-Object)
        $stageRelative = @($stageFiles | ForEach-Object {
            $_.FullName.Substring($stageRootFull.Length).Replace('\', '/').TrimStart('/')
        } | Sort-Object)
        if (Compare-Object -ReferenceObject $stageRelative -DifferenceObject $zipRelative) {
            throw "ZIP 与便携目录文件清单不一致。"
        }
    }
    finally {
        $archive.Dispose()
    }

    $hash = (Get-FileHash -LiteralPath $zipFull -Algorithm SHA256).Hash.ToLowerInvariant()
    $hashContent = Get-Content -LiteralPath "$zipFull.sha256" -Raw
    if ($hashContent -notmatch [regex]::Escape($hash)) {
        throw "ZIP 的 SHA-256 校验文件不匹配。"
    }
    $report.Add("ZIP 一致性与 SHA-256：$($stageFiles.Count) 个文件，通过")
}

if ($SmokeTest) {
    if (-not [System.Runtime.InteropServices.RuntimeInformation]::IsOSPlatform(
        [System.Runtime.InteropServices.OSPlatform]::Windows
    )) {
        throw "EXE 启动冒烟测试仅支持 Windows。"
    }
    if (-not $SmokeWorkspace) {
        $SmokeWorkspace = Join-Path $projectRoot "artifacts\exe-verification"
    }
    $smokeRoot = Join-Path $SmokeWorkspace $stageName
    Assert-ChildPath -Parent $projectRoot -Child $smokeRoot
    if (Test-Path -LiteralPath $smokeRoot) {
        Remove-Item -LiteralPath $smokeRoot -Recurse -Force
    }
    New-Item -ItemType Directory -Path $SmokeWorkspace -Force | Out-Null
    Copy-Item -LiteralPath $stageRootFull -Destination $smokeRoot -Recurse -Force

    $smokeExe = Join-Path $smokeRoot "DeepTutor.exe"
    $smokeLog = Join-Path $smokeRoot "logs\desktop.log"
    $smokeStdout = Join-Path $SmokeWorkspace "$stageName-stdout.txt"
    $smokeStderr = Join-Path $SmokeWorkspace "$stageName-stderr.txt"
    $hadRustLog = Test-Path -LiteralPath "Env:RUST_LOG"
    $previousRustLog = $env:RUST_LOG
    $env:RUST_LOG = "deeptutor_desktop_lib=info,tauri=warn"
    try {
        $process = Start-Process -FilePath $smokeExe -WorkingDirectory $smokeRoot `
            -WindowStyle Hidden -RedirectStandardOutput $smokeStdout `
            -RedirectStandardError $smokeStderr -PassThru
    }
    finally {
        if ($hadRustLog) {
            $env:RUST_LOG = $previousRustLog
        }
        else {
            Remove-Item -LiteralPath "Env:RUST_LOG" -ErrorAction SilentlyContinue
        }
    }
    try {
        $deadline = [DateTime]::UtcNow.AddSeconds($SmokeTimeoutSeconds)
        $ready = $false
        while ([DateTime]::UtcNow -lt $deadline) {
            if ($process.HasExited) {
                $stderr = if (Test-Path -LiteralPath $smokeStderr) {
                    Get-Content -LiteralPath $smokeStderr -Raw
                }
                else {
                    ""
                }
                throw "DeepTutor.exe 提前退出，退出码 $($process.ExitCode)。stderr：$stderr"
            }
            if ((Test-Path -LiteralPath $smokeLog) -and
                (Get-Content -LiteralPath $smokeLog -Raw) -match "Rust 原生服务已启动") {
                $ready = $true
                break
            }
            Start-Sleep -Milliseconds 250
        }
        if (-not $ready) {
            throw "启动冒烟测试超时：$SmokeTimeoutSeconds 秒内 Rust 服务未就绪。"
        }
        Start-Sleep -Seconds 2
        if ($process.HasExited) {
            throw "DeepTutor.exe 在 Rust 服务就绪后异常退出。"
        }

        $startupLog = Get-Content -LiteralPath $smokeLog -Raw
        if ($startupLog -notmatch 'url=(http://127\.0\.0\.1:\d+)') {
            throw "启动日志未记录可访问的 Rust 服务地址。"
        }
        $serviceUrl = $Matches[1]
        $moduleResponse = Invoke-RestMethod -Uri "$serviceUrl/api/modules" -Method Get
        $moduleRows = @($moduleResponse.modules)
        if ($moduleRows.Count -ne 25) {
            throw "/api/modules 返回 $($moduleRows.Count) 个模块，预期 25 个。"
        }
        if (@($moduleRows | Where-Object {
            -not ($_.installed -eq $true -and $_.enabled -eq $true -and $_.ready -eq $true)
        }).Count -gt 0) {
            throw "/api/modules 存在未就绪、未启用或未集成模块。"
        }
    }
    finally {
        if (-not $process.HasExited) {
            Stop-Process -Id $process.Id -Force
            $process.WaitForExit(5000) | Out-Null
        }
    }

    foreach ($created in @(
        "data\deeptutor.sqlite3",
        "cache\webview2",
        "logs\desktop.log",
        "temp",
        "home"
    )) {
        if (-not (Test-Path -LiteralPath (Join-Path $smokeRoot $created))) {
            throw "启动后缺少安装目录内状态：$created"
        }
    }
    $report.Add("EXE 启动冒烟测试：WebView/Rust 服务、25 个模块接口与安装目录数据，通过")
}

$report.Insert(0, "DeepTutor 便携 EXE 验证通过")
$report | ForEach-Object { Write-Host $_ }
