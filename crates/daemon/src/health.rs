use crate::manager::ServiceManager;
use anyhow::{Result, bail};
use server_on_core::config::{HealthCheckConfig, ServiceId};
use server_on_core::state::HealthState;
use std::collections::HashMap;
use std::time::Duration;
use tokio::task::JoinSet;

#[derive(Clone, Copy, Default)]
pub struct HealthChecker;

impl HealthChecker {
    pub fn new() -> Self {
        Self
    }

    pub async fn check(&self, config: &HealthCheckConfig) -> Result<()> {
        match config {
            HealthCheckConfig::Tcp {
                host,
                port,
                timeout_ms,
                ..
            } => {
                let address = format!("{host}:{port}");
                let result = tokio::time::timeout(
                    Duration::from_millis(*timeout_ms),
                    tokio::net::TcpStream::connect(address),
                )
                .await;
                match result {
                    Ok(Ok(_)) => Ok(()),
                    Ok(Err(error)) => bail!("TCP 连接失败：{error}"),
                    Err(_) => bail!("TCP 健康检查超时"),
                }
            }
            HealthCheckConfig::Http {
                url,
                expected_status,
                timeout_ms,
                ..
            } => {
                // Windows 11 includes curl.exe. It handles HTTPS and proxy
                // behavior without adding another runtime to the daemon.
                let seconds = format!("{:.3}", *timeout_ms as f64 / 1_000.0);
                let mut command = tokio::process::Command::new("curl.exe");
                command.args([
                    "--silent",
                    "--show-error",
                    "--location",
                    "--output",
                    "NUL",
                    "--write-out",
                    "%{http_code}",
                    "--max-time",
                    &seconds,
                    url,
                ]);
                crate::command::hide_console(&mut command);
                let output = command.output().await?;
                if !output.status.success() {
                    let error = String::from_utf8_lossy(&output.stderr);
                    bail!("HTTP 请求失败：{}", error.trim());
                }
                let status = String::from_utf8_lossy(&output.stdout);
                let status = status.trim().parse::<u16>()?;
                let accepted = expected_status
                    .map(|expected| expected == status)
                    .unwrap_or_else(|| (200..300).contains(&status));
                if accepted {
                    Ok(())
                } else {
                    bail!("HTTP 健康检查返回状态码 {status}")
                }
            }
        }
    }
}

pub async fn run(manager: ServiceManager) -> Result<()> {
    let checker = HealthChecker::new();
    let mut last_checks = HashMap::<ServiceId, tokio::time::Instant>::new();
    let mut ticker = tokio::time::interval(Duration::from_secs(1));
    loop {
        ticker.tick().await;
        let candidates = manager.health_candidates();
        let mut checks = JoinSet::new();
        for (definition, _) in candidates {
            let Some(config) = definition.health_check.clone() else {
                continue;
            };
            let interval = health_interval(&config);
            let should_check = last_checks
                .get(&definition.id)
                .is_none_or(|last| last.elapsed() >= interval);
            if !should_check {
                continue;
            }
            last_checks.insert(definition.id.clone(), tokio::time::Instant::now());
            let id = definition.id.clone();
            checks.spawn(async move {
                let result = checker.check(&config).await;
                (id, result)
            });
        }
        while let Some(result) = checks.join_next().await {
            let (service_id, result) = match result {
                Ok(result) => result,
                Err(error) => {
                    tracing::warn!(%error, "健康检查任务失败");
                    continue;
                }
            };
            match result {
                Ok(()) => manager.update_health(&service_id, HealthState::Healthy, None),
                Err(error) => manager.update_health(
                    &service_id,
                    HealthState::Unhealthy,
                    Some(error.to_string()),
                ),
            }
        }
    }
}

fn health_interval(config: &HealthCheckConfig) -> Duration {
    let seconds = match config {
        HealthCheckConfig::Tcp {
            interval_seconds, ..
        }
        | HealthCheckConfig::Http {
            interval_seconds, ..
        } => *interval_seconds,
    };
    Duration::from_secs(seconds.max(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tcp_health_check_accepts_a_local_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let config = HealthCheckConfig::Tcp {
            host: address.ip().to_string(),
            port: address.port(),
            interval_seconds: 1,
            timeout_ms: 1_000,
        };
        HealthChecker::new().check(&config).await.unwrap();
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn http_health_check_accepts_a_local_success_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = [0_u8; 1_024];
            let _ = socket.read(&mut buffer).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let config = HealthCheckConfig::Http {
            url: format!("http://{address}/health"),
            expected_status: Some(200),
            interval_seconds: 1,
            timeout_ms: 2_000,
        };
        HealthChecker::new().check(&config).await.unwrap();
        server.await.unwrap();
    }
}
