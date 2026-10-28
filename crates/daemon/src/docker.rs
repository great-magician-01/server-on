use crate::metrics::{parse_memory_bytes, parse_percentage};
use anyhow::{Result, bail};
use serde_json::Value;
use server_on_core::discovery::ContainerInfo;
use server_on_core::state::{HealthState, ServiceState};
use std::process::Output;
use tokio::process::Command;

pub async fn start(distribution: &str, container_id: &str) -> Result<ContainerInfo> {
    run(distribution, &["docker", "start", container_id]).await?;
    inspect(distribution, container_id).await
}

pub async fn stop(
    distribution: &str,
    container_id: &str,
    timeout_seconds: u64,
) -> Result<ContainerInfo> {
    let timeout = timeout_seconds.to_string();
    run_with_timeout(
        distribution,
        &["docker", "stop", "--time", &timeout, container_id],
        std::time::Duration::from_secs(timeout_seconds.saturating_add(30)),
    )
    .await?;
    inspect(distribution, container_id).await
}

pub async fn restart(
    distribution: &str,
    container_id: &str,
    timeout_seconds: u64,
) -> Result<ContainerInfo> {
    let timeout = timeout_seconds.to_string();
    run_with_timeout(
        distribution,
        &["docker", "restart", "--time", &timeout, container_id],
        std::time::Duration::from_secs(timeout_seconds.saturating_add(30)),
    )
    .await?;
    inspect(distribution, container_id).await
}

pub async fn inspect(distribution: &str, container_id: &str) -> Result<ContainerInfo> {
    let output = run(distribution, &["docker", "inspect", container_id]).await?;
    let values: Vec<Value> = serde_json::from_slice(&output.stdout)?;
    let value = values
        .first()
        .ok_or_else(|| anyhow::anyhow!("Docker 未返回容器信息"))?;
    let labels = value
        .pointer("/Config/Labels")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let ports = value
        .pointer("/NetworkSettings/Ports")
        .and_then(Value::as_object)
        .map(|ports| {
            let mut ports: Vec<String> = ports
                .iter()
                .map(|(container_port, bindings)| {
                    if let Some(binding) = bindings
                        .as_array()
                        .and_then(|items| items.first())
                        .and_then(Value::as_object)
                    {
                        format!(
                            "{}:{} -> {}",
                            binding
                                .get("HostIp")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            binding
                                .get("HostPort")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                            container_port
                        )
                    } else {
                        container_port.clone()
                    }
                })
                .collect();
            ports.sort();
            ports
        })
        .unwrap_or_default();
    Ok(ContainerInfo {
        id: value
            .get("Id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        name: value
            .get("Name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_start_matches('/')
            .into(),
        image: value
            .pointer("/Config/Image")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
        state: value
            .pointer("/State/Status")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .into(),
        health: value
            .pointer("/State/Health/Status")
            .and_then(Value::as_str)
            .map(str::to_owned),
        started_at: value
            .pointer("/State/StartedAt")
            .and_then(Value::as_str)
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            .map(|value| value.with_timezone(&chrono::Utc)),
        ports,
        compose_project: labels
            .get("com.docker.compose.project")
            .and_then(Value::as_str)
            .map(str::to_owned),
        compose_service: labels
            .get("com.docker.compose.service")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

pub fn container_status(container: &ContainerInfo) -> (ServiceState, HealthState, Option<String>) {
    let health = match container.health.as_deref() {
        Some("healthy") => HealthState::Healthy,
        Some("unhealthy") => HealthState::Unhealthy,
        Some("starting") => HealthState::Checking,
        _ => HealthState::NotConfigured,
    };
    let state = match container.state.as_str() {
        "running" => ServiceState::Running,
        "created" | "exited" => ServiceState::Stopped,
        "restarting" => ServiceState::Starting,
        "paused" => ServiceState::Degraded,
        "dead" => ServiceState::Failed,
        _ => ServiceState::Unknown,
    };
    (state, health, None)
}

#[derive(Clone, Debug, Default)]
pub struct DockerStats {
    pub cpu_percent: Option<f64>,
    pub memory_bytes: Option<u64>,
    pub process_count: Option<u32>,
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
}

pub async fn stats(distribution: &str, container_id: &str) -> Result<DockerStats> {
    let output = run(
        distribution,
        &[
            "docker",
            "stats",
            "--no-stream",
            "--format",
            "{{json .}}",
            container_id,
        ],
    )
    .await?;
    let value: Value = serde_json::from_slice(&output.stdout)?;
    let mut stats = parse_docker_stats(&value);
    stats.started_at = inspect(distribution, container_id).await?.started_at;
    Ok(stats)
}

fn parse_docker_stats(value: &Value) -> DockerStats {
    DockerStats {
        cpu_percent: value
            .get("CPUPerc")
            .and_then(Value::as_str)
            .and_then(parse_percentage),
        memory_bytes: value
            .get("MemUsage")
            .and_then(Value::as_str)
            .and_then(|value| value.split('/').next())
            .and_then(parse_memory_bytes),
        process_count: value
            .get("PIDs")
            .and_then(Value::as_str)
            .and_then(|value| value.trim().parse().ok()),
        started_at: None,
    }
}

pub async fn logs(distribution: &str, container_id: &str, lines: usize) -> Result<Vec<String>> {
    let lines = lines.clamp(1, 10_000).to_string();
    let output = run(
        distribution,
        &[
            "docker",
            "logs",
            "--tail",
            &lines,
            "--timestamps",
            container_id,
        ],
    )
    .await?;
    let mut entries = Vec::new();
    for (stream, bytes) in [("stdout", output.stdout), ("stderr", output.stderr)] {
        for line in String::from_utf8_lossy(&bytes).lines() {
            entries.push(format!("[{stream}] {line}"));
        }
    }
    Ok(entries)
}

async fn run(distribution: &str, arguments: &[&str]) -> Result<Output> {
    run_with_timeout(
        distribution,
        arguments,
        crate::command::DEFAULT_COMMAND_TIMEOUT,
    )
    .await
}

async fn run_with_timeout(
    distribution: &str,
    arguments: &[&str],
    timeout_duration: std::time::Duration,
) -> Result<Output> {
    let mut command = Command::new("wsl.exe");
    command
        .arg("--distribution")
        .arg(distribution)
        .arg("--")
        .args(arguments);
    crate::command::hide_console(&mut command);
    let output = crate::command::output(
        &mut command,
        timeout_duration,
        &format!("无法在 WSL 发行版 {distribution} 中执行 Docker 命令"),
    )
    .await?;
    if output.status.success() {
        Ok(output)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let message = if stderr.trim().is_empty() {
            stdout
        } else {
            stderr
        };
        bail!("{}", message.trim())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_docker_stats_fields() {
        let value = serde_json::json!({
            "CPUPerc": "12.5%",
            "MemUsage": "1.5MiB / 4GiB",
            "PIDs": "17"
        });
        let stats = parse_docker_stats(&value);
        assert_eq!(stats.cpu_percent, Some(12.5));
        assert_eq!(stats.memory_bytes, Some(1_572_864));
        assert_eq!(stats.process_count, Some(17));
    }
}
