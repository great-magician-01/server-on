use crate::discovery::DiscoveryManager;
use crate::docker;
use crate::metrics::{MetricsCache, RawMetrics, logical_processor_count, query_timeout};
use crate::process::{ProcessController, ProcessMonitor, SpawnedProcess};
use crate::storage::{ConfigStore, LogStore, StateStore};
use anyhow::{Result, anyhow, bail};
use chrono::Utc;
use server_on_core::config::{
    AppConfig, RestartPolicy, RuntimeConfig, ServiceDefinition, ServiceDependency, ServiceId,
    ServiceOrigin,
};
use server_on_core::discovery::DiscoverySnapshot;
use server_on_core::event::{DaemonEvent, LogEvent, LogStream};
use server_on_core::metrics::ServiceMetrics;
use server_on_core::paths::{AppPaths, PersistedState, ServiceInstanceState};
use server_on_core::state::{
    DaemonSnapshot, HealthState, OperationKind, ServiceState, ServiceStatus, ServiceView,
};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, BufReader};
use tokio::sync::broadcast;

// Lock acquisition order for the sync locks below (never hold one across
// an .await): config_commit -> config -> operations -> restart_attempts ->
// controllers -> statuses. In particular, never take restart_attempts while
// holding statuses: the failure-restart task holds restart_attempts while
// reading statuses, so the reverse order would deadlock.
pub struct ManagerInner {
    config: RwLock<AppConfig>,
    // Serializes the read-clone-validate-save-swap cycles in
    // upsert_service/remove_service/update_settings/refresh_discovery so
    // concurrent editors cannot silently drop each other's changes.
    config_commit: Mutex<()>,
    statuses: RwLock<BTreeMap<ServiceId, ServiceStatus>>,
    discovery: RwLock<DiscoverySnapshot>,
    discovered_at: RwLock<Option<chrono::DateTime<Utc>>>,
    controllers: Mutex<BTreeMap<ServiceId, ProcessController>>,
    metrics_cache: Mutex<MetricsCache>,
    restart_attempts: Mutex<BTreeMap<ServiceId, u32>>,
    started_at: Mutex<BTreeMap<ServiceId, tokio::time::Instant>>,
    operations: Mutex<BTreeSet<ServiceId>>,
    monitoring_lost: Mutex<BTreeSet<ServiceId>>,
    discovery_operation: tokio::sync::Mutex<()>,
    store: ConfigStore,
    state_store: StateStore,
    persisted: Mutex<PersistedState>,
    logs: Arc<LogStore>,
    log_sequence: AtomicU64,
    log_record: Mutex<()>,
    log_backfill: tokio::sync::Mutex<()>,
    events: broadcast::Sender<DaemonEvent>,
}

#[derive(Clone)]
pub struct ServiceManager {
    inner: Arc<ManagerInner>,
}

impl ServiceManager {
    pub fn new(paths: AppPaths) -> Result<Self> {
        let store = ConfigStore::new(paths.clone());
        let config = store.load()?;
        let state_store = StateStore::new(&paths);
        let persisted = state_store.load().unwrap_or_else(|error| {
            tracing::warn!(%error, "运行状态读取失败，按无恢复记录处理");
            PersistedState::default()
        });
        let persisted_ids: BTreeSet<ServiceId> = persisted
            .instances
            .iter()
            .map(|instance| instance.service_id.clone())
            .collect();
        let logs = Arc::new(LogStore::new(
            paths.logs_dir.clone(),
            config.settings.logs.max_file_bytes,
            config.settings.logs.max_files,
            config.settings.logs.retention_days,
        ));
        let initial_log_sequence = logs.max_sequence();
        let statuses = config
            .services
            .iter()
            .map(|service| {
                let status = match service.runtime {
                    RuntimeConfig::DockerContainer { .. } => {
                        ServiceStatus::unknown(service.id.clone(), "等待 Docker 自动发现校准状态")
                    }
                    _ if persisted_ids.contains(&service.id) => ServiceStatus::unknown(
                        service.id.clone(),
                        "检测到 daemon 重启前的服务实例，等待恢复校验",
                    ),
                    _ => ServiceStatus::new(service.id.clone()),
                };
                (service.id.clone(), status)
            })
            .collect();
        let (events, _) = broadcast::channel(2_048);
        Ok(Self {
            inner: Arc::new(ManagerInner {
                config: RwLock::new(config),
                config_commit: Mutex::new(()),
                statuses: RwLock::new(statuses),
                discovery: RwLock::new(DiscoverySnapshot::default()),
                discovered_at: RwLock::new(None),
                controllers: Mutex::new(BTreeMap::new()),
                metrics_cache: Mutex::new(MetricsCache::default()),
                restart_attempts: Mutex::new(BTreeMap::new()),
                started_at: Mutex::new(BTreeMap::new()),
                operations: Mutex::new(BTreeSet::new()),
                monitoring_lost: Mutex::new(BTreeSet::new()),
                discovery_operation: tokio::sync::Mutex::new(()),
                store,
                state_store,
                persisted: Mutex::new(persisted),
                logs,
                log_sequence: AtomicU64::new(initial_log_sequence),
                log_record: Mutex::new(()),
                log_backfill: tokio::sync::Mutex::new(()),
                events,
            }),
        })
    }

    pub fn subscribe(&self) -> broadcast::Receiver<DaemonEvent> {
        self.inner.events.subscribe()
    }

    pub fn snapshot(&self) -> DaemonSnapshot {
        let config = self.inner.config.read().unwrap();
        let statuses = self.inner.statuses.read().unwrap();
        let services = config
            .services
            .iter()
            .map(|definition| ServiceView {
                definition: definition.clone(),
                status: statuses
                    .get(&definition.id)
                    .cloned()
                    .unwrap_or_else(|| ServiceStatus::new(definition.id.clone())),
            })
            .collect();
        DaemonSnapshot {
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            services,
            discovery: self.inner.discovery.read().unwrap().clone(),
            discovered_at: *self.inner.discovered_at.read().unwrap(),
        }
    }

    pub fn upsert_service(&self, definition: ServiceDefinition) -> Result<()> {
        let _commit = self.inner.config_commit.lock().unwrap();
        if self
            .inner
            .operations
            .lock()
            .unwrap()
            .contains(&definition.id)
        {
            bail!("服务已有启动、停止或重启操作正在执行，请完成后再修改配置");
        }
        if definition.origin != ServiceOrigin::User {
            bail!("自动发现的容器不能通过服务编辑器修改");
        }
        definition.validate().map_err(|error| anyhow!(error))?;
        if self
            .inner
            .controllers
            .lock()
            .unwrap()
            .contains_key(&definition.id)
        {
            bail!("服务正在运行，请先停止后再修改配置");
        }
        let mut config = self.inner.config.read().unwrap().clone();
        if let Some(existing) = config
            .services
            .iter_mut()
            .find(|service| service.id == definition.id)
        {
            *existing = definition.clone();
        } else {
            config.services.push(definition.clone());
        }
        config.validate()?;
        self.inner.store.save(&config)?;
        *self.inner.config.write().unwrap() = config;
        self.inner
            .statuses
            .write()
            .unwrap()
            .entry(definition.id.clone())
            .or_insert_with(|| ServiceStatus::new(definition.id.clone()));
        self.emit_view(&definition.id);
        Ok(())
    }

