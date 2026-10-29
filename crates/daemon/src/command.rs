use anyhow::{Context, Result, anyhow};
use std::process::Output;
use std::time::Duration;
use tokio::process::Command;

pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);

pub async fn output(
    command: &mut Command,
    timeout_duration: Duration,
    description: &str,
) -> Result<Output> {
    command.kill_on_drop(true);
    tokio::time::timeout(timeout_duration, command.output())
        .await
        .map_err(|_| anyhow!("{description}超时（{} 秒）", timeout_duration.as_secs()))?
        .with_context(|| description.to_owned())
}

/// Hide console windows for short-lived Windows helper processes.
///
/// WSL and Docker are controlled through CLI processes. Without this flag,
/// Windows may create a visible console for every probe, even when the daemon
/// itself is detached from a console.
pub fn hide_console(command: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn output_enforces_deadline() {
        let mut command = Command::new("powershell.exe");
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "Start-Sleep -Seconds 5",
        ]);
        hide_console(&mut command);
        let error = output(&mut command, Duration::from_millis(50), "测试命令")
            .await
            .unwrap_err();
        assert!(error.to_string().contains("超时"));
    }
}
