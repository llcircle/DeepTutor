param(
    [string]$PathPattern = "*",
    [string]$OpenApiPath = "web/contracts/schema/openapi.json"
)

$ErrorActionPreference = "Stop"
$document = Get-Content -LiteralPath $OpenApiPath -Raw -Encoding UTF8 | ConvertFrom-Json -AsHashtable
$methods = @("get", "post", "put", "delete", "patch", "head", "options")

foreach ($path in ($document.paths.Keys | Where-Object { $_ -like $PathPattern } | Sort-Object)) {
    foreach ($method in $document.paths[$path].Keys) {
        if ($methods -notcontains $method) {
            continue
        }
        $operation = $document.paths[$path][$method]
        $parameters = @()
        if ($operation.parameters) {
            foreach ($parameter in $operation.parameters) {
                if ($parameter.ContainsKey('$ref')) {
                    $parameters += "ref:$($parameter.'$ref')"
                }
                else {
                    $parameters += "$($parameter.name):$($parameter.in)"
                }
            }
        }
        $requestBodies = @()
        if ($operation.requestBody -and $operation.requestBody.content) {
            foreach ($contentType in $operation.requestBody.content.Keys) {
                $schema = $operation.requestBody.content[$contentType].schema
                if ($schema -and $schema.ContainsKey('$ref')) {
                    $requestBodies += "$contentType=$($schema.'$ref')"
                }
                elseif ($schema -and $schema.type) {
                    $requestBodies += "$contentType=$($schema.type)"
                }
                else {
                    $requestBodies += $contentType
                }
            }
        }
        $summary = if ($operation.summary) { $operation.summary } else { "" }
        $operationId = if ($operation.operationId) { $operation.operationId } else { "" }
        "${method.ToUpperInvariant()} $path | $operationId | $summary | params=$($parameters -join ',') | bodies=$($requestBodies -join ',')"
    }
}