    pub fn remove_service(&self, service_id: &ServiceId) -> Result<()> {
        let _commit = self.inner.config_commit.lock().unwrap();
        if self.inner.operations.lock().unwrap().contains(service_id) {
            bail!("服务已有启动、停止或重启操作正在执行，请完成后再删除");
        }
        self.inner.metrics_cache.lock().unwrap().remove(service_id);
        if self
            .inner
            .controllers
            .lock()
            .unwrap()
            .contains_key(service_id)
        {
            bail!("服务正在运行，请先停止后再删除");
        }
        let mut config = self.inner.config.read().unwrap().clone();
        let original_length = config.services.len();
        config.services.retain(|service| &service.id != service_id);
        if config.services.len() == original_length {
            bail!("服务不存在：{service_id}");
        }
        config.validate()?;
        self.inner.store.save(&config)?;
        *self.inner.config.write().unwrap() = config;
        self.inner.statuses.write().unwrap().remove(service_id);
        let _ = self
            .inner
            .events
            .send(DaemonEvent::ServiceRemoved(service_id.clone()));
        Ok(())
    }

    pub async fn refresh_discovery(&self) -> Result<()> {
        let _operation = self.inner.discovery_operation.lock().await;
        let scan_stopped = self
            .inner
            .config
            .read()
            .unwrap()
            .settings
            .discovery
            .scan_stopped_distributions;
        let snapshot = DiscoveryManager::new(scan_stopped).discover().await?;
        // The commit lock is only held over the synchronous reconcile section;
        // all awaits in this method happen before it is taken.
        let _commit = self.inner.config_commit.lock().unwrap();
        let mut config = self.inner.config.read().unwrap().clone();
        let original_config = config.clone();
        let mut statuses = self.inner.statuses.write().unwrap();
        let mut affected = BTreeSet::new();
        let mut discovered_ids = BTreeSet::new();

        for distribution in &snapshot.distributions {
            let Some(engine_id) = distribution.docker.engine_id.clone() else {
                continue;
            };
            for container in &distribution.containers {
                let mut definition = ServiceDefinition::docker_container(
                    distribution.name.clone(),
                    engine_id.clone(),
                    container.id.clone(),
                    container.name.clone(),
                    container.compose_project.clone(),
                    container.compose_service.clone(),
                );
                let definition_changed = match config
                    .services
                    .iter()
                    .find(|service| service.id == definition.id)
                {
                    Some(existing) => {
                        definition.enabled = existing.enabled;
                        definition.auto_start = existing.auto_start;
                        definition.stop_timeout_seconds = existing.stop_timeout_seconds;
                        definition.restart_policy = existing.restart_policy.clone();
                        definition.health_check = existing.health_check.clone();
                        definition.depends_on = existing.depends_on.clone();
                        existing != &definition
                    }
                    None => true,
                };
                let id = definition.id.clone();
                discovered_ids.insert(id.clone());
                if definition_changed {
                    affected.insert(id.clone());
                }
                if let Some(existing) = config.services.iter_mut().find(|service| service.id == id)
                {
                    if definition_changed {
                        *existing = definition;
                    }
                } else {
                    config.services.push(definition);
                }
                let (state, health, message) = docker::container_status(container);
                let previous_status = statuses.get(&id).cloned();
                let mut status = previous_status
                    .clone()
                    .unwrap_or_else(|| ServiceStatus::new(id.clone()));
                status.container_id = Some(container.id.clone());
                if status.active_operation.is_none() {
                    status.state = state;
                    status.health = health;
                    status.message = message;
                }
                if previous_status
                    .as_ref()
                    .is_none_or(|previous| status_changed(previous, &status))
                {
                    status.updated_at = Utc::now();
                    statuses.insert(id.clone(), status);
                    affected.insert(id);
                }
            }
        }

        let existing_discovered: Vec<ServiceDefinition> = config
            .services
            .iter()
            .filter(|service| {
                service.origin == ServiceOrigin::Discovered && !discovered_ids.contains(&service.id)
            })
            .cloned()
            .collect();
        for definition in existing_discovered {
            let RuntimeConfig::DockerContainer { distribution, .. } = &definition.runtime else {
                continue;
            };
            let known_distribution = snapshot
                .distributions
                .iter()
                .find(|candidate| &candidate.name == distribution);
            let (state, message) = match known_distribution {
                Some(distribution)
                    if distribution.docker.state
                        == server_on_core::discovery::DockerEngineState::Available =>
                {
                    (
                        ServiceState::Missing,
                        "容器已被删除或尚未重新发现".to_owned(),
                    )
                }
                Some(_) => (
                    ServiceState::Unknown,
                    "Docker Engine 不可达，保留最后已知容器身份".to_owned(),
                ),
                None => (ServiceState::Unknown, "WSL 发行版未找到".to_owned()),
            };
            let previous_status = statuses.get(&definition.id).cloned();
            let mut status = previous_status
                .clone()
                .unwrap_or_else(|| ServiceStatus::new(definition.id.clone()));
            status.state = state;
            status.message = Some(message);
            if previous_status
                .as_ref()
                .is_none_or(|previous| status_changed(previous, &status))
            {
                status.updated_at = Utc::now();
                statuses.insert(definition.id.clone(), status);
                affected.insert(definition.id.clone());
            }
        }
        drop(statuses);
        if config != original_config {
            config.validate()?;
            self.inner.store.save(&config)?;
            *self.inner.config.write().unwrap() = config;
        }
        *self.inner.discovery.write().unwrap() = snapshot;
        *self.inner.discovered_at.write().unwrap() = Some(Utc::now());
        for id in affected {
            self.emit_view(&id);
        }
        let _ = self.inner.events.send(DaemonEvent::DiscoveryCompleted {
            discovered_at: Utc::now(),
        });
        Ok(())
    }

    pub async fn start(&self, service_id: &ServiceId) -> Result<()> {
        self.inner
            .restart_attempts
            .lock()
            .unwrap()
            .insert(service_id.clone(), 0);
        let mut stack = Vec::new();
        self.start_with_dependencies(service_id, &mut stack).await
    }

