use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use server_on_core::config::ServiceDefinition;
use server_on_core::protocol::{Command, ResponseData};
use server_on_daemon::ipc::DaemonClient;

#[derive(Parser)]
#[command(name = "server-on-cli", about = "ServerOn 本地服务管理命令行")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    Ping,
    Status,
    Refresh,
    Start {
        service: String,
    },
    Stop {
        service: String,
    },
    Restart {
        service: String,
    },
    Remove {
        service: String,
    },
    Logs {
        service: String,
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
    AddWindows {
        name: String,
        working_directory: String,
        script: String,
    },
    AddWsl {
        name: String,
        distribution: String,
        working_directory: String,
        script: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let client = DaemonClient::connect_or_start().await?;
    match cli.command {
        Commands::Ping => {
            client.request(Command::Ping).await?;
            println!("pong");
        }
        Commands::Status => {
            let response = client.request(Command::GetSnapshot).await?;
            print_response(response)?;
        }
        Commands::Refresh => {
            client.request(Command::RefreshDiscovery).await?;
            let response = client.request(Command::GetSnapshot).await?;
            print_response(response)?;
        }
        Commands::Start { service } => {
            let id = resolve_service(&client, &service).await?;
            client
                .request(Command::StartService { service_id: id })
                .await?;
            println!("已启动：{service}");
        }
        Commands::Stop { service } => {
            let id = resolve_service(&client, &service).await?;
            client
                .request(Command::StopService { service_id: id })
                .await?;
            println!("已停止：{service}");
        }
        Commands::Restart { service } => {
            let id = resolve_service(&client, &service).await?;
            client
                .request(Command::RestartService { service_id: id })
                .await?;
            println!("已重启：{service}");
        }
        Commands::Remove { service } => {
            let id = resolve_service(&client, &service).await?;
            client
                .request(Command::RemoveService { service_id: id })
                .await?;
            println!("已删除：{service}");
        }
        Commands::Logs { service, lines } => {
            let id = resolve_service(&client, &service).await?;
            let response = client
                .request(Command::GetLogs {
                    service_id: id,
                    limit: lines,
                })
                .await?;
            if let ResponseData::Logs(logs) = response {
                for log in logs {
                    println!("[{}] {}", log.timestamp.format("%H:%M:%S"), log.message);
                }
            }
        }
        Commands::AddWindows {
            name,
            working_directory,
            script,
        } => {
            let definition = ServiceDefinition::windows_script(name, working_directory, script);
            client
                .request(Command::UpsertService(Box::new(definition)))
                .await?;
            println!("已保存 Windows 服务");
        }
        Commands::AddWsl {
            name,
            distribution,
            working_directory,
            script,
        } => {
            let definition =
                ServiceDefinition::wsl_script(name, distribution, working_directory, script);
            client
                .request(Command::UpsertService(Box::new(definition)))
                .await?;
            println!("已保存 WSL 服务");
        }
    }
    Ok(())
}

async fn resolve_service(
    client: &DaemonClient,
    value: &str,
) -> Result<server_on_core::config::ServiceId> {
    let response = client.request(Command::GetSnapshot).await?;
    let ResponseData::Snapshot(snapshot) = response else {
        bail!("daemon 未返回服务快照");
    };
    let mut matches = snapshot.services.into_iter().filter(|service| {
        service.definition.id.as_str() == value || service.definition.name == value
    });
    let Some(service) = matches.next() else {
        bail!("找不到服务：{value}");
    };
    if matches.next().is_some() {
        bail!("服务名称不唯一，请使用完整服务 ID：{value}");
    }
    Ok(service.definition.id)
}

fn print_response(response: ResponseData) -> Result<()> {
    match response {
        ResponseData::Snapshot(snapshot) => {
            println!("{}", serde_json::to_string_pretty(&snapshot)?)
        }
        ResponseData::Logs(logs) => println!("{}", serde_json::to_string_pretty(&logs)?),
        ResponseData::Pong => println!("pong"),
        ResponseData::Empty => {}
    }
    Ok(())
}
