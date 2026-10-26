# ServerOn

ServerOn 是一个面向 Windows 本地开发环境的原生服务管理器，统一管理 Windows 进程、WSL 服务和 WSL 内 Docker Engine 中的已有容器。

## 当前实现

- Rust + Slint 原生 GUI，不依赖 Electron/Chromium。
- 独立 daemon：关闭 GUI 不会停止服务，重新打开后恢复状态和日志。
- Windows 自定义 PowerShell/CMD 启动脚本。
- WSL 自定义 Bash 启动脚本。
- WSL 发行版、Docker Engine、已有容器自动发现。
- 已有容器直接启动、停止、重启和读取日志，不需要启动脚本或 Compose 文件。
- Windows Job Object 进程树管理。
- WSL systemd transient service 管理；没有 systemd 的发行版使用进程组后端。
- WSL systemd unit 和 Windows 进程树的 daemon 重启恢复。
- TCP/HTTP(S) 健康检查、依赖启动保护、异常退出退避重启。
- Named Pipe 当前用户 IPC、JSON 配置备份、日志轮转。
- GUI 新建和编辑 Windows/WSL 服务、状态筛选、实时日志和配置草稿保护。
- 深浅主题切换及主题偏好持久化。
- 系统托盘、关闭到托盘、托盘恢复窗口和单实例激活。
- 选中服务 CPU、内存、进程数、运行时长和有限历史趋势。
- 原创 SVG 图标、光学小尺寸 PNG、多尺寸 ICO 和 Windows EXE 资源。

产品目标是完整、可长期使用的桌面管理工具。安装和便携包脚本已提供，自动更新、配置导入导出和长期稳定性验证仍在继续完善，当前文档不把已完成的核心链路描述为全部产品工作已经结束。

## 环境要求

- Windows 11。
- Rust 1.92 或更新版本，以及 Cargo。
- WSL2；需要管理 WSL 服务时，发行版应提供 Bash。
- WSL 中的 Docker Engine，且当前 WSL 用户可以执行 `docker info`。
- WSL 服务推荐使用 systemd；systemd 不可用时回退到 Linux 进程组管理。
- HTTP(S) 健康检查使用 Windows 11 自带的 `curl.exe`。
- 从源码构建 Windows 图标资源时需要 Windows SDK 的 `rc.exe`；正常使用已生成的发布包不需要它。

## 构建

在项目根目录执行：

```bash
cargo build --workspace
```

如果依赖已经下载完成，离线构建和测试也可以执行：

```bash
cargo build --workspace --offline
cargo test --workspace --offline
```

格式化：

```bash
cargo fmt --all
```

## 启动

### 原生 GUI

```bash
cargo run -p server-on-gui
```

或者直接运行构建产物：

```text
target\debug\server-on.exe
```

GUI 会自动连接或启动同目录下的 `server-on-daemon.exe`。daemon 也可以单独启动：

```text
target\debug\server-on-daemon.exe
```

daemon 是后台管理进程，不需要保持 GUI 窗口打开。自动发现和 Docker/WSL 子进程都使用隐藏控制台方式，不应反复弹出 WSL 终端窗口。

### CLI

CLI 与 GUI 使用同一个 daemon，不是另一套独立服务状态：

```bash
# 查看 daemon 是否可用
server-on-cli ping

# 自动发现 WSL 发行版、Docker Engine 和容器
server-on-cli refresh

# 查看服务、容器及状态快照
server-on-cli status

# 启动、停止、重启，参数为服务名称或完整 ID
server-on-cli start my-api
server-on-cli stop my-api
server-on-cli restart my-api

# 查看最近日志
server-on-cli logs my-api --lines 100

# 只读查询当前服务的 CPU、内存、进程数和运行时长
server-on-cli metrics my-api
```

添加 Windows 服务：

```powershell
server-on-cli add-windows my-api "D:\work\api" "npm run dev"
```

添加 WSL 服务：

```powershell
server-on-cli add-wsl my-worker Ubuntu "/home/me/worker" "npm run worker"
```

在 Git Bash 中传递 WSL 路径时，需要禁止 MSYS 路径转换，否则 `/tmp` 等路径可能被转换成 Windows 路径：

```bash
MSYS_NO_PATHCONV=1 ./target/debug/server-on-cli.exe add-wsl my-worker Ubuntu /home/me/worker 'npm run worker'
```