    fn start_with_dependencies<'a>(
        &'a self,
        service_id: &'a ServiceId,
        stack: &'a mut Vec<ServiceId>,
    ) -> Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>> {
        Box::pin(async move {
            if stack.contains(service_id) {
                bail!("检测到服务依赖循环：{service_id}");
            }
            stack.push(service_id.clone());
            let definition = self.definition(service_id)?;
            for dependency in &definition.depends_on {
                let dependency_id = dependency.service_id();
                let state = self
                    .inner
                    .statuses
                    .read()
                    .unwrap()
                    .get(dependency_id)
                    .map(|status| status.state)
                    .ok_or_else(|| anyhow!("依赖服务不存在：{dependency_id}"))?;
                match state {
                    ServiceState::Stopped | ServiceState::Failed => {
                        self.start_with_dependencies(dependency_id, stack).await?;
                    }
                    ServiceState::Starting | ServiceState::Running | ServiceState::Degraded => {}
                    ServiceState::Stopping => {
                        bail!("依赖服务 {dependency_id} 正在停止，不能启动当前服务");
                    }
                    ServiceState::Unknown | ServiceState::Missing => {
                        bail!("依赖服务 {dependency_id} 状态未知，拒绝重复启动");
                    }
                }
                self.wait_for_dependency(dependency).await?;
            }
            stack.pop();
            self.start_one(service_id).await
        })
    }

    async fn wait_for_dependency(&self, dependency: &ServiceDependency) -> Result<()> {
        let dependency_id = dependency.service_id();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
        loop {
            let status = self
                .inner
                .statuses
                .read()
                .unwrap()
                .get(dependency_id)
                .cloned()
                .ok_or_else(|| anyhow!("依赖服务不存在：{dependency_id}"))?;
            let satisfied = match dependency {
                ServiceDependency::Running { .. } => {
                    matches!(status.state, ServiceState::Running | ServiceState::Degraded)
                }
                ServiceDependency::Healthy { .. } => status.health == HealthState::Healthy,
            };
            if satisfied {
                return Ok(());
            }
            if matches!(status.state, ServiceState::Failed | ServiceState::Missing) {
                bail!("依赖服务 {dependency_id} 启动失败或已丢失");
            }
            if matches!(status.state, ServiceState::Unknown) {
                bail!("依赖服务 {dependency_id} 状态未知");
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("等待依赖服务 {dependency_id} 满足条件超时");
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    async fn start_one(&self, service_id: &ServiceId) -> Result<()> {
        let commit = self.inner.config_commit.lock().unwrap();
        let definition = self.definition(service_id)?;
        if !definition.enabled {
            bail!("服务已禁用");
        }
        if self
            .inner
            .statuses
            .read()
            .unwrap()
            .get(service_id)
            .is_some_and(|status| {
                matches!(status.state, ServiceState::Unknown | ServiceState::Missing)
            })
        {
            bail!("服务状态未知，必须先完成发现或恢复校验，拒绝重复启动");
        }
        if self
            .inner
            .controllers
            .lock()
            .unwrap()
            .contains_key(service_id)
        {
            bail!("服务已经在运行");
        }
        let operation = self.begin_operation(service_id, OperationKind::Start)?;
        drop(commit);
        let result = match &definition.runtime {
            RuntimeConfig::DockerContainer {
                distribution,
                container_id,
                ..
            } => {
                self.start_docker(&definition, distribution, container_id)
                    .await
            }
            _ => self.start_process(definition.clone()).await,
        };
        self.finish_operation(service_id, OperationKind::Start, operation, result)
            .await
    }

    pub async fn stop(&self, service_id: &ServiceId) -> Result<()> {
        self.inner
            .restart_attempts
            .lock()
            .unwrap()
            .insert(service_id.clone(), 0);
        let commit = self.inner.config_commit.lock().unwrap();
        let definition = self.definition(service_id)?;
        let operation = self.begin_operation(service_id, OperationKind::Stop)?;
        drop(commit);
        let result = match &definition.runtime {
            RuntimeConfig::DockerContainer {
                distribution,
                container_id,
                ..
            } => {
                self.stop_docker(&definition, distribution, container_id)
                    .await
            }
            _ => self.stop_process(&definition).await,
        };
        self.finish_operation(service_id, OperationKind::Stop, operation, result)
            .await
    }

    pub async fn restart(&self, service_id: &ServiceId) -> Result<()> {
        let definition = self.definition(service_id)?;
        if let RuntimeConfig::DockerContainer {
            distribution,
            container_id,
            ..
        } = &definition.runtime
        {
            let commit = self.inner.config_commit.lock().unwrap();
            let definition = self.definition(service_id)?;
            let operation = self.begin_operation(service_id, OperationKind::Restart)?;
            drop(commit);
            let result =
                docker::restart(distribution, container_id, definition.stop_timeout_seconds)
                    .await
                    .and_then(|container| self.apply_container_started(&definition, &container));
            return self
                .finish_operation(service_id, OperationKind::Restart, operation, result)
                .await;
        }
        self.stop(service_id).await?;
        self.start(service_id).await
    }

    pub async fn metrics(&self, service_id: &ServiceId) -> Result<ServiceMetrics> {
        let definition = self.definition(service_id)?;
        let status = self
            .inner
            .statuses
            .read()
            .unwrap()
            .get(service_id)
            .cloned()
            .unwrap_or_else(|| ServiceStatus::new(service_id.clone()));
        let started_at = self
            .inner
            .persisted
            .lock()
            .unwrap()
            .instances
            .iter()
            .find(|instance| &instance.service_id == service_id)
            .and_then(|instance| instance.process_started_at);
        let uptime_seconds =
            started_at.map(|started| (Utc::now() - started).num_seconds().max(0) as u64);
        let mut result = ServiceMetrics {
            service_id: service_id.clone(),
            sampled_at: Utc::now(),
            cpu_percent: None,
            memory_bytes: None,
            process_count: None,
            uptime_seconds,
            message: None,
        };
        if !matches!(
            status.state,
            ServiceState::Running | ServiceState::Degraded | ServiceState::Starting
        ) {
            result.message = Some("服务未运行，暂无资源指标".into());
            return Ok(result);
        }

        let mut cache_identity = None;
        let raw_result: Result<Result<RawMetrics, anyhow::Error>, tokio::time::error::Elapsed> =
            tokio::time::timeout(query_timeout(), async {
                match &definition.runtime {
                    RuntimeConfig::WindowsScript { .. } => {
                        let controller = self
                            .inner
                            .controllers
                            .lock()
                            .unwrap()
                            .get(service_id)
                            .cloned();
                        let Some(ProcessController::Windows(controller)) = controller else {
                            return Ok(RawMetrics {
                                message: Some("进程未受管或已退出".into()),
                                ..Default::default()
                            });
                        };
                        let identity = controller.identity();
                        let metrics =
                            tokio::task::spawn_blocking(move || controller.metrics()).await??;
                        cache_identity = Some((identity, metrics.cpu_time_ns));
                        Ok(RawMetrics {
                            cpu_time_ns: Some(metrics.cpu_time_ns),
                            memory_bytes: Some(metrics.memory_bytes),
                            process_count: Some(metrics.process_count),
                            message: metrics.partial.then(|| "部分进程内存无法读取".into()),
                            ..Default::default()
                        })
                    }
                    RuntimeConfig::WslScript { .. } => {
                        let controller = self
                            .inner
                            .controllers
                            .lock()
                            .unwrap()
                            .get(service_id)
                            .cloned();
                        match controller {
                            Some(ProcessController::Systemd {
                                distribution,
                                unit_name,
                            }) => {
                                let raw =
                                    crate::process::systemd_metrics(&distribution, &unit_name)
                                        .await?;
                                if let Some(cpu_time_ns) = raw.cpu_time_ns {
                                    cache_identity =
                                        Some((format!("systemd:{unit_name}"), cpu_time_ns));
                                }
                                Ok(raw)
                            }
                            Some(ProcessController::Wsl {
                                distribution,
                                process_group_id,
                                ..
                            }) => {
                                let raw = crate::process::process_group_metrics(
                                    &distribution,
                                    process_group_id,
                                )
                                .await?;
                                if let Some(cpu_time_ns) = raw.cpu_time_ns {
                                    cache_identity =
                                        Some((format!("pgid:{process_group_id}"), cpu_time_ns));
                                }
                                Ok(raw)
                            }
                            _ => Ok(RawMetrics {
                                message: Some("进程未受管或已退出".into()),
                                ..Default::default()
                            }),
                        }
                    }
                    RuntimeConfig::DockerContainer {
                        distribution,
                        container_id,
                        ..
                    } => {
                        let stats = crate::docker::stats(distribution, container_id).await?;
                        if let Some(started) = stats.started_at {
                            result.uptime_seconds =
                                Some((Utc::now() - started).num_seconds().max(0) as u64);
                        }
                        Ok(RawMetrics {
                            cpu_percent: stats.cpu_percent,
                            memory_bytes: stats.memory_bytes,
                            process_count: stats.process_count,
                            ..Default::default()
                        })
                    }
                }
            })
            .await;
        let raw = match raw_result {
            Ok(Ok(raw)) => raw,
            Ok(Err(error)) => {
                result.message = Some(error.to_string());
                return Ok(result);
            }
            Err(_) => {
                result.message = Some("指标查询超时".into());
                return Ok(result);
            }
        };
        if let Some((identity, cumulative_ns)) = cache_identity {
            result.cpu_percent = self.inner.metrics_cache.lock().unwrap().cpu_percent(
                service_id,
                identity,
                cumulative_ns,
                logical_processor_count(),
            );
        } else if let Some(percent) = raw.cpu_percent {
            result.cpu_percent =
                Some((percent / logical_processor_count() as f64).clamp(0.0, 100.0));
        }
        result.memory_bytes = raw.memory_bytes;
        result.process_count = raw.process_count;
        if let Some(message) = raw.message {
            result.message = Some(message);
        }
        Ok(result)
    }

    pub async fn recent_logs(&self, service_id: &ServiceId, limit: usize) -> Result<Vec<LogEvent>> {
        let definition = self.definition(service_id)?;
        if let RuntimeConfig::DockerContainer {
            distribution,
            container_id,
            ..
        } = &definition.runtime
        {
            return Ok(docker::logs(distribution, container_id, limit)
                .await?
                .into_iter()
                .map(|line| {
                    let (stream, message) = line
                        .strip_prefix("[stderr] ")
                        .map_or((LogStream::Stdout, line.as_str()), |line| {
                            (LogStream::Stderr, line)
                        });
                    LogEvent {
                        service_id: service_id.clone(),
                        timestamp: Utc::now(),
                        stream,
                        message: message.to_owned(),
                        sequence: 0,
                    }
                })
                .collect());
        }
        if let RuntimeConfig::WslScript { distribution, .. } = &definition.runtime {
            let unit_name = self
                .inner
                .persisted
                .lock()
                .unwrap()
                .instances
                .iter()
                .find(|instance| instance.service_id == *service_id)
                .and_then(|instance| instance.unit_name.clone());
            if let Some(unit_name) = unit_name {
                let local = self.inner.logs.recent(service_id, limit)?;
                if !local.is_empty() {
                    return Ok(local);
                }
                let _backfill = self.inner.log_backfill.lock().await;
                let local = self.inner.logs.recent(service_id, limit)?;
                if !local.is_empty() {
                    return Ok(local);
                }
                let messages =
                    crate::process::journal_logs(distribution, &unit_name, limit).await?;
                // A concurrent log event may have populated the file while the
                // journal query was in flight. Prefer it to avoid inserting
                // duplicate historical records.
                let local = self.inner.logs.recent(service_id, limit)?;
                if !local.is_empty() {
                    return Ok(local);
                }
                let _record = self.inner.log_record.lock().unwrap();
                let local = self.inner.logs.recent(service_id, limit)?;
                if !local.is_empty() {
                    return Ok(local);
                }
                let mut events = Vec::with_capacity(messages.len());
                for message in messages {
                    let event = LogEvent {
                        service_id: service_id.clone(),
                        timestamp: Utc::now(),
                        stream: LogStream::Stdout,
                        message,
                        sequence: self.next_log_sequence(),
                    };
                    self.inner.logs.append(&event)?;
                    events.push(event);
                }
                return Ok(events);
            }
        }
        self.inner.logs.recent(service_id, limit)
    }

    async fn start_process(&self, definition: ServiceDefinition) -> Result<()> {
        self.emit_log(
            &definition.id,
            LogStream::System,
            "正在创建受管服务进程".into(),
        );
        let spawned = SpawnedProcess::spawn(&definition).await?;
        let process_id = spawned.process_id;
        let process_group_id = spawned.process_group_id;
        let job_name = spawned.job_name.clone();
        let unit_name = spawned.unit_name.clone();
        let recovery_identity = spawned.recovery_identity.clone();
        let SpawnedProcess {
            controller,
            mut monitor,
            stdout,
            stderr,
            ..
        } = spawned;
        self.inner
            .monitoring_lost
            .lock()
            .unwrap()
            .remove(&definition.id);
        self.inner
            .controllers
            .lock()
            .unwrap()
            .insert(definition.id.clone(), controller);
        {
            let mut statuses = self.inner.statuses.write().unwrap();
            let status = statuses
                .entry(definition.id.clone())
                .or_insert_with(|| ServiceStatus::new(definition.id.clone()));
            status.state = ServiceState::Running;
            status.health = if definition.health_check.is_some() {
                HealthState::Checking
            } else {
                HealthState::NotConfigured
            };
            status.message = None;
            status.process_id = process_id;
            status.last_exit_code = None;
            status.updated_at = Utc::now();
        }
        self.persist_instance(ServiceInstanceState {
            service_id: definition.id.clone(),
            instance_id: uuid::Uuid::new_v4(),
            process_id,
            process_group_id,
            job_name,
            unit_name,
            process_started_at: Some(Utc::now()),
            recovery_identity,
        });
        self.inner
            .started_at
            .lock()
            .unwrap()
            .insert(definition.id.clone(), tokio::time::Instant::now());
        self.emit_view(&definition.id);
        self.spawn_log_reader(definition.id.clone(), stdout, LogStream::Stdout);
        self.spawn_log_reader(definition.id.clone(), stderr, LogStream::Stderr);
        let manager = self.clone();
        let service_id = definition.id.clone();
        tokio::spawn(async move {
            let result = monitor.wait().await;
            manager.process_exited(&service_id, result);
        });
        Ok(())
    }

    async fn stop_process(&self, definition: &ServiceDefinition) -> Result<()> {
        let controller = self
            .inner
            .controllers
            .lock()
            .unwrap()
            .get(&definition.id)
            .cloned();
        let Some(controller) = controller else {
            let is_unknown = self
                .inner
                .statuses
                .read()
                .unwrap()
                .get(&definition.id)
                .is_some_and(|status| status.state == ServiceState::Unknown);
            if is_unknown {
                bail!("无法确认旧服务实例身份，请先完成恢复校验，避免误停其他进程");
            }
            self.clear_instance(&definition.id);
            self.set_simple_state(&definition.id, ServiceState::Stopped, None);
            return Ok(());
        };
        controller
            .stop(Duration::from_secs(definition.stop_timeout_seconds))
            .await?;
        if self
            .inner
            .monitoring_lost
            .lock()
            .unwrap()
            .remove(&definition.id)
        {
            self.inner
                .controllers
                .lock()
                .unwrap()
                .remove(&definition.id);
            self.inner.started_at.lock().unwrap().remove(&definition.id);
            self.inner
                .metrics_cache
                .lock()
                .unwrap()
                .remove(&definition.id);
            self.clear_instance(&definition.id);
            self.set_simple_state(&definition.id, ServiceState::Stopped, None);
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while self
            .inner
            .controllers
            .lock()
            .unwrap()
            .contains_key(&definition.id)
        {
            if tokio::time::Instant::now() >= deadline {
                bail!("停止命令已发送，但服务进程仍未完全退出");
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        self.set_simple_state(&definition.id, ServiceState::Stopped, None);
        Ok(())
    }

    async fn start_docker(
        &self,
        definition: &ServiceDefinition,
        distribution: &str,
        container_id: &str,
    ) -> Result<()> {
        let container = docker::start(distribution, container_id).await?;
        self.apply_container_started(definition, &container)
    }

    fn apply_container_started(
        &self,
        definition: &ServiceDefinition,
        container: &server_on_core::discovery::ContainerInfo,
    ) -> Result<()> {
        let (state, health, _) = docker::container_status(container);
        if state != ServiceState::Running {
            bail!("Docker 命令已返回，但容器当前状态为 {}", container.state);
        }
        let mut statuses = self.inner.statuses.write().unwrap();
        let status = statuses
            .entry(definition.id.clone())
            .or_insert_with(|| ServiceStatus::new(definition.id.clone()));
        status.state = state;
        status.health = health;
        status.message = None;
        status.container_id = Some(container.id.clone());
        status.last_exit_code = None;
        status.updated_at = Utc::now();
        drop(statuses);
        self.emit_view(&definition.id);
        Ok(())
    }

    async fn stop_docker(
        &self,
        definition: &ServiceDefinition,
        distribution: &str,
        container_id: &str,
    ) -> Result<()> {
        let container =
            docker::stop(distribution, container_id, definition.stop_timeout_seconds).await?;
        let (_, health, _) = docker::container_status(&container);
        let mut statuses = self.inner.statuses.write().unwrap();
        let status = statuses
            .entry(definition.id.clone())
            .or_insert_with(|| ServiceStatus::new(definition.id.clone()));
        status.state = ServiceState::Stopped;
        status.health = health;
        status.message = None;
        status.updated_at = Utc::now();
        drop(statuses);
        self.emit_view(&definition.id);
        Ok(())
    }

    fn process_exited(&self, service_id: &ServiceId, result: Result<crate::process::ProcessExit>) {
        let exit = match result {
            Ok(exit) => {
                self.inner
                    .monitoring_lost
                    .lock()
                    .unwrap()
                    .remove(service_id);
                exit
            }
            Err(error) => {
                self.inner
                    .monitoring_lost
                    .lock()
                    .unwrap()
                    .insert(service_id.clone());
                self.process_monitoring_lost(service_id, error);
                return;
            }
        };
        self.inner.metrics_cache.lock().unwrap().remove(service_id);
        let elapsed = self
            .inner
            .started_at
            .lock()
            .unwrap()
            .get(service_id)
            .map(|started| started.elapsed());
        let code = exit.code;
        let expected = exit.expected;
        // Capture the stop intent before releasing the controller. The stop
        // operation waits for the controller, so it cannot clear Stopping first.
        let was_stopping = self
            .inner
            .statuses
            .read()
            .unwrap()
            .get(service_id)
            .is_some_and(|status| status.state == ServiceState::Stopping);
        let mut retry = None;
        // Restart bookkeeping must happen before the statuses write lock is
        // taken: the delayed restart task holds restart_attempts while reading
        // statuses, so locking in the other order here would deadlock.
        let restart_policy = if !was_stopping && !expected && code != Some(0) {
            self.definition(service_id)
                .ok()
                .map(|definition| definition.restart_policy)
        } else {
            None
        };
        if let Some(RestartPolicy::OnFailure {
            max_attempts,
            delay_seconds,
        }) = restart_policy
        {
            let mut attempts = self.inner.restart_attempts.lock().unwrap();
            let attempt = attempts.entry(service_id.clone()).or_insert(0);
            if elapsed.is_some_and(|elapsed| elapsed >= Duration::from_secs(60)) {
                *attempt = 0;
            }
            if *attempt < max_attempts {
                *attempt += 1;
                retry = Some((*attempt, delay_seconds));
            }
        }
        {
            let mut statuses = self.inner.statuses.write().unwrap();
            if let Some(status) = statuses.get_mut(service_id) {
                status.state = if was_stopping || expected || code == Some(0) {
                    ServiceState::Stopped
                } else {
                    ServiceState::Failed
                };
                status.last_exit_code = code;
                status.process_id = None;
                if status.state == ServiceState::Stopped {
                    status.health = HealthState::NotConfigured;
                }
                status.message = match code {
                    Some(0) => None,
                    Some(code) => Some(format!("服务进程异常退出，退出码 {code}")),
                    None if was_stopping || expected => None,
                    None => Some("无法读取服务退出状态".into()),
                };
                status.updated_at = Utc::now();
            }
        }
        self.clear_instance(service_id);
        self.inner.started_at.lock().unwrap().remove(service_id);
        // Publishing the final state before removing the controller prevents a
        // new start from being overwritten by this instance's exit handling.
        self.inner.controllers.lock().unwrap().remove(service_id);
        self.emit_view(service_id);
        if let Some((attempt, delay_seconds)) = retry {
            let manager = self.clone();
            let service_id = service_id.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_secs(delay_seconds)).await;
                let should_restart = manager
                    .inner
                    .restart_attempts
                    .lock()
                    .unwrap()
                    .get(&service_id)
                    .is_some_and(|current| *current == attempt)
                    && !manager
                        .inner
                        .controllers
                        .lock()
                        .unwrap()
                        .contains_key(&service_id)
                    && manager
                        .inner
                        .statuses
                        .read()
                        .unwrap()
                        .get(&service_id)
                        .is_some_and(|status| status.state == ServiceState::Failed);
                if !should_restart {
                    return;
                }
                let mut stack = Vec::new();
                if let Err(error) = manager
                    .start_with_dependencies(&service_id, &mut stack)
                    .await
                {
                    manager.emit_log(
                        &service_id,
                        LogStream::System,
                        format!("异常退出后的自动重启失败：{error:#}"),
                    );
                }
            });
        }
    }

    fn process_monitoring_lost(&self, service_id: &ServiceId, error: anyhow::Error) {
        let mut statuses = self.inner.statuses.write().unwrap();
        if let Some(status) = statuses.get_mut(service_id) {
            status.state = ServiceState::Unknown;
            status.message = Some(format!(
                "无法监控服务进程，旧实例可能仍在运行：{error:#}。请重新对账或确认后停止旧实例"
            ));
            status.updated_at = Utc::now();
        }
        drop(statuses);
        self.emit_view(service_id);
    }

    fn persist_instance(&self, instance: ServiceInstanceState) {
        let service_id = instance.service_id.clone();
        let mut persisted = self.inner.persisted.lock().unwrap();
        persisted
            .instances
            .retain(|candidate| candidate.service_id != service_id);
        persisted.instances.push(instance);
        if let Err(error) = self.inner.state_store.save(&persisted) {
            tracing::warn!(%error, "保存服务实例恢复信息失败");
        }
    }

    fn clear_instance(&self, service_id: &ServiceId) {
        let mut persisted = self.inner.persisted.lock().unwrap();
        let old_length = persisted.instances.len();
        persisted
            .instances
            .retain(|candidate| &candidate.service_id != service_id);
        if persisted.instances.len() != old_length
            && let Err(error) = self.inner.state_store.save(&persisted)
        {
            tracing::warn!(%error, "清理服务实例恢复信息失败");
        }
    }

    pub async fn recover(&self) {
        let instances = self.inner.persisted.lock().unwrap().instances.clone();
        for instance in instances {
            let service_id = instance.service_id.clone();
            let Ok(definition) = self.definition(&service_id) else {
                self.clear_instance(&service_id);
                continue;
            };
            match &definition.runtime {
                RuntimeConfig::WindowsScript { .. } => {
                    #[cfg(windows)]
                    {
                        let recovered = match (instance.job_name.as_deref(), instance.process_id) {
                            (Some(job_name), Some(pid)) => {
                                crate::windows_process::WindowsProcess::recover(
                                    job_name,
                                    pid,
                                    instance.process_started_at,
                                )
                            }
                            _ => Err(anyhow!("缺少 Windows 实例恢复信息")),
                        };
                        match recovered {
                            Ok(Some((process, controller))) => {
                                self.inner.controllers.lock().unwrap().insert(
                                    service_id.clone(),
                                    ProcessController::Windows(controller),
                                );
                                self.mark_recovered(&definition, instance.process_id);
                                let manager = self.clone();
                                let id = service_id.clone();
                                tokio::spawn(async move {
                                    let result = process.wait().await.map(|code| {
                                        crate::process::ProcessExit {
                                            code: Some(code),
                                            expected: false,
                                        }
                                    });
                                    manager.process_exited(&id, result);
                                });
                            }
                            Ok(None) => {
                                self.clear_instance(&service_id);
                                self.set_simple_state(
                                    &service_id,
                                    ServiceState::Stopped,
                                    Some("daemon 停止期间旧服务进程已退出".into()),
                                );
                            }
                            Err(error) => self.mark_recovery_unknown(&service_id, error),
                        }
                    }
                }
                RuntimeConfig::WslScript { distribution, .. } => {
                    if let Some(unit_name) = instance.unit_name.clone() {
                        match crate::process::systemd_is_active(distribution, &unit_name).await {
                            Ok(true) => {
                                match crate::process::recover_systemd(distribution, &unit_name)
                                    .await
                                {
                                    Ok(spawned) => {
                                        self.attach_recovered_process(&definition, spawned)
                                    }
                                    Err(error) => self.mark_recovery_unknown(&service_id, error),
                                }
                            }
                            Ok(false) => {
                                self.clear_instance(&service_id);
                                self.set_simple_state(&service_id, ServiceState::Stopped, None);
                            }
                            Err(error) => self.mark_recovery_unknown(&service_id, error),
                        }
                        continue;
                    }
                    let Some(process_group_id) = instance.process_group_id else {
                        self.mark_recovery_unknown(&service_id, anyhow!("缺少 WSL 进程组信息"));
                        continue;
                    };
                    let Some(expected_identity) = instance.recovery_identity.as_deref() else {
                        self.mark_recovery_unknown(
                            &service_id,
                            anyhow!("缺少 WSL 进程组恢复身份，拒绝接管 PGID {process_group_id}"),
                        );
                        continue;
                    };
                    match crate::process::wsl_group_status(
                        distribution,
                        process_group_id,
                        expected_identity,
                    )
                    .await
                    {
                        Ok(crate::process::WslGroupStatus::Matching) => {
                            self.attach_recovered_wsl_group(
                                &definition,
                                process_group_id,
                                expected_identity.to_owned(),
                                instance.process_id,
                            );
                        }
                        Ok(crate::process::WslGroupStatus::Missing) => {
                            self.clear_instance(&service_id);
                            self.set_simple_state(&service_id, ServiceState::Stopped, None);
                        }
                        Ok(crate::process::WslGroupStatus::Different) => {
                            self.clear_instance(&service_id);
                            self.set_simple_state(
                                &service_id,
                                ServiceState::Stopped,
                                Some(
                                    "旧实例已退出，原 PGID 已被其他进程复用；未接管或发送信号"
                                        .into(),
                                ),
                            );
                        }
                        Err(error) => self.mark_recovery_unknown(&service_id, error),
                    }
                }
                RuntimeConfig::DockerContainer { .. } => {
                    // Docker Engine remains the source of truth and is reconciled
                    // by discovery rather than by persisted process handles.
                }
            }
        }
    }

    fn attach_recovered_wsl_group(
        &self,
        definition: &ServiceDefinition,
        process_group_id: u32,
        recovery_identity: String,
        process_id: Option<u32>,
    ) {
        let RuntimeConfig::WslScript { distribution, .. } = &definition.runtime else {
            return;
        };
        let controller = ProcessController::Wsl {
            distribution: distribution.clone(),
            process_group_id,
            recovery_identity: recovery_identity.clone(),
        };
        self.inner
            .controllers
            .lock()
            .unwrap()
            .insert(definition.id.clone(), controller);
        self.mark_recovered(definition, process_id);
        let manager = self.clone();
        let service_id = definition.id.clone();
        let distribution = distribution.clone();
        tokio::spawn(async move {
            let mut monitor = ProcessMonitor::WslRecovered {
                distribution,
                process_group_id,
                recovery_identity,
            };
            let result = monitor.wait().await;
            manager.process_exited(&service_id, result);
        });
    }

    fn attach_recovered_process(&self, definition: &ServiceDefinition, spawned: SpawnedProcess) {
        let process_id = spawned.process_id;
        let SpawnedProcess {
            controller,
            mut monitor,
            stdout,
            stderr,
            ..
        } = spawned;
        self.inner
            .monitoring_lost
            .lock()
            .unwrap()
            .remove(&definition.id);
        self.inner
            .controllers
            .lock()
            .unwrap()
            .insert(definition.id.clone(), controller);
        self.mark_recovered(definition, process_id);
        self.spawn_log_reader(definition.id.clone(), stdout, LogStream::Stdout);
        self.spawn_log_reader(definition.id.clone(), stderr, LogStream::Stderr);
        let manager = self.clone();
        let service_id = definition.id.clone();
        tokio::spawn(async move {
            let result = monitor.wait().await;
            manager.process_exited(&service_id, result);
        });
    }

    fn mark_recovered(&self, definition: &ServiceDefinition, process_id: Option<u32>) {
        self.inner
            .monitoring_lost
            .lock()
            .unwrap()
            .remove(&definition.id);
        self.inner
            .started_at
            .lock()
            .unwrap()
            .insert(definition.id.clone(), tokio::time::Instant::now());
        let mut statuses = self.inner.statuses.write().unwrap();
        if let Some(status) = statuses.get_mut(&definition.id) {
            status.state = ServiceState::Running;
            status.health = if definition.health_check.is_some() {
                HealthState::Checking
            } else {
                HealthState::NotConfigured
            };
            status.process_id = process_id;
            status.message = Some("daemon 重启后已恢复托管".into());
            status.updated_at = Utc::now();
        }
        drop(statuses);
        self.emit_view(&definition.id);
    }

    fn mark_recovery_unknown(&self, service_id: &ServiceId, error: anyhow::Error) {
        let mut statuses = self.inner.statuses.write().unwrap();
        if let Some(status) = statuses.get_mut(service_id) {
            status.state = ServiceState::Unknown;
            status.message = Some(format!("无法恢复旧实例：{error}"));
            status.updated_at = Utc::now();
        }
        drop(statuses);
        self.emit_view(service_id);
    }

    pub async fn auto_start_services(&self) {
        let services: Vec<ServiceDefinition> = self
            .inner
            .config
            .read()
            .unwrap()
            .services
            .iter()
            .filter(|definition| definition.enabled && definition.auto_start)
            .cloned()
            .collect();
        for definition in services {
            let can_start = self
                .inner
                .statuses
                .read()
                .unwrap()
                .get(&definition.id)
                .is_some_and(|status| {
                    matches!(status.state, ServiceState::Stopped | ServiceState::Failed)
                });
            if !can_start {
                continue;
            }
            if let Err(error) = self.start(&definition.id).await {
                self.emit_log(
                    &definition.id,
                    LogStream::System,
                    format!("自动启动失败：{error:#}"),
                );
            }
        }
    }

    pub fn settings(&self) -> server_on_core::config::AppSettings {
        self.inner.config.read().unwrap().settings.clone()
    }

    pub fn update_settings(&self, settings: server_on_core::config::AppSettings) -> Result<()> {
        let _commit = self.inner.config_commit.lock().unwrap();
        let mut config = self.inner.config.read().unwrap().clone();
        config.settings = settings;
        config.validate()?;
        self.inner.store.save(&config)?;
        *self.inner.config.write().unwrap() = config;
        Ok(())
    }

    pub fn health_candidates(&self) -> Vec<(ServiceDefinition, ServiceStatus)> {
        let config = self.inner.config.read().unwrap();
        let statuses = self.inner.statuses.read().unwrap();
        config
            .services
            .iter()
            .filter(|definition| {
                definition.health_check.is_some()
                    && !matches!(definition.runtime, RuntimeConfig::DockerContainer { .. })
            })
            .filter_map(|definition| {
                let status = statuses.get(&definition.id)?.clone();
                matches!(
                    status.state,
                    ServiceState::Starting | ServiceState::Running | ServiceState::Degraded
                )
                .then(|| (definition.clone(), status))
            })
            .collect()
    }

    pub fn update_health(
        &self,
        service_id: &ServiceId,
        health: HealthState,
        message: Option<String>,
    ) {
        let mut changed = false;
        let mut statuses = self.inner.statuses.write().unwrap();
        if let Some(status) = statuses.get_mut(service_id) {
            if !matches!(
                status.state,
                ServiceState::Starting | ServiceState::Running | ServiceState::Degraded
            ) {
                return;
            }
            let desired_state = match (status.state, health) {
                (ServiceState::Running, HealthState::Unhealthy) => ServiceState::Degraded,
                (ServiceState::Degraded, HealthState::Healthy) => ServiceState::Running,
                (state, _) => state,
            };
            changed = status.health != health
                || status.state != desired_state
                || status.message != message;
            if changed {
                status.health = health;
                status.state = desired_state;
                status.message = message;
                status.updated_at = Utc::now();
            }
        }
        drop(statuses);
        if changed {
            self.emit_view(service_id);
        }
    }

    pub fn discovery_interval(&self) -> Duration {
        Duration::from_secs(
            self.inner
                .config
                .read()
                .unwrap()
                .settings
                .discovery
                .poll_interval_seconds
                .max(1),
        )
    }

    fn definition(&self, service_id: &ServiceId) -> Result<ServiceDefinition> {
        self.inner
            .config
            .read()
            .unwrap()
            .services
            .iter()
            .find(|service| &service.id == service_id)
            .cloned()
            .ok_or_else(|| anyhow!("服务不存在：{service_id}"))
    }

    fn begin_operation(
        &self,
        service_id: &ServiceId,
        operation: OperationKind,
    ) -> Result<OperationGuard> {
        {
            let mut operations = self.inner.operations.lock().unwrap();
            if !operations.insert(service_id.clone()) {
                bail!("服务已有启动、停止或重启操作正在执行");
            }
        }
        {
            let mut statuses = self.inner.statuses.write().unwrap();
            let status = statuses
                .entry(service_id.clone())
                .or_insert_with(|| ServiceStatus::new(service_id.clone()));
            status.active_operation = Some(operation);
            status.state = match operation {
                OperationKind::Start | OperationKind::Restart => ServiceState::Starting,
                OperationKind::Stop => ServiceState::Stopping,
                OperationKind::RefreshDiscovery => status.state,
            };
            status.updated_at = Utc::now();
        }
        self.emit_view(service_id);
        Ok(OperationGuard {
            inner: self.inner.clone(),
            service_id: service_id.clone(),
        })
    }

    async fn finish_operation(
        &self,
        service_id: &ServiceId,
        operation: OperationKind,
        guard: OperationGuard,
        result: Result<()>,
    ) -> Result<()> {
        if let Err(error) = &result {
            let mut statuses = self.inner.statuses.write().unwrap();
            if let Some(status) = statuses.get_mut(service_id) {
                status.state = ServiceState::Failed;
                status.message = Some(error.to_string());
                status.updated_at = Utc::now();
            }
        }
        let success = result.is_ok();
        let message = result.as_ref().err().map(|error| format!("{error:#}"));
        drop(guard);
        let _ = self.inner.events.send(DaemonEvent::OperationCompleted {
            service_id: service_id.clone(),
            operation,
            success,
            message,
        });
        result
    }

    fn set_simple_state(
        &self,
        service_id: &ServiceId,
        state: ServiceState,
        message: Option<String>,
    ) {
        let mut statuses = self.inner.statuses.write().unwrap();
        if let Some(status) = statuses.get_mut(service_id) {
            status.state = state;
            status.message = message;
            status.process_id = None;
            if state == ServiceState::Stopped {
                status.health = HealthState::NotConfigured;
            }
            status.updated_at = Utc::now();
        }
        drop(statuses);
        self.emit_view(service_id);
    }

    fn emit_view(&self, service_id: &ServiceId) {
        emit_view_inner(&self.inner, service_id);
    }

    fn emit_log(&self, service_id: &ServiceId, stream: LogStream, message: String) {
        let _record = self.inner.log_record.lock().unwrap();
        let event = LogEvent {
            service_id: service_id.clone(),
            timestamp: Utc::now(),
            stream,
            message,
            sequence: self.next_log_sequence(),
        };
        if let Err(error) = self.inner.logs.append(&event) {
            tracing::warn!(%error, "写入服务日志失败");
        }
        let _ = self.inner.events.send(DaemonEvent::Log(event));
    }

    fn next_log_sequence(&self) -> u64 {
        self.inner.log_sequence.fetch_add(1, Ordering::Relaxed) + 1
    }

    fn spawn_log_reader(
        &self,
        service_id: ServiceId,
        reader: impl AsyncRead + Unpin + Send + 'static,
        stream: LogStream,
    ) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut lines = BufReader::new(reader).lines();
            loop {
                match lines.next_line().await {
                    Ok(Some(line)) => manager.emit_log(&service_id, stream, line),
                    Ok(None) => break,
                    Err(error) => {
                        manager.emit_log(
                            &service_id,
                            LogStream::System,
                            format!("读取服务日志失败：{error}"),
                        );
                        break;
                    }
                }
            }
        });
    }
}

