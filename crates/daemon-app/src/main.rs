use anyhow::Result;
use server_on_core::paths::AppPaths;
use server_on_daemon::ipc::serve;
use server_on_daemon::manager::ServiceManager;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();
    let manager = ServiceManager::new(AppPaths::local())?;
    let lifecycle_manager = manager.clone();
    tokio::spawn(async move {
        lifecycle_manager.recover().await;
        if let Err(error) = lifecycle_manager.refresh_discovery().await {
            tracing::warn!(%error, "启动时自动发现失败");
        }
        lifecycle_manager.auto_start_services().await;
        loop {
            if let Err(error) = lifecycle_manager.refresh_discovery().await {
                tracing::warn!(%error, "自动发现失败");
            }
            tokio::time::sleep(lifecycle_manager.discovery_interval()).await;
        }
    });
    let health_manager = manager.clone();
    tokio::spawn(async move {
        if let Err(error) = server_on_daemon::health::run(health_manager).await {
            tracing::warn!(%error, "健康检查任务退出");
        }
    });
    serve(manager).await
}
