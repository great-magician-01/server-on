use crate::metrics::RawMetrics;
use anyhow::{Context, Result, anyhow, bail};
use server_on_core::config::{RuntimeConfig, ServiceDefinition};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::process::{Child, Command};
use tokio::time::timeout;
use uuid::Uuid;

#[cfg(windows)]
use crate::windows_process::{WindowsProcess, WindowsProcessController};

pub type ProcessReader = Box<dyn AsyncRead + Unpin + Send>;

pub struct SpawnedProcess {
    pub controller: ProcessController,
    pub monitor: ProcessMonitor,
    pub stdout: ProcessReader,
    pub stderr: ProcessReader,
    pub process_id: Option<u32>,
    pub process_group_id: Option<u32>,
    pub job_name: Option<String>,
    pub unit_name: Option<String>,
    pub recovery_identity: Option<String>,
}

pub enum ProcessMonitor {
    #[cfg(windows)]
    Windows(WindowsProcess),
    Wsl(Child),
    WslRecovered {
        distribution: String,
        process_group_id: u32,
        recovery_identity: String,
    },
    Systemd {
        distribution: String,
        unit_name: String,
        log_process: Child,
    },
}

#[derive(Clone)]
pub enum ProcessController {
    #[cfg(windows)]
    Windows(WindowsProcessController),
    Wsl {
        distribution: String,
        process_group_id: u32,
        recovery_identity: String,
    },
    Systemd {
        distribution: String,
        unit_name: String,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct ProcessExit {
    pub code: Option<i32>,
    pub expected: bool,
}

impl SpawnedProcess {
    pub async fn spawn(definition: &ServiceDefinition) -> Result<Self> {
        match &definition.runtime {
            RuntimeConfig::WindowsScript {
                working_directory,
                shell,
                script,
                environment,
            } => {
                #[cfg(windows)]
                {
                    let spawned =
                        WindowsProcess::spawn(*shell, working_directory, script, environment)?;
                    let process_id = spawned.process.pid();
                    let job_name = spawned.process.job_name().to_owned();
                    Ok(Self {
                        controller: ProcessController::Windows(spawned.controller),
                        monitor: ProcessMonitor::Windows(spawned.process),
                        stdout: Box::new(spawned.stdout),
                        stderr: Box::new(spawned.stderr),
                        process_id: Some(process_id),
                        process_group_id: None,
                        job_name: Some(job_name),
                        unit_name: None,
                        recovery_identity: None,
                    })
                }
                #[cfg(not(windows))]
                {
                    let _ = (working_directory, shell, script, environment);
                    bail!("Windows 脚本服务只能在 Windows 上运行")
                }
            }
            RuntimeConfig::WslScript {
                distribution,
                working_directory,
                script,
                environment,
            } => {
                if systemd_available(distribution).await {
                    spawn_systemd(
                        distribution,
                        working_directory,
                        script,
                        environment,
                        definition.stop_timeout_seconds,
                    )
                    .await
                } else {
                    spawn_wsl_process_group(distribution, working_directory, script, environment)
                        .await
                }
            }
            RuntimeConfig::DockerContainer { .. } => {
                bail!("Docker 容器不是脚本进程，应使用 Docker 生命周期管理")
            }
        }
    }
}

impl ProcessMonitor {
    pub async fn wait(&mut self) -> Result<ProcessExit> {
        match self {
            #[cfg(windows)]
            Self::Windows(process) => Ok(ProcessExit {
                code: Some(process.wait().await?),
                expected: false,
            }),
            Self::Wsl(child) => {
                let status = child.wait().await?;
                Ok(ProcessExit {
                    code: status.code(),
                    expected: false,
                })
            }
            Self::WslRecovered {
                distribution,
                process_group_id,
                recovery_identity,
            } => loop {
                match wsl_group_status(distribution, *process_group_id, recovery_identity).await? {
                    WslGroupStatus::Missing => {
                        return Ok(ProcessExit {
                            code: None,
                            expected: true,
                        });
                    }
                    WslGroupStatus::Matching => {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                    }
                    WslGroupStatus::Different => {
                        // The original group is gone and this PGID now belongs
                        // to an unrelated process group. Treat the managed
                        // instance as exited without touching the new group.
                        return Ok(ProcessExit {
                            code: None,
                            expected: true,
                        });
                    }
                }
            },
            Self::Systemd {
                distribution,
                unit_name,
                log_process,
            } => loop {
                if !systemd_is_active(distribution, unit_name).await? {
                    let code = systemd_exit_code(distribution, unit_name).await;
                    let _ = log_process.kill().await;
                    return Ok(ProcessExit {
                        code,
                        expected: code == Some(0),
                    });
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            },
        }
    }
}

impl ProcessController {
    pub async fn stop(&self, timeout_duration: Duration) -> Result<()> {
        match self {
            #[cfg(windows)]
            Self::Windows(controller) => controller.stop(timeout_duration).await,
            Self::Wsl {
                distribution,
                process_group_id,
                recovery_identity,
            } => {
                match wsl_group_status(distribution, *process_group_id, recovery_identity).await? {
                    WslGroupStatus::Missing => return Ok(()),
                    WslGroupStatus::Different => {
                        // The managed group exited and the PGID was reused.
                        // There is nothing left to stop, and the new owner must
                        // not receive a signal.
                        return Ok(());
                    }
                    WslGroupStatus::Matching => {}
                }
                signal_wsl_group(distribution, *process_group_id, "TERM").await?;
                let deadline = tokio::time::Instant::now() + timeout_duration;
                while tokio::time::Instant::now() < deadline {
                    if !wsl_group_alive(distribution, *process_group_id).await? {
                        return Ok(());
                    }
                    tokio::time::sleep(Duration::from_millis(150)).await;
                }
                match wsl_group_status(distribution, *process_group_id, recovery_identity).await? {
                    WslGroupStatus::Missing | WslGroupStatus::Different => Ok(()),
                    WslGroupStatus::Matching => {
                        signal_wsl_group(distribution, *process_group_id, "KILL").await
                    }
                }
            }
            Self::Systemd {
                distribution,
                unit_name,
            } => stop_systemd(distribution, unit_name, timeout_duration).await,
        }
    }
}

async fn spawn_systemd(
    distribution: &str,
    working_directory: &str,
    script: &str,
    environment: &std::collections::BTreeMap<String, String>,
    stop_timeout_seconds: u64,
) -> Result<SpawnedProcess> {
    let unit_name = format!("server-on-{}.service", Uuid::new_v4().simple());
    let working_directory_property = format!("WorkingDirectory={working_directory}");
    let timeout_property = format!("TimeoutStopSec={stop_timeout_seconds}");
    let mut arguments = vec![
        "systemd-run".to_owned(),
        "--user".into(),
        format!("--unit={unit_name}"),
        "--collect".into(),
        format!("--property={working_directory_property}"),
        format!("--property={timeout_property}"),
        "--property=Restart=no".into(),
    ];
    for (key, value) in environment {
        arguments.push(format!("--setenv={key}={value}"));
    }
    arguments.extend(["--".into(), "bash".into(), "-lc".into(), script.into()]);
    let argument_refs: Vec<&str> = arguments.iter().map(String::as_str).collect();
    let output = wsl_output(distribution, &argument_refs).await?;
    if !output.status.success() {
        bail!("创建 WSL systemd 服务失败：{}", output_error(&output));
    }
    match make_systemd_process(distribution, &unit_name).await {
        Ok(process) => Ok(process),
        Err(error) => {
            let _ = stop_systemd(distribution, &unit_name, Duration::from_secs(5)).await;
            Err(error)
        }
    }
}

pub async fn recover_systemd(distribution: &str, unit_name: &str) -> Result<SpawnedProcess> {
    if !systemd_is_active(distribution, unit_name).await? {
        bail!("WSL systemd 服务已经退出");
    }
    make_systemd_process(distribution, unit_name).await
}

async fn make_systemd_process(distribution: &str, unit_name: &str) -> Result<SpawnedProcess> {
    let process_id = systemd_main_pid(distribution, unit_name).await;
    let output = Command::new("wsl.exe");
    let mut command = output;
    command
        .arg("--distribution")
        .arg(distribution)
        .arg("--")
        .arg("journalctl")
        .arg("--user")
        .arg("--unit")
        .arg(unit_name)
        .arg("--follow")
        .arg("--lines=0")
        .arg("--no-pager")
        .arg("--output=cat")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::command::hide_console(&mut command);
    let mut log_process = command.spawn().context("无法读取 WSL systemd 日志")?;
    let stdout = log_process
        .stdout
        .take()
        .ok_or_else(|| anyhow!("无法捕获 systemd 标准输出"))?;
    let stderr = log_process
        .stderr
        .take()
        .ok_or_else(|| anyhow!("无法捕获 systemd 标准错误"))?;
    Ok(SpawnedProcess {
        controller: ProcessController::Systemd {
            distribution: distribution.to_owned(),
            unit_name: unit_name.to_owned(),
        },
        monitor: ProcessMonitor::Systemd {
            distribution: distribution.to_owned(),
            unit_name: unit_name.to_owned(),
            log_process,
        },
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        process_id,
        process_group_id: None,
        job_name: None,
        unit_name: Some(unit_name.to_owned()),
        recovery_identity: None,
    })
}

async fn systemd_main_pid(distribution: &str, unit_name: &str) -> Option<u32> {
    let output = wsl_output(
        distribution,
        &[
            "systemctl",
            "--user",
            "show",
            unit_name,
            "--property=MainPID",
            "--value",
        ],
    )
    .await
    .ok()?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u32>()
        .ok()
        .filter(|pid| *pid > 0)
}

async fn systemd_available(distribution: &str) -> bool {
    let Ok(output) = wsl_output(distribution, &["systemctl", "--user", "is-system-running"]).await
    else {
        return false;
    };
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    output.status.success() || matches!(state, "running" | "degraded" | "starting" | "initializing")
}

pub async fn systemd_is_active(distribution: &str, unit_name: &str) -> Result<bool> {
    let output = wsl_output(
        distribution,
        &[
            "systemctl",
            "--user",
            "show",
            unit_name,
            "--property=ActiveState",
            "--value",
        ],
    )
    .await?;
    if !output.status.success() {
        bail!("读取 WSL systemd 状态失败：{}", output_error(&output));
    }
    let state = String::from_utf8_lossy(&output.stdout);
    Ok(matches!(
        state.trim(),
        "active" | "activating" | "reloading"
    ))
}

async fn systemd_exit_code(distribution: &str, unit_name: &str) -> Option<i32> {
    let output = wsl_output(
        distribution,
        &[
            "systemctl",
            "--user",
            "show",
            unit_name,
            "--property=ExecMainStatus",
            "--value",
        ],
    )
    .await
    .ok()?;
    String::from_utf8_lossy(&output.stdout).trim().parse().ok()
}

async fn stop_systemd(distribution: &str, unit_name: &str, timeout: Duration) -> Result<()> {
    if !systemd_is_active(distribution, unit_name).await? {
        return Ok(());
    }
    let output = wsl_output(distribution, &["systemctl", "--user", "stop", unit_name]).await?;
    if !output.status.success() {
        bail!("停止 WSL systemd 服务失败：{}", output_error(&output));
    }
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if !systemd_is_active(distribution, unit_name).await? {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    let output = wsl_output(
        distribution,
        &[
            "systemctl",
            "--user",
            "kill",
            "--kill-who=all",
            "--signal=SIGKILL",
            unit_name,
        ],
    )
    .await?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("强制停止 WSL systemd 服务失败：{}", output_error(&output));
    }
}

pub async fn journal_logs(
    distribution: &str,
    unit_name: &str,
    limit: usize,
) -> Result<Vec<String>> {
    let limit = limit.clamp(1, 10_000).to_string();
    let output = wsl_output(
        distribution,
        &[
            "journalctl",
            "--user",
            "--unit",
            unit_name,
            "--lines",
            &limit,
            "--no-pager",
            "--output=cat",
        ],
    )
    .await?;
    if !output.status.success() {
        bail!("读取 WSL systemd 日志失败：{}", output_error(&output));
    }
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

pub async fn systemd_metrics(distribution: &str, unit_name: &str) -> Result<RawMetrics> {
    let output = wsl_output(
        distribution,
        &[
            "systemctl",
            "--user",
            "show",
            unit_name,
            "--property=CPUUsageNSec,MemoryCurrent,TasksCurrent",
        ],
    )
    .await?;
    if !output.status.success() {
        bail!("读取 WSL systemd 指标失败：{}", output_error(&output));
    }
    Ok(crate::metrics::parse_systemd_properties(
        &String::from_utf8_lossy(&output.stdout),
    ))
}

pub async fn process_group_metrics(
    distribution: &str,
    process_group_id: u32,
) -> Result<RawMetrics> {
    let output = wsl_output(distribution, &["ps", "-e", "-o", "pgid=,rss=,cputimes="]).await?;
    if !output.status.success() {
        bail!("读取 WSL 进程组指标失败：{}", output_error(&output));
    }
    Ok(crate::metrics::parse_ps_metrics(
        &String::from_utf8_lossy(&output.stdout),
        process_group_id,
    ))
}

async fn spawn_wsl_process_group(
    distribution: &str,
    working_directory: &str,
    script: &str,
    environment: &std::collections::BTreeMap<String, String>,
) -> Result<SpawnedProcess> {
    let mut command = Command::new("wsl.exe");
    command
        .arg("--distribution")
        .arg(distribution)
        .arg("--cd")
        .arg(working_directory)
        .arg("--")
        .arg("env");
    for (key, value) in environment {
        command.arg(format!("{key}={value}"));
    }
    let launch_script = format!(
        "printf '__SERVER_ON_PGID__%s\\n' \"$$\"; exec bash -lc {}",
        shell_quote(script)
    );
    command
        .arg("setsid")
        .arg("--wait")
        .arg("bash")
        .arg("-c")
        .arg(launch_script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    crate::command::hide_console(&mut command);
    let mut child = command.spawn().context("无法启动 WSL 服务进程")?;
    let process_id = child.id();
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("无法捕获 WSL 标准输出"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("无法捕获 WSL 标准错误"))?;
    let mut stdout = BufReader::new(stdout);
    let mut marker = String::new();
    let read = timeout(Duration::from_secs(10), stdout.read_line(&mut marker)).await;
    match read {
        Ok(Ok(count)) if count > 0 => {}
        Ok(Ok(_)) => {
            let _ = child.kill().await;
            bail!("WSL 服务在返回进程组信息前退出")
        }
        Ok(Err(error)) => {
            let _ = child.kill().await;
            return Err(error).context("读取 WSL 进程组信息失败");
        }
        Err(_) => {
            let _ = child.kill().await;
            bail!("等待 WSL 进程组信息超时");
        }
    }
    let process_group_id = marker
        .trim()
        .strip_prefix("__SERVER_ON_PGID__")
        .ok_or_else(|| anyhow!("WSL 进程组标识无效：{:?}", marker.trim()))?
        .parse::<u32>()?;
    let recovery_identity = match wsl_group_identity(distribution, process_group_id).await {
        Ok(identity) => identity,
        Err(error) => {
            let _ = signal_wsl_group(distribution, process_group_id, "KILL").await;
            let _ = child.kill().await;
            return Err(error).context("无法记录 WSL 进程组恢复身份");
        }
    };
    Ok(SpawnedProcess {
        controller: ProcessController::Wsl {
            distribution: distribution.to_owned(),
            process_group_id,
            recovery_identity: recovery_identity.clone(),
        },
        monitor: ProcessMonitor::Wsl(child),
        stdout: Box::new(stdout),
        stderr: Box::new(stderr),
        process_id,
        process_group_id: Some(process_group_id),
        job_name: None,
        unit_name: None,
        recovery_identity: Some(recovery_identity),
    })
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

async fn signal_wsl_group(distribution: &str, process_group_id: u32, signal: &str) -> Result<()> {
    let group = format!("-{process_group_id}");
    let output = wsl_output(distribution, &["kill", &format!("-{signal}"), "--", &group]).await?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("向 WSL 进程组 {process_group_id} 发送 {signal} 失败")
    }
}

pub async fn wsl_group_alive(distribution: &str, process_group_id: u32) -> Result<bool> {
    let output = wsl_output(distribution, &["ps", "-e", "-o", "pgid="]).await?;
    if !output.status.success() {
        bail!("读取 WSL 进程组状态失败：{}", output_error(&output));
    }
    Ok(process_group_list_contains(
        &String::from_utf8_lossy(&output.stdout),
        process_group_id,
    ))
}

fn process_group_list_contains(output: &str, process_group_id: u32) -> bool {
    output
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .any(|candidate| candidate == process_group_id)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WslGroupStatus {
    Missing,
    Matching,
    Different,
}

pub async fn wsl_group_status(
    distribution: &str,
    process_group_id: u32,
    expected_identity: &str,
) -> Result<WslGroupStatus> {
    if !wsl_group_alive(distribution, process_group_id).await? {
        return Ok(WslGroupStatus::Missing);
    }
    let identity = match wsl_group_identity(distribution, process_group_id).await {
        Ok(identity) => identity,
        Err(error) => {
            // The leader may exit between the PGID probe and reading its
            // identity. Re-check before treating a normal exit as ambiguity.
            if !wsl_group_alive(distribution, process_group_id).await? {
                return Ok(WslGroupStatus::Missing);
            }
            return Err(error);
        }
    };
    if identity == expected_identity {
        Ok(WslGroupStatus::Matching)
    } else {
        Ok(WslGroupStatus::Different)
    }
}

pub async fn wsl_group_identity(distribution: &str, process_group_id: u32) -> Result<String> {
    let boot = wsl_output(distribution, &["cat", "/proc/sys/kernel/random/boot_id"]).await?;
    if !boot.status.success() {
        bail!("读取 WSL boot ID 失败：{}", output_error(&boot));
    }
    let stat_path = format!("/proc/{process_group_id}/stat");
    let stat = wsl_output(distribution, &["cat", &stat_path]).await?;
    if !stat.status.success() {
        bail!("读取 WSL 进程组领导进程身份失败：{}", output_error(&stat));
    }
    let boot = String::from_utf8_lossy(&boot.stdout);
    let boot = boot.trim();
    if boot.is_empty() {
        bail!("WSL boot ID 为空");
    }
    let stat = String::from_utf8_lossy(&stat.stdout);
    let started_at = linux_stat_start_time(&stat)?;
    Ok(format!("{boot}:{started_at}"))
}

fn linux_stat_start_time(stat: &str) -> Result<&str> {
    let command_end = stat
        .rfind(')')
        .ok_or_else(|| anyhow!("Linux /proc stat 缺少命令名结束标记"))?;
    stat[command_end + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| anyhow!("Linux /proc stat 缺少进程启动时间"))
}

async fn wsl_output(distribution: &str, arguments: &[&str]) -> Result<std::process::Output> {
    let mut command = Command::new("wsl.exe");
    command
        .arg("--distribution")
        .arg(distribution)
        .arg("--")
        .args(arguments);
    crate::command::hide_console(&mut command);
    crate::command::output(
        &mut command,
        crate::command::DEFAULT_COMMAND_TIMEOUT,
        &format!("无法在 WSL 发行版 {distribution} 中执行命令"),
    )
    .await
}

fn output_error(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let message = if stderr.trim().is_empty() {
        stdout
    } else {
        stderr
    };
    format!(
        "{}（退出码 {}）",
        message.trim(),
        output.status.code().unwrap_or(-1)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_group_lookup_requires_an_exact_id() {
        let output = "  1\n  123\n  1234\n";
        assert!(process_group_list_contains(output, 123));
        assert!(!process_group_list_contains(output, 12));
        assert!(!process_group_list_contains(output, 999));
    }

    #[test]
    fn linux_identity_parser_handles_spaces_and_parentheses_in_command_names() {
        let stat =
            "123 (bash (test)) S 1 123 123 0 -1 4194304 100 0 0 0 10 5 0 0 20 0 1 0 987654 1000";
        assert_eq!(linux_stat_start_time(stat).unwrap(), "987654");
        assert!(linux_stat_start_time("invalid").is_err());
    }
}
