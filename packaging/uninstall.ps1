param(
    [switch]$RemoveData
)

$ErrorActionPreference = "Stop"
$installDir = Join-Path $env:LOCALAPPDATA "Programs\ServerOn"
$running = Get-Process -Name "server-on", "server-on-daemon" -ErrorAction SilentlyContinue
if ($running) {
    throw "检测到 ServerOn 正在运行。请先关闭 GUI 和 daemon，再执行卸载；服务不会由卸载脚本自动停止。"
}

$startMenuShortcut = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\ServerOn.lnk"
if (Test-Path $startMenuShortcut) {
    Remove-Item $startMenuShortcut -Force
}

$runKey = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Run"
if (Get-ItemProperty -Path $runKey -Name "ServerOnDaemon" -ErrorAction SilentlyContinue) {
    Remove-ItemProperty -Path $runKey -Name "ServerOnDaemon"
}

if (Test-Path $installDir) {
    Remove-Item $installDir -Recurse -Force
}

if ($RemoveData) {
    $dataDir = Join-Path $env:LOCALAPPDATA "ServerOn"
    if (Test-Path $dataDir) {
        Remove-Item $dataDir -Recurse -Force
    }
    Write-Host "ServerOn 已卸载，配置和日志已删除。"
} else {
    Write-Host "ServerOn 已卸载，配置和日志已保留在 $env:LOCALAPPDATA\ServerOn。"
}
