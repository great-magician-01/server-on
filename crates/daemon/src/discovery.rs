use anyhow::{Result, bail};
use serde_json::Value;
use server_on_core::discovery::{
    ContainerInfo, DiscoverySnapshot, DistributionInfo, DockerEngineInfo, DockerEngineState,
    WslDistributionState,
};
use std::process::Output;
use tokio::process::Command;

pub struct DiscoveryManager {
    scan_stopped_distributions: bool,
}

impl DiscoveryManager {
    pub fn new(scan_stopped_distributions: bool) -> Self {
        Self {
            scan_stopped_distributions,
        }
    }

    pub async fn discover(&self) -> Result<DiscoverySnapshot> {
        let mut command = Command::new("wsl.exe");
        command.args(["--list", "--verbose"]);
        crate::command::hide_console(&mut command);
        let output = crate::command::output(
            &mut command,
            crate::command::DEFAULT_COMMAND_TIMEOUT,
            "无法执行 wsl.exe；请确认已安装 WSL",
        )
        .await?;
        if !output.status.success() {
            bail!("枚举 WSL 发行版失败：{}", output_error(&output));
        }
        let distributions = parse_wsl_verbose(&decode_windows_output(&output.stdout));
        let mut result = Vec::with_capacity(distributions.len());
        for distribution in distributions {
            result.push(self.inspect_distribution(distribution).await);
        }
        Ok(DiscoverySnapshot {
            distributions: result,
        })
    }

    async fn inspect_distribution(&self, mut distribution: DistributionInfo) -> DistributionInfo {
        if distribution.state == WslDistributionState::Stopped && !self.scan_stopped_distributions {
            distribution.docker = DockerEngineInfo::unavailable(
                DockerEngineState::NotScanned,
                "发行版未运行；默认不为了扫描而启动它",
            );
            return distribution;
        }

        let output = match wsl_output(
            &distribution.name,
            &["docker", "info", "--format", "{{json .}}"],
        )
        .await
        {
            Ok(output) => output,
            Err(error) => {
                distribution.docker =
                    DockerEngineInfo::unavailable(DockerEngineState::Unknown, error.to_string());
                return distribution;
            }
        };
        if !output.status.success() {
            let message = output_error(&output);
            distribution.docker =
                DockerEngineInfo::unavailable(classify_docker_error(&message), message);
            return distribution;
        }

        let engine_id = serde_json::from_slice::<Value>(&output.stdout)
            .ok()
            .and_then(|value| value.get("ID")?.as_str().map(str::to_owned))
            .unwrap_or_else(|| "default".into());
        distribution.docker = DockerEngineInfo {
            state: DockerEngineState::Available,
            engine_id: Some(engine_id),
            message: None,
        };
        match inspect_containers(&distribution.name).await {
            Ok(containers) => distribution.containers = containers,
            Err(error) => {
                distribution.docker = DockerEngineInfo::unavailable(
                    DockerEngineState::Unknown,
                    format!("读取容器失败：{error:#}"),
                );
            }
        }
        distribution
    }
}

async fn inspect_containers(distribution: &str) -> Result<Vec<ContainerInfo>> {
    let output = wsl_output(
        distribution,
        &[
            "docker",
            "container",
            "ls",
            "--all",
            "--quiet",
            "--no-trunc",
        ],
    )
    .await?;
    if !output.status.success() {
        bail!(output_error(&output));
    }
    let ids: Vec<String> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    let mut containers = Vec::new();
    for chunk in ids.chunks(32) {
        let mut arguments = vec!["docker", "inspect"];
        arguments.extend(chunk.iter().map(String::as_str));
        let output = wsl_output(distribution, &arguments).await?;
        if !output.status.success() {
            bail!(output_error(&output));
        }
        let values: Vec<Value> = serde_json::from_slice(&output.stdout)?;
        containers.extend(values.iter().map(container_from_inspect));
    }
    containers.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(containers)
}

