# 发布与安装

这些脚本只提供发布和安装入口，不会在开发过程中自动执行安装、卸载或修改登录启动项。

## 生成便携发布包

在 Windows PowerShell 中执行：

```powershell
.\packaging\build-release.ps1
```

脚本会构建 release 版本，并在 `dist\ServerOn-windows-x64.zip` 生成便携包。发布包包含 GUI、daemon、CLI 和文档。

## 当前用户安装

```powershell
.\packaging\install.ps1
```

安装位置：`%LOCALAPPDATA%\Programs\ServerOn`，不需要管理员权限。脚本会创建开始菜单快捷方式。

如需当前用户登录时启动 daemon：

```powershell
.\packaging\install.ps1 -StartOnLogin
```

登录启动只启动管理器，不会因此自动启动所有服务；服务是否随 daemon 启动由各自策略决定。

## 卸载

先关闭 GUI 和 daemon，再执行：

```powershell
.\packaging\uninstall.ps1
```

默认保留配置和日志。只有明确传入以下参数时才删除 `%LOCALAPPDATA%\ServerOn`：

```powershell
.\packaging\uninstall.ps1 -RemoveData
```

卸载脚本不会自动停止用户服务，避免卸载管理器时误伤仍在运行的进程。