WSL 工作目录必须是 Linux 绝对路径，例如 `/home/me/project` 或 `~/project`。

## 服务行为

### Windows 和 WSL 普通服务

启动脚本可以自行组合准备和启动步骤：

```bash
set -e

git pull
npm install
npm run build

exec npm run dev
```

脚本失败时不应继续执行后续步骤。长期服务需要保持前台运行，不要使用 `nohup`、`disown` 或 `&` 将进程隐藏到后台。

- Windows 使用 Job Object 管理整棵进程树，优先尝试正常退出，超时后结束 Job。
- 有 systemd 的 WSL 发行版使用 transient user service 和 `systemctl --user` 管理，日志通过 journal 获取。
- 非 systemd WSL 发行版使用独立 Linux 进程组，先 `SIGTERM`，超时后 `SIGKILL`。
- daemon 重启后会校验恢复信息。无法确认旧实例身份时显示 `Unknown`，不会盲目重复启动或按旧 PID 杀进程。

### WSL 中已有 Docker 容器

管理器自动发现容器并展示名称、镜像、状态、端口、健康状态和 Compose 标签。容器已经保存了镜像、启动命令、环境变量、挂载和端口配置，因此直接执行：

```text
docker start <container-id>
docker stop --time <seconds> <container-id>
docker restart --time <seconds> <container-id>
```

不会因为发现容器就自动启动或停止它。停止和删除是不同操作，默认不删除容器、网络或数据卷。Compose 标签用于分组，不要求提供 Compose 文件。

### 健康检查、依赖和异常重启

- 普通服务支持 TCP 和 HTTP(S) 健康检查。
- Docker 优先使用 Docker 自带 Healthcheck。
- `Running` 仅表示进程或容器运行，`Healthy` 才表示健康检查通过。
- 依赖为 `Healthy` 时，必须等健康检查通过后才能启动当前服务。
- 异常退出支持有限次数、延迟退避重启；手动停止或新的手动启动会使旧的延迟重试失效。

## 托盘、主题与资源指标

- 关闭主窗口默认隐藏到托盘，daemon 和服务继续运行；托盘菜单可恢复窗口、刷新发现或退出 GUI。
- 第二个 GUI 实例会激活已有窗口，不会创建第二个托盘图标。
- 顶部“浅色/深色”按钮切换主题，选择会保存到 daemon 配置。
- 概览页只采样当前选中服务：Windows 使用 Job 指标，WSL 使用 systemd/进程组指标，Docker 使用 `docker stats`。CPU 显示机器占比，首次采样没有差值时显示采样中。
- 图标母版和多尺寸生成说明见 `assets/icon/README.md`。

## 配置和日志

默认数据目录：

```text
%LOCALAPPDATA%\ServerOn\
  config.json       服务定义、策略和设置
  state.json        实例恢复信息
  logs\             按服务划分的日志
  backups\          配置备份
```

配置写入使用临时文件替换并保留备份，日志按大小轮转。GUI 关闭只断开客户端，不删除这些文件。

## 发布与安装

提供当前用户安装、开始菜单快捷方式、可选登录启动、卸载和便携包脚本，详见 `packaging/README.md`。

```powershell
# 生成 release 便携包
.\packaging\build-release.ps1

# 安装到当前用户目录并创建开始菜单快捷方式
.\packaging\install.ps1

# 可选：允许当前用户登录时启动 daemon
.\packaging\install.ps1 -StartOnLogin
```

安装和卸载脚本不会在开发过程中自动执行；卸载默认保留配置和日志。

## 代码结构

```text
crates/
  core/          配置、状态、发现模型、IPC 协议、数据路径
  daemon/        执行器、自动发现、健康检查、恢复、存储、IPC 客户端/服务端
  daemon-app/    daemon 可执行程序
  gui/           Slint 原生界面
  cli/           管理和诊断命令行
packaging/       release、安装和卸载脚本

DESIGN.md        产品设计目标与边界
VALIDATION.md    已执行的验证记录
```

## 验证

```bash
cargo fmt --all
cargo test --workspace --offline
cargo check --workspace --offline
```

除自动化测试外，已验证 Windows 进程、WSL systemd 服务、已有 Docker 容器、daemon 强制重启恢复，以及隐藏控制台下的自动发现。详见 `VALIDATION.md`。
