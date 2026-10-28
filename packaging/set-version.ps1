param(
    [Parameter(Mandatory = $true)]
    [ValidatePattern('^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$')]
    [string]$Version
)

$ErrorActionPreference = "Stop"
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$manifestPath = Join-Path $projectRoot "Cargo.toml"
$manifest = [System.IO.File]::ReadAllText($manifestPath)
$sectionMatch = [regex]::Match(
    $manifest,
    '(?ms)^\[workspace\.package\]\s*\r?\n.*?(?=^\[|\z)'
)
if (-not $sectionMatch.Success) {
    throw "Cargo.toml 中缺少 [workspace.package]。"
}

$versionPattern = [regex]'(?m)^version\s*=\s*"[^"]*"'
if (-not $versionPattern.IsMatch($sectionMatch.Value)) {
    throw "[workspace.package] 中缺少 version。"
}
$updatedSection = $versionPattern.Replace($sectionMatch.Value, "version = `"$Version`"", 1)
$updatedManifest = $manifest.Remove($sectionMatch.Index, $sectionMatch.Length).Insert(
    $sectionMatch.Index,
    $updatedSection
)
[System.IO.File]::WriteAllText(
    $manifestPath,
    $updatedManifest,
    [System.Text.UTF8Encoding]::new($false)
)

Push-Location $projectRoot
try {
    & cargo update --workspace
    if ($LASTEXITCODE -ne 0) {
        throw "更新 Cargo.lock 失败。"
    }
    & (Join-Path $PSScriptRoot "check-version.ps1")
    if ($LASTEXITCODE -ne 0) {
        throw "版本校验失败。"
    }
} finally {
    Pop-Location
}