fn container_from_inspect(value: &Value) -> ContainerInfo {
    let labels = value
        .pointer("/Config/Labels")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let mut ports: Vec<String> = value
        .pointer("/NetworkSettings/Ports")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .map(|(container_port, bindings)| {
                    if let Some(binding) = bindings
                        .as_array()
                        .and_then(|items| items.first())
                        .and_then(Value::as_object)
                    {
                        let host = binding
                            .get("HostIp")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let port = binding
                            .get("HostPort")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        format!("{host}:{port} -> {container_port}")
                    } else {
                        container_port.clone()
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    ports.sort();
    ContainerInfo {
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
    }
}

async fn wsl_output(distribution: &str, arguments: &[&str]) -> Result<Output> {
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

fn classify_docker_error(message: &str) -> DockerEngineState {
    let message = message.to_lowercase();
    if message.contains("permission denied") || message.contains("access denied") {
        DockerEngineState::PermissionDenied
    } else if message.contains("command not found")
        || message.contains("no such file or directory")
        || message.contains("not recognized")
    {
        DockerEngineState::NotInstalled
    } else if message.contains("cannot connect")
        || message.contains("is the docker daemon running")
        || message.contains("docker daemon")
    {
        DockerEngineState::Stopped
    } else {
        DockerEngineState::Unknown
    }
}

fn output_error(output: &Output) -> String {
    let stderr = decode_windows_output(&output.stderr);
    let stdout = decode_windows_output(&output.stdout);
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

fn decode_windows_output(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    let (little_endian, offset) = if bytes.starts_with(&[0xff, 0xfe]) {
        (true, 2)
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        (false, 2)
    } else {
        let sample = &bytes[..bytes.len().min(512)];
        let odd_nuls = sample
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|byte| **byte == 0)
            .count();
        let even_nuls = sample.iter().step_by(2).filter(|byte| **byte == 0).count();
        let pairs = (sample.len() / 2).max(1);
        if odd_nuls * 4 > pairs {
            (true, 0)
        } else if even_nuls * 4 > pairs {
            (false, 0)
        } else {
            return String::from_utf8_lossy(bytes).into_owned();
        }
    };
    let words: Vec<u16> = bytes[offset..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect();
    String::from_utf16_lossy(&words)
}

fn parse_wsl_verbose(input: &str) -> Vec<DistributionInfo> {
    input
        .lines()
        .filter_map(|line| {
            let line = line.trim_matches('\0').trim();
            if line.is_empty() {
                return None;
            }
            let (is_default, line) = line
                .strip_prefix('*')
                .map_or((false, line), |line| (true, line.trim_start()));
            let mut columns: Vec<&str> = line.split_whitespace().collect();
            let version = columns.last().and_then(|value| value.parse::<u32>().ok());
            if version.is_some() {
                columns.pop();
            }
            let state = columns.pop().map(parse_distribution_state)?;
            let name = columns.join(" ");
            if name.is_empty()
                || (state == WslDistributionState::Unknown && version.is_none())
                || name.eq_ignore_ascii_case("NAME")
            {
                return None;
            }
            Some(DistributionInfo {
                name,
                is_default,
                state,
                version,
                docker: DockerEngineInfo::unavailable(DockerEngineState::NotScanned, "尚未探测"),
                containers: Vec::new(),
            })
        })
        .collect()
}

fn parse_distribution_state(value: &str) -> WslDistributionState {
    match value.to_lowercase().as_str() {
        "running" | "正在运行" => WslDistributionState::Running,
        "stopped" | "已停止" => WslDistributionState::Stopped,
        "installing" | "正在安装" => WslDistributionState::Installing,
        _ => WslDistributionState::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_utf16le_wsl_output() {
        let text = "  NAME      STATE     VERSION\r\n* Ubuntu    Stopped   2\r\n";
        let bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(decode_windows_output(&bytes), text);
    }

    #[test]
    fn parses_default_distribution_and_state() {
        let input = "  NAME            STATE       VERSION\n* Ubuntu          Stopped     2\n  Debian          Running     2\n";
        let distributions = parse_wsl_verbose(input);
        assert_eq!(distributions.len(), 2);
        assert_eq!(distributions[0].name, "Ubuntu");
        assert!(distributions[0].is_default);
        assert_eq!(distributions[0].state, WslDistributionState::Stopped);
        assert_eq!(distributions[1].state, WslDistributionState::Running);
    }

    #[test]
    fn reads_container_health_and_compose_labels() {
        let value = serde_json::json!({
            "Id": "abc123",
            "Name": "/project-db-1",
            "Config": {
                "Image": "postgres:17",
                "Labels": {
                    "com.docker.compose.project": "project",
                    "com.docker.compose.service": "db"
                }
            },
            "State": {"Status": "running", "Health": {"Status": "healthy"}},
            "NetworkSettings": {"Ports": {"5432/tcp": [{"HostIp": "0.0.0.0", "HostPort": "15432"}]}}
        });
        let container = container_from_inspect(&value);
        assert_eq!(container.id, "abc123");
        assert_eq!(container.name, "project-db-1");
        assert_eq!(container.health.as_deref(), Some("healthy"));
        assert_eq!(container.compose_project.as_deref(), Some("project"));
        assert_eq!(container.ports, vec!["0.0.0.0:15432 -> 5432/tcp"]);
    }
}
