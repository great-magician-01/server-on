# 验证记录

更新时间：2026-10-26。

## 自动化验证

执行命令：

```bash
cargo fmt --all
cargo test --workspace --offline
cargo check --workspace --offline
cargo build --workspace --offline
cargo clippy --workspace --offline -- -D warnings
```

结果：通过。

当前自动化测试覆盖：

- 配置版本、重复 ID 和循环依赖校验。
- WSL UTF-16 输出解码及发行版解析。
- Docker inspect 的容器状态、健康状态和 Compose 标签解析。
- 配置原子保存、损坏配置从备份恢复。
- 日志轮转和最近日志读取。
- 本地 TCP 健康检查。
- 本地 HTTP 健康检查及状态码判断（Windows `curl.exe`）。

## 真实环境验证

以下验证均使用临时服务或临时容器，不依赖 mock：

### Windows 进程

- 创建 PowerShell 长驻脚本服务。
- 启动成功并持续写入日志。
- 停止后服务状态为停止，测试进程不再存在。
- daemon 强制结束后，原服务仍以原 PID 运行；重新连接后恢复原实例，没有重复启动。
- 恢复后的停止和清理成功。

### WSL 进程和 systemd

- 通过 Bash 自定义脚本启动服务并读取日志。
- 在 Ubuntu systemd 环境中创建 transient user service。
- 通过 `systemctl --user` 停止服务，退出状态正确。
- daemon 强制结束后，当前 systemd unit 仍为 active；重新连接后恢复原 unit，随后停止成功。
- 非 systemd 后端保留独立进程组管理，不能确认旧进程已存活时不会伪报恢复成功。

### Docker

- 自动发现 Ubuntu 和 Docker Engine。
- 发现已有 `mysql8.4` 与 `postgres` 容器及其状态、镜像和端口。
- 使用临时 `sleep` 容器验证发现、停止、再次启动、再次停止和清理。
- 未对已有 MySQL/PostgreSQL 容器执行启停。

### GUI 与 IPC

- Slint 原生窗口实际启动并显示真实服务。
- GUI 自动连接或启动 daemon。
- Named Pipe 当前用户连接和 CLI `ping/status/refresh` 正常。
- GUI、CLI 和 daemon 使用同一份快照与事件协议。
- 后台自动发现已恢复，并通过隐藏控制台方式运行。
- 已处理自动刷新覆盖编辑中配置的问题，新建草稿和编辑中的表单不会被后台快照直接覆盖。

## 控制台窗口问题回归

此前 daemon 定时调用 `wsl.exe` 时未统一设置隐藏窗口标志，导致终端窗口反复闪现。处理过程：

1. 停止开发 daemon，确认停止重复弹窗。
2. 在统一命令边界为 WSL、Docker、WSL 启停子进程设置 `CREATE_NO_WINDOW`。
3. 暂时关闭定时扫描，验证手动扫描和 WSL 启停。
4. 用户确认不再弹窗后，恢复隐藏控制台下的自动发现。
5. 最新 GUI/daemon 联合运行未发现运行时错误输出。

## 本轮产品体验验证

### 图标

- 原创 `Tri-Runtime S-Rail` SVG 已生成完整 PNG、透明托盘 PNG 和多尺寸 ICO。
- ICO 已验证包含 16/20/24/32/40/48/64/128/256 九种尺寸。
- 使用 Windows SDK `rc.exe` 嵌入 GUI EXE，已通过 `Icon.ExtractAssociatedIcon` 验证可提取。
- `build.rs` 已处理 SDK include 路径，离线资源构建通过。

### 托盘与主题

- Slint 原生托盘组件已编译并运行，主窗口关闭后 GUI 进程仍保留。
- 第二次启动只激活已有窗口，没有创建第二个 GUI 进程或托盘。
- 浅色/深色主题均已通过原生 UI 自动化实际切换和截图验证。
- 主题偏好通过 daemon `GetSettings/UpdateSettings` 持久化。

### 资源指标

- Windows 临时服务：首次 CPU 为 `null`，第二次返回有效百分比，内存约 79 MB、进程数 2。
- WSL systemd 临时服务：CPU、内存和进程数均返回有效值，随后正常停止清理。
- 现有 PostgreSQL 容器：只读返回 CPU、内存和 PID 数，没有执行启停。
- 指标走独立 `GetMetrics` 只读通道，不修改服务状态、恢复数据或触发重启。
- 已修复历史趋势固定宽度导致的指标卡溢出，改为最多 12 个趋势点，概览布局在深色和浅色下均可用。

## 发布验证

- `cargo build --release --workspace --offline` 通过。
- `packaging/install.ps1`、`uninstall.ps1`、`build-release.ps1` 已通过 PowerShell 纯语法检查。
- 已生成 `dist/ServerOn-windows-x64.zip`，包含 GUI、daemon、CLI 和三份文档。
- 未执行安装、卸载或登录启动项修改。

## 尚未作为本次验证结论的内容

配置导入导出、自动更新和长时间稳定性测试仍属于后续产品工作，不在上述“全部产品工作完成”的含义之内。
