use anyhow::{Context, Result, bail};
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
    run(
        distribution,
        &["docker", "stop", "--time", &timeout, container_id],
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
    run(
        distribution,
        &["docker", "restart", "--time", &timeout, container_id],
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
    let mut command = Command::new("wsl.exe");
    command
        .arg("--distribution")
        .arg(distribution)
        .arg("--")
        .args(arguments);
    crate::command::hide_console(&mut command);
    let output = command
        .output()
        .await
        .with_context(|| format!("无法在 WSL 发行版 {distribution} 中执行 Docker 命令"))?;
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
