param(
    [ValidateSet("debug", "release")]
    [string]$Configuration = "release",
    [switch]$StartOnLogin
)

$ErrorActionPreference = "Stop"
$projectRoot = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$sourceBin = Join-Path $projectRoot "target\$Configuration"
$installDir = Join-Path $env:LOCALAPPDATA "Programs\ServerOn"
$required = @("server-on.exe", "server-on-daemon.exe", "server-on-cli.exe")

foreach ($name in $required) {
    $path = Join-Path $sourceBin $name
    if (-not (Test-Path $path)) {
        throw "缺少 $path。请先执行 cargo build --$Configuration --workspace。"
    }
}

New-Item -ItemType Directory -Path $installDir -Force | Out-Null
foreach ($name in $required) {
    Copy-Item (Join-Path $sourceBin $name) (Join-Path $installDir $name) -Force
}
Copy-Item (Join-Path $projectRoot "README.md") (Join-Path $installDir "README.md") -Force

$startMenu = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs"
New-Item -ItemType Directory -Path $startMenu -Force | Out-Null
$shortcutPath = Join-Path $startMenu "ServerOn.lnk"
$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = Join-Path $installDir "server-on.exe"
$shortcut.WorkingDirectory = $installDir
$shortcut.IconLocation = Join-Path $installDir "server-on.exe"
$shortcut.Description = "ServerOn 本地服务管理器"
$shortcut.Save()

if ($StartOnLogin) {
    $runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
    New-Item -Path $runKey -Force | Out-Null
    $daemonPath = Join-Path $installDir "server-on-daemon.exe"
    Set-ItemProperty -Path $runKey -Name "ServerOnDaemon" -Value "`"$daemonPath`""
}

Write-Host "ServerOn 已安装到 $installDir"
Write-Host "开始菜单快捷方式已创建。配置和日志仍保存在 $env:LOCALAPPDATA\ServerOn。"
if ($StartOnLogin) {
    Write-Host "已配置当前用户登录时启动 daemon。"
}
