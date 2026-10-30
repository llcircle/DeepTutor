param(
    [string]$OpenApiPath = "web/contracts/schema/openapi.json",
    [string]$ServerDirectory = "src-tauri/src/server",
    [string]$OutputPath = ".tauri-build-cache/missing-operations-current.txt",
    [string]$ImplementedOutputPath = ".tauri-build-cache/implemented-operations-current.txt"
)

$ErrorActionPreference = "Stop"
[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new($false)

function Normalize-RoutePath {
    param([string]$Path)

    if ([string]::IsNullOrWhiteSpace($Path)) {
        return "/"
    }
    $normalized = $Path.Trim()
    if (-not $normalized.StartsWith("/")) {
        $normalized = "/$normalized"
    }
    $wildcardMarker = "__DEEPTUTOR_WILDCARD__"
    $normalized = $normalized -replace "\{\*[^}]+\}", $wildcardMarker
    $normalized = $normalized -replace "\{[^}]+\}", "{}"
    $normalized = $normalized.Replace($wildcardMarker, "{*}")
    if ($normalized.Length -gt 1) {
        $normalized = $normalized.TrimEnd("/")
    }
    return $normalized
}

function Test-OperationImplemented {
    param(
        [string]$Operation,
        [System.Collections.Generic.HashSet[string]]$Implemented
    )

    if ($Implemented.Contains($Operation)) {
        return $true
    }
    $parts = $Operation.Split(" ", 2)
    if ($parts.Count -ne 2) {
        return $false
    }
    $method = $parts[0]
    $path = $parts[1]
    if ($method -eq "HEAD" -and $Implemented.Contains("GET $path")) {
        return $true
    }
    foreach ($candidate in $Implemented) {
        $candidateParts = $candidate.Split(" ", 2)
        if ($candidateParts.Count -ne 2) {
            continue
        }
        $candidateMethod = $candidateParts[0]
        $candidatePath = $candidateParts[1]
        if ($method -eq "HEAD" -and $candidateMethod -eq "GET") {
            $candidateMethod = "HEAD"
        }
        if ($candidateMethod -ne $method -or -not $candidatePath.Contains("/{*}")) {
            continue
        }
        $prefix = $candidatePath.Substring(0, $candidatePath.IndexOf("/{*}", [StringComparison]::Ordinal))
        if ($path.StartsWith($prefix, [StringComparison]::Ordinal)) {
            return $true
        }
    }
    return $false
}

function Get-CallExpression {
    param(
        [string]$Text,
        [int]$OpenParenthesis
    )

    $depth = 0
    $quote = [char]0
    $escaped = $false
    for ($index = $OpenParenthesis; $index -lt $Text.Length; $index++) {
        $character = $Text[$index]
        if ($quote -ne [char]0) {
            if ($escaped) {
                $escaped = $false
            }
            elseif ($character -eq "\") {
                $escaped = $true
            }
            elseif ($character -eq $quote) {
                $quote = [char]0
            }
            continue
        }
        if ($character -eq '"' -or $character -eq "'") {
            $quote = $character
            continue
        }
        if ($character -eq "(") {
            $depth++
        }
        elseif ($character -eq ")") {
            $depth--
            if ($depth -eq 0) {
                return $Text.Substring($OpenParenthesis, $index - $OpenParenthesis + 1)
            }
        }
    }
    return $Text.Substring($OpenParenthesis)
}

$document = Get-Content -LiteralPath $OpenApiPath -Raw -Encoding UTF8 | ConvertFrom-Json
$httpMethods = @("get", "post", "put", "delete", "patch", "head", "options")
$contract = [System.Collections.Generic.HashSet[string]]::new()
foreach ($pathProperty in $document.paths.PSObject.Properties) {
    $path = $pathProperty.Name
    $normalizedPath = Normalize-RoutePath $path
    foreach ($methodProperty in $pathProperty.Value.PSObject.Properties) {
        $normalizedMethod = $methodProperty.Name.ToLowerInvariant()
        if ($httpMethods -contains $normalizedMethod) {
            [void]$contract.Add("$($normalizedMethod.ToUpperInvariant()) $normalizedPath")
        }
    }
}

$implemented = [System.Collections.Generic.HashSet[string]]::new()
$rootMountedFiles = @("mod.rs", "files.rs", "files_api.rs")
foreach ($file in Get-ChildItem -LiteralPath $ServerDirectory -Filter "*.rs" -File) {
    $text = Get-Content -LiteralPath $file.FullName -Raw -Encoding UTF8
    $prefix = if ($rootMountedFiles -contains $file.Name) { "" } else { "/api" }
    $cursor = 0
    while (($routeIndex = $text.IndexOf(".route(", $cursor, [StringComparison]::Ordinal)) -ge 0) {
        $openParenthesis = $routeIndex + ".route".Length
        $expression = Get-CallExpression -Text $text -OpenParenthesis $openParenthesis
        $cursor = $routeIndex + $expression.Length
        $pathMatch = [regex]::Match($expression, '^\s*\(\s*"([^"]+)"')
        if (-not $pathMatch.Success) {
            continue
        }
        $routePath = $pathMatch.Groups[1].Value
        if ($routePath -eq "/") {
            $fullPath = if ($prefix) { "/api" } else { "/" }
        }
        else {
            $fullPath = "$prefix$routePath"
        }
        $fullPath = Normalize-RoutePath $fullPath
        foreach ($methodMatch in [regex]::Matches($expression, '(?<![\w])(get|post|put|delete|patch|head|options)\s*\(')) {
            $operation = "$($methodMatch.Groups[1].Value.ToUpperInvariant()) $fullPath"
            [void]$implemented.Add($operation)
        }
    }
}

$missing = $contract | Where-Object {
    -not (Test-OperationImplemented -Operation $_ -Implemented $implemented)
} | Sort-Object
$extra = $implemented | Where-Object {
    -not $contract.Contains($_) -and
    -not (
        $_.StartsWith("HEAD ") -and
        $contract.Contains("GET $($_.Substring(5))")
    )
} | Sort-Object

$outputDirectory = Split-Path -Parent $OutputPath
if ($outputDirectory -and -not (Test-Path -LiteralPath $outputDirectory)) {
    New-Item -ItemType Directory -Path $outputDirectory | Out-Null
}

$contractCount = @($contract).Count
$implementedCount = @($implemented).Count
$missingCount = @($missing).Count
$extraCount = @($extra).Count
$lines = [System.Collections.Generic.List[string]]::new()
$lines.Add("OpenAPI 操作：$contractCount")
$lines.Add("Rust 已注册操作：$implementedCount")
$lines.Add("缺失契约操作：$missingCount")
$lines.Add("额外 Rust 操作：$extraCount")
$lines.Add("")
$missing | ForEach-Object { $lines.Add("MISSING $_") }
$lines.Add("")
$extra | ForEach-Object { $lines.Add("EXTRA $_") }
$lines | Set-Content -LiteralPath $OutputPath -Encoding UTF8
$implemented | Sort-Object | Set-Content -LiteralPath $ImplementedOutputPath -Encoding UTF8
$lines | Select-Object -First 8
