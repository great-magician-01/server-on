param(
    [string]$Tag
)

$ErrorActionPreference = "Stop"
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path

Push-Location $projectRoot
try {
    $metadataJson = & cargo metadata --locked --no-deps --format-version 1 | Out-String
    if ($LASTEXITCODE -ne 0) {
        throw "cargo metadata 失败。请确认 Cargo.lock 已与 Cargo.toml 同步。"
    }

    $metadata = $metadataJson | ConvertFrom-Json
    $workspacePackages = @($metadata.packages | Where-Object {
        $metadata.workspace_members -contains $_.id
    })
    if ($workspacePackages.Count -eq 0) {
        throw "cargo metadata 未返回 workspace 包。"
    }

    $versions = @($workspacePackages | ForEach-Object { $_.version } | Sort-Object -Unique)
    if ($versions.Count -ne 1 -or $versions[0] -notmatch '^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$') {
        $details = $workspacePackages | ForEach-Object { "$($_.name)=$($_.version)" }
        throw "workspace 版本不一致或不是 X.Y.Z：$($details -join ', ')"
    }

    $version = $versions[0]
    $lockPath = Join-Path $projectRoot "Cargo.lock"
    $lockText = [System.IO.File]::ReadAllText($lockPath)
    $lockEntries = [regex]::Matches(
        $lockText,
        '(?ms)^\[\[package\]\]\r?\n(.*?)(?=^\[\[package\]\]|\z)'
    )
    $lockVersions = @{}
    foreach ($entry in $lockEntries) {
        $nameMatch = [regex]::Match($entry.Value, '(?m)^name = "([^"]+)"\r?$')
        $versionMatch = [regex]::Match($entry.Value, '(?m)^version = "([^"]+)"\r?$')
        if ($nameMatch.Success -and $versionMatch.Success) {
            $lockVersions[$nameMatch.Groups[1].Value] = $versionMatch.Groups[1].Value
        }
    }
    $lockMismatches = @($workspacePackages | Where-Object {
        -not $lockVersions.ContainsKey($_.name) -or $lockVersions[$_.name] -ne $version
    })
    if ($lockMismatches.Count -ne 0) {
        $details = $lockMismatches | ForEach-Object {
            "$($_.name): manifest=$version, lock=$($lockVersions[$_.name])"
        }
        throw "Cargo.lock 中的 workspace 版本未同步：$($details -join ', ')。请运行 packaging/set-version.ps1。"
    }

    if ($PSBoundParameters.ContainsKey("Tag")) {
        if ($Tag -notmatch '^v(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$') {
            throw "Tag '$Tag' 不符合 vX.Y.Z 格式。"
        }
        if ($Tag -ne "v$version") {
            throw "Tag '$Tag' 与 Cargo workspace 版本 '$version' 不一致。"
        }
    }

    Write-Host "版本校验通过：$($workspacePackages.Count) 个 workspace 包均为 $version$(if ($Tag) { "，Tag 为 $Tag" })。"
} finally {
    Pop-Location
}
