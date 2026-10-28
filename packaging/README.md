# 发布与安装

这些脚本只提供发布和安装入口，不会在开发过程中自动执行安装、卸载或修改登录启动项。GitHub Release 同时提供 Windows EXE 安装程序和便携 ZIP；安装程序当前未签名，Windows SmartScreen 可能显示提示。

## Windows 安装程序

从 GitHub Release 下载 `ServerOn-Setup-X.Y.Z-windows-x64.exe` 并运行：

- 安装位置：`%LOCALAPPDATA%\Programs\ServerOn`，不需要管理员权限。
- 创建开始菜单快捷方式和 Windows 卸载项。
- 可选择登录 Windows 时启动 daemon，默认不勾选。
- GUI 或 daemon 正在运行时，安装和卸载会中止并提示先关闭它们；安装程序不会自动停止 daemon 或其管理的服务。

可通过 Windows“已安装的应用”卸载。卸载会删除程序文件、快捷方式和登录启动项，默认保留 `%LOCALAPPDATA%\ServerOn` 中的配置、状态和日志；交互式卸载时可明确选择同时删除数据。

静默卸载时，只有显式添加 `/REMOVEDATA` 才会删除用户数据：

```powershell
& "$env:LOCALAPPDATA\Programs\ServerOn\unins000.exe" /VERYSILENT /REMOVEDATA
```

## 本地生成发布包

需要 Rust、Windows SDK 的 `rc.exe`；编译安装程序还需要 Inno Setup 6。

```powershell
# 生成 release 二进制和便携 ZIP
.\packaging\build-release.ps1 -ExpectedVersion 1.0.0

# 编译安装程序，ISCC.exe 也可通过完整路径调用
iscc /DAppVersion=1.0.0 /DNumericVersion=1.0.0.0 .\packaging\server-on.iss
```

产物：

```text
dist\ServerOn-windows-x64.zip
dist\ServerOn-Setup-1.0.0-windows-x64.exe
```

便携包包含 GUI、daemon、CLI 和文档。`build-release.ps1` 会清理并重新创建 `dist\ServerOn` staging 目录。

## 当前用户脚本安装

仍可使用现有脚本安装本地构建结果：

```powershell
.\packaging\install.ps1
```

安装位置：`%LOCALAPPDATA%\Programs\ServerOn`，不需要管理员权限。脚本会创建开始菜单快捷方式。

如需当前用户登录时启动 daemon：

```powershell
.\packaging\install.ps1 -StartOnLogin
```

登录启动只启动管理器，不会因此自动启动所有服务；服务是否随 daemon 启动由各自策略决定。

脚本卸载：

```powershell
.\packaging\uninstall.ps1
```

默认保留配置和日志。只有明确传入以下参数时才删除 `%LOCALAPPDATA%\ServerOn`：

```powershell
.\packaging\uninstall.ps1 -RemoveData
```

卸载脚本不会自动停止用户服务，避免卸载管理器时误伤仍在运行的进程。请使用与安装方式对应的卸载入口：脚本安装使用 `uninstall.ps1`，EXE 安装使用 Windows 卸载项，不要混用两种卸载方式。

## 自动版本与发布

- PR 创建和更新只运行格式、Clippy、测试及版本一致性检查，不创建 Tag 或 Release。
- PR 合并到 `main` 后，按 PR 标题计算未发布变更：`fix:` / `fix(scope):` 升 Patch，`feat:` / `feat(scope):` 升 Minor。例如 `v1.1.0` 分别升级为 `v1.1.1` 和 `v1.2.0`。
- 同一批未发布 PR 只发布一次，`feat` 优先于 `fix`。其他 PR 标题不自动发版；直接 push 的提交本身不参与版本计算，发布版本仍只由未发布的 `feat` / `fix` PR 决定。
- `feat!:`、`fix!:` 或标题包含 `BREAKING CHANGE` 时停止自动发版，主版本由维护者手动处理。
- 自动化更新根 `Cargo.toml` 和 `Cargo.lock`，提交 `chore(release): vX.Y.Z`，创建 annotated Tag，并在同一次工作流运行中调用 Windows Release。
- 手动推送 `vX.Y.Z` Tag 也会构建发布。Tag 必须与 Cargo workspace 版本完全一致，否则发布失败。
- Release 包含安装 EXE、便携 ZIP 和 SHA-256 校验文件，并在临时 runner 上执行静默安装/卸载冒烟测试。
- 发布失败后，在原工作流中选择 **Re-run failed jobs**；也可手动运行 **Windows release**，输入已有 Tag。手动运行时取消 `publish` 可只上传 Actions Artifact。

自动发版需要仓库允许 GitHub Actions 读写内容，并且 `main` 分支保护规则允许 GitHub Actions bot 推送版本提交和 Tag；否则只能使用手动 Tag 发布。不需要额外 PAT。

## 首个版本 v1.0.0

首个 Tag 不由自动化创建。确认包含 `Cargo.toml` 版本 `1.0.0` 的代码已合并到 `main` 后，由维护者手动执行：

```bash
git switch main
git pull origin main
git tag -a v1.0.0 -m "ServerOn v1.0.0"
git push origin v1.0.0
```

此后合并 `fix` 或 `feat` PR 才会自动升级版本。

## 版本维护命令

```powershell
# 设置所有 workspace 包的版本并同步 Cargo.lock
.\packaging\set-version.ps1 -Version 1.0.1

# 校验 workspace/lockfile，以及可选的 Tag 一致性
.\packaging\check-version.ps1
.\packaging\check-version.ps1 -Tag v1.0.1
```
