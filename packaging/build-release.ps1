$ErrorActionPreference = "Stop"
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$distDir = Join-Path $projectRoot "dist"
$stageDir = Join-Path $distDir "ServerOn"

cargo build --release --workspace
if ($LASTEXITCODE -ne 0) {
    throw "Release 构建失败"
}

New-Item -ItemType Directory -Path $stageDir -Force | Out-Null
$files = @("server-on.exe", "server-on-daemon.exe", "server-on-cli.exe")
foreach ($name in $files) {
    Copy-Item (Join-Path $projectRoot "target\release\$name") (Join-Path $stageDir $name) -Force
}
Copy-Item (Join-Path $projectRoot "README.md") (Join-Path $stageDir "README.md") -Force
Copy-Item (Join-Path $projectRoot "DESIGN.md") (Join-Path $stageDir "DESIGN.md") -Force
Copy-Item (Join-Path $projectRoot "VALIDATION.md") (Join-Path $stageDir "VALIDATION.md") -Force

$archive = Join-Path $distDir "ServerOn-windows-x64.zip"
if (Test-Path $archive) {
    Remove-Item $archive -Force
}
Compress-Archive -Path (Join-Path $stageDir "*") -DestinationPath $archive -Force
Write-Host "已生成 $archive"
