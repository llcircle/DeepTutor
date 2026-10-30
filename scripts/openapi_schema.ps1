param(
    [Parameter(Mandatory = $true)]
    [string[]]$Name,
    [string]$OpenApiPath = "web/contracts/schema/openapi.json"
)

$ErrorActionPreference = "Stop"
$document = Get-Content -LiteralPath $OpenApiPath -Raw -Encoding UTF8 | ConvertFrom-Json -AsHashtable
foreach ($schemaName in $Name) {
    if (-not $document.components.schemas.ContainsKey($schemaName)) {
        Write-Output "MISSING SCHEMA $schemaName"
        continue
    }
    Write-Output "=== $schemaName ==="
    $document.components.schemas[$schemaName] |
        ConvertTo-Json -Depth 30
}
