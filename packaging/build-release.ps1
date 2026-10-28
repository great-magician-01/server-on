param(
    [ValidatePattern('^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$')]
    [string]$ExpectedVersion
)

$ErrorActionPreference = "Stop"
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$distDir = Join-Path $projectRoot "dist"
$stageDir = Join-Path $distDir "ServerOn"
$targetDir = if ($env:CARGO_TARGET_DIR) {
    if ([System.IO.Path]::IsPathRooted($env:CARGO_TARGET_DIR)) {
        $env:CARGO_TARGET_DIR
    } else {
        Join-Path $projectRoot $env:CARGO_TARGET_DIR
    }
} else {
    Join-Path $projectRoot "target"
}
$releaseDir = Join-Path $targetDir "release"

Push-Location $projectRoot
try {
    cargo build --release --workspace --locked
    if ($LASTEXITCODE -ne 0) {
        throw "Release 构建失败"
    }
} finally {
    Pop-Location
}

if (Test-Path $stageDir) {
    Remove-Item $stageDir -Recurse -Force
}
New-Item -ItemType Directory -Path $stageDir -Force | Out-Null
$files = @("server-on.exe", "server-on-daemon.exe", "server-on-cli.exe")
foreach ($name in $files) {
    Copy-Item (Join-Path $releaseDir $name) (Join-Path $stageDir $name) -Force
}
Copy-Item (Join-Path $projectRoot "README.md") (Join-Path $stageDir "README.md") -Force
Copy-Item (Join-Path $projectRoot "DESIGN.md") (Join-Path $stageDir "DESIGN.md") -Force
Copy-Item (Join-Path $projectRoot "VALIDATION.md") (Join-Path $stageDir "VALIDATION.md") -Force

if ($ExpectedVersion) {
    & (Join-Path $PSScriptRoot "check-version.ps1") -Tag "v$ExpectedVersion"
    $productVersion = (Get-Item (Join-Path $stageDir "server-on.exe")).VersionInfo.ProductVersion
    if ($productVersion -ne $ExpectedVersion) {
        throw "GUI ProductVersion '$productVersion' 与预期版本 '$ExpectedVersion' 不一致。"
    }
}

$archive = Join-Path $distDir "ServerOn-windows-x64.zip"
if (Test-Path $archive) {
    Remove-Item $archive -Force
}
Compress-Archive -Path (Join-Path $stageDir "*") -DestinationPath $archive -Force
Write-Host "已生成 $archive"