struct OperationGuard {
    inner: Arc<ManagerInner>,
    service_id: ServiceId,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.inner
            .operations
            .lock()
            .unwrap()
            .remove(&self.service_id);
        let mut statuses = self.inner.statuses.write().unwrap();
        if let Some(status) = statuses.get_mut(&self.service_id) {
            status.active_operation = None;
            status.updated_at = Utc::now();
        }
        drop(statuses);
        emit_view_inner(&self.inner, &self.service_id);
    }
}

fn status_changed(before: &ServiceStatus, after: &ServiceStatus) -> bool {
    before.service_id != after.service_id
        || before.state != after.state
        || before.health != after.health
        || before.message != after.message
        || before.process_id != after.process_id
        || before.container_id != after.container_id
        || before.last_exit_code != after.last_exit_code
        || before.active_operation != after.active_operation
}

fn emit_view_inner(inner: &Arc<ManagerInner>, service_id: &ServiceId) {
    let definition = inner
        .config
        .read()
        .unwrap()
        .services
        .iter()
        .find(|service| &service.id == service_id)
        .cloned();
    let status = inner.statuses.read().unwrap().get(service_id).cloned();
    if let (Some(definition), Some(status)) = (definition, status) {
        let _ = inner
            .events
            .send(DaemonEvent::ServiceUpdated(Box::new(ServiceView {
                definition,
                status,
            })));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::ProcessExit;

    fn test_manager() -> (ServiceManager, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let manager =
            ServiceManager::new(AppPaths::from_root(directory.path().to_path_buf())).unwrap();
        (manager, directory)
    }

    #[test]
    fn discovery_status_comparison_ignores_only_timestamp() {
        let before = ServiceStatus::new(ServiceId::from_string("service:test"));
        let mut after = before.clone();
        after.updated_at += chrono::Duration::seconds(15);
        assert!(!status_changed(&before, &after));

        let changes = [
            ServiceStatus {
                service_id: ServiceId::from_string("service:other"),
                ..before.clone()
            },
            ServiceStatus {
                state: ServiceState::Running,
                ..before.clone()
            },
            ServiceStatus {
                health: HealthState::Healthy,
                ..before.clone()
            },
            ServiceStatus {
                message: Some("changed".into()),
                ..before.clone()
            },
            ServiceStatus {
                process_id: Some(42),
                ..before.clone()
            },
            ServiceStatus {
                container_id: Some("container".into()),
                ..before.clone()
            },
            ServiceStatus {
                last_exit_code: Some(1),
                ..before.clone()
            },
            ServiceStatus {
                active_operation: Some(OperationKind::Start),
                ..before.clone()
            },
        ];
        for changed in changes {
            assert!(status_changed(&before, &changed));
        }
    }

    #[tokio::test]
    async fn operations_are_mutually_exclusive_and_guard_releases_state() {
        let (manager, _directory) = test_manager();
        let definition = ServiceDefinition::windows_script("api", "D:\\tmp", "echo hi");
        let id = definition.id.clone();
        manager.upsert_service(definition).unwrap();

        let guard = manager.begin_operation(&id, OperationKind::Start).unwrap();
        assert!(manager.begin_operation(&id, OperationKind::Stop).is_err());
        let statuses = manager.inner.statuses.read().unwrap();
        assert_eq!(
            statuses.get(&id).unwrap().active_operation,
            Some(OperationKind::Start)
        );
        assert_eq!(statuses.get(&id).unwrap().state, ServiceState::Starting);
        drop(statuses);
        drop(guard);
        let statuses = manager.inner.statuses.read().unwrap();
        assert!(statuses.get(&id).unwrap().active_operation.is_none());
    }

    #[tokio::test]
    async fn stop_rejects_while_another_operation_is_active() {
        let (manager, _directory) = test_manager();
        let definition = ServiceDefinition::windows_script("api", "D:\\tmp", "echo hi");
        let id = definition.id.clone();
        manager.upsert_service(definition).unwrap();

        let _guard = manager.begin_operation(&id, OperationKind::Start).unwrap();
        assert!(manager.stop(&id).await.is_err());
    }

    #[tokio::test]
    async fn active_operations_reject_edit_and_remove() {
        let (manager, _directory) = test_manager();
        let definition = ServiceDefinition::windows_script("api", "D:\\tmp", "echo hi");
        let id = definition.id.clone();
        manager.upsert_service(definition.clone()).unwrap();

        let guard = manager.begin_operation(&id, OperationKind::Start).unwrap();
        assert!(manager.upsert_service(definition).is_err());
        assert!(manager.remove_service(&id).is_err());
        assert!(manager.definition(&id).is_ok());
        drop(guard);
        assert!(manager.remove_service(&id).is_ok());
    }

    #[tokio::test]
    async fn monitoring_failure_preserves_instance_and_blocks_restart() {
        let (manager, _directory) = test_manager();
        let mut definition =
            ServiceDefinition::wsl_script("api", "ubuntu", "/home/user/app", "echo hi");
        definition.restart_policy = RestartPolicy::OnFailure {
            max_attempts: 2,
            delay_seconds: 0,
        };
        let id = definition.id.clone();
        manager.upsert_service(definition).unwrap();
        manager.inner.controllers.lock().unwrap().insert(
            id.clone(),
            ProcessController::Wsl {
                distribution: "ubuntu".into(),
                process_group_id: 123,
                recovery_identity: "boot:start".into(),
            },
        );
        manager.persist_instance(ServiceInstanceState {
            service_id: id.clone(),
            instance_id: uuid::Uuid::new_v4(),
            process_id: Some(123),
            process_group_id: Some(123),
            job_name: None,
            unit_name: None,
            process_started_at: Some(Utc::now()),
            recovery_identity: Some("boot:start".into()),
        });

        manager.process_exited(&id, Err(anyhow::anyhow!("query failed")));

        assert!(manager.inner.controllers.lock().unwrap().contains_key(&id));
        assert_eq!(manager.inner.persisted.lock().unwrap().instances.len(), 1);
        assert!(
            manager
                .inner
                .restart_attempts
                .lock()
                .unwrap()
                .get(&id)
                .is_none()
        );
        assert_eq!(
            manager
                .inner
                .statuses
                .read()
                .unwrap()
                .get(&id)
                .unwrap()
                .state,
            ServiceState::Unknown
        );
    }

    #[tokio::test]
    async fn exit_during_manual_stop_never_schedules_restart() {
        let (manager, _directory) = test_manager();
        let mut definition =
            ServiceDefinition::wsl_script("api", "ubuntu", "/home/user/app", "echo hi");
        definition.restart_policy = RestartPolicy::OnFailure {
            max_attempts: 2,
            delay_seconds: 0,
        };
        let id = definition.id.clone();
        manager.upsert_service(definition).unwrap();
        let guard = manager.begin_operation(&id, OperationKind::Stop).unwrap();

        manager.process_exited(
            &id,
            Ok(ProcessExit {
                code: Some(1),
                expected: false,
            }),
        );

        assert_eq!(
            manager
                .inner
                .statuses
                .read()
                .unwrap()
                .get(&id)
                .unwrap()
                .state,
            ServiceState::Stopped
        );
        assert!(
            manager
                .inner
                .restart_attempts
                .lock()
                .unwrap()
                .get(&id)
                .is_none()
        );
        drop(guard);
    }

    #[tokio::test]
    async fn emitted_logs_have_monotonic_sequences() {
        let (manager, _directory) = test_manager();
        let id = ServiceId::new();
        manager.emit_log(&id, LogStream::System, "first".into());
        manager.emit_log(&id, LogStream::Stdout, "second".into());

        let logs = manager.inner.logs.recent(&id, 10).unwrap();
        assert_eq!(logs.len(), 2);
        assert!(logs[0].sequence > 0);
        assert!(logs[0].sequence < logs[1].sequence);
    }

    #[tokio::test]
    async fn log_sequence_continues_after_daemon_restart() {
        let directory = tempfile::tempdir().unwrap();
        let paths = AppPaths::from_root(directory.path().to_path_buf());
        let id = ServiceId::new();
        LogStore::new(paths.logs_dir.clone(), 1_000_000, 3, 7)
            .append(&LogEvent {
                service_id: id.clone(),
                timestamp: Utc::now(),
                stream: LogStream::Stdout,
                message: "before restart".into(),
                sequence: 41,
            })
            .unwrap();
        let manager = ServiceManager::new(paths).unwrap();
        manager.emit_log(&id, LogStream::Stdout, "after restart".into());

        let logs = manager.inner.logs.recent(&id, 10).unwrap();
        assert_eq!(logs.last().unwrap().sequence, 42);
    }

    #[tokio::test]
    async fn start_rejects_disabled_services() {
        let (manager, _directory) = test_manager();
        let mut definition = ServiceDefinition::windows_script("api", "D:\\tmp", "echo hi");
        definition.enabled = false;
        let id = definition.id.clone();
        manager.upsert_service(definition).unwrap();
        assert!(manager.start(&id).await.is_err());
    }

    #[tokio::test]
    async fn failed_exits_consume_restart_attempts() {
        let (manager, _directory) = test_manager();
        let mut definition =
            ServiceDefinition::wsl_script("api", "ubuntu", "/home/user/app", "echo hi");
        definition.restart_policy = RestartPolicy::OnFailure {
            max_attempts: 2,
            // The delayed restart task sleeps this long and never runs in the
            // test; it is dropped with the runtime.
            delay_seconds: u64::MAX,
        };
        let id = definition.id.clone();
        manager.upsert_service(definition).unwrap();

        manager.process_exited(
            &id,
            Ok(ProcessExit {
                code: Some(1),
                expected: false,
            }),
        );
        assert_eq!(
            manager.inner.restart_attempts.lock().unwrap().get(&id),
            Some(&1)
        );
        assert_eq!(
            manager
                .inner
                .statuses
                .read()
                .unwrap()
                .get(&id)
                .unwrap()
                .state,
            ServiceState::Failed
        );

        manager.process_exited(
            &id,
            Ok(ProcessExit {
                code: Some(1),
                expected: false,
            }),
        );
        assert_eq!(
            manager.inner.restart_attempts.lock().unwrap().get(&id),
            Some(&2)
        );

        manager.process_exited(
            &id,
            Ok(ProcessExit {
                code: Some(1),
                expected: false,
            }),
        );
        assert_eq!(
            manager.inner.restart_attempts.lock().unwrap().get(&id),
            Some(&2)
        );
    }

    #[tokio::test]
    async fn clean_exit_marks_stopped_without_restart() {
        let (manager, _directory) = test_manager();
        let mut definition =
            ServiceDefinition::wsl_script("api", "ubuntu", "/home/user/app", "echo hi");
        definition.restart_policy = RestartPolicy::OnFailure {
            max_attempts: 3,
            delay_seconds: u64::MAX,
        };
        let id = definition.id.clone();
        manager.upsert_service(definition).unwrap();

        manager.process_exited(
            &id,
            Ok(ProcessExit {
                code: Some(0),
                expected: false,
            }),
        );
        assert_eq!(
            manager
                .inner
                .statuses
                .read()
                .unwrap()
                .get(&id)
                .unwrap()
                .state,
            ServiceState::Stopped
        );
        assert!(
            manager
                .inner
                .restart_attempts
                .lock()
                .unwrap()
                .get(&id)
                .is_none()
        );
    }
}
