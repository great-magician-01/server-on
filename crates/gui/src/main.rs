#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use anyhow::{Result, anyhow, bail};
use chrono::Local;
use server_on_core::config::{
    HealthCheckConfig, RestartPolicy, RuntimeConfig, ServiceDefinition, ServiceId, ServiceOrigin,
    ThemePreference, WindowsShell,
};
use server_on_core::discovery::{ContainerInfo, DiscoverySnapshot, DockerEngineState};
use server_on_core::event::{DaemonEvent, LogEvent};
use server_on_core::protocol::{Command, ResponseData};
use server_on_core::state::{DaemonSnapshot, HealthState, ServiceState, ServiceView};
use server_on_daemon::ipc::DaemonClient;
use slint::{
    ComponentHandle, Model, ModelRc, SharedString, VecModel, Weak, invoke_from_event_loop,
};
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::runtime::Handle;

mod logs;
mod metrics;
mod single_instance;

use single_instance::SingleInstance;

slint::include_modules!();

struct SharedState {
    snapshot: DaemonSnapshot,
    selected_id: String,
    logs: logs::LogBuffer,
    metrics: Option<server_on_core::metrics::ServiceMetrics>,
    cpu_history: Vec<f32>,
    memory_history: Vec<f64>,
    draft: bool,
    config_revision: u64,
    save_generation: u64,
}

impl SharedState {
    fn new() -> Self {
        Self {
            snapshot: DaemonSnapshot {
                daemon_version: env!("CARGO_PKG_VERSION").into(),
                services: Vec::new(),
                discovery: DiscoverySnapshot::default(),
                discovered_at: None,
            },
            selected_id: String::new(),
            logs: logs::LogBuffer::default(),
            metrics: None,
            cpu_history: Vec::new(),
            memory_history: Vec::new(),
            draft: false,
            config_revision: 0,
            save_generation: 0,
        }
    }

    fn edit_config(&mut self) {
        self.config_revision = self.config_revision.wrapping_add(1);
    }

    fn begin_config_save(&mut self) -> (u64, u64) {
        self.save_generation = self.save_generation.wrapping_add(1);
        (self.config_revision, self.save_generation)
    }

    fn complete_config_save(
        &mut self,
        revision: u64,
        generation: u64,
        service_id: String,
    ) -> Option<bool> {
        if generation != self.save_generation {
            return None;
        }
        if revision != self.config_revision {
            return Some(false);
        }
        self.selected_id = service_id;
        self.draft = false;
        Some(true)
    }
}

#[derive(Clone)]
struct AppContext {
    runtime: Handle,
    window: Weak<AppWindow>,
    tray: Weak<AppTray>,
    client: Arc<Mutex<Option<DaemonClient>>>,
    shared: Arc<Mutex<SharedState>>,
}

impl AppContext {
    fn spawn<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        std::mem::drop(self.runtime.spawn(future));
    }

    fn client(&self) -> Result<DaemonClient> {
        self.client
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow!("守护进程尚未连接"))
    }

    fn render_now(&self) {
        let Some(window) = self.window.upgrade() else {
            return;
        };
        let (snapshot, selected, draft) = {
            let shared = self.shared.lock().unwrap();
            (
                shared.snapshot.clone(),
                shared.selected_id.clone(),
                shared.draft,
            )
        };
        let selected = render_services(&window, &snapshot, &selected, draft);
        let mut shared = self.shared.lock().unwrap();
        if shared.selected_id != selected {
            shared.edit_config();
        }
        shared.selected_id = selected;
        drop(shared);
        update_tray(&self.tray, &snapshot);
    }

    fn render_async(&self, snapshot: DaemonSnapshot) {
        let window = self.window.clone();
        let tray = self.tray.clone();
        let shared = self.shared.clone();
        let context = self.clone();
        let _ = invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                let (preferred, draft) = {
                    let shared = shared.lock().unwrap();
                    (shared.selected_id.clone(), shared.draft)
                };
                let selected = render_services(&window, &snapshot, &preferred, draft);
                let selection_changed = selected != preferred;
                let mut shared = shared.lock().unwrap();
                if selection_changed {
                    shared.edit_config();
                }
                shared.selected_id = selected;
                drop(shared);
                update_tray(&tray, &snapshot);
                if selection_changed {
                    context.clear_logs();
                    let load_context = context.clone();
                    context.spawn(async move {
                        if let Err(error) = load_selected_logs(load_context.clone()).await {
                            load_context.set_status(format!("读取日志失败：{error:#}"));
                        }
                    });
                }
            }
        });
    }

    fn render_shared_async(&self) {
        let snapshot = self.shared.lock().unwrap().snapshot.clone();
        self.render_async(snapshot);
    }

    fn set_connection(&self, connected: bool, message: String) {
        let window = self.window.clone();
        let _ = invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                window.set_daemon_connected(connected);
                window.set_daemon_status(SharedString::from(message));
            }
        });
    }

    fn set_status(&self, message: String) {
        let window = self.window.clone();
        let _ = invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                window.set_daemon_status(SharedString::from(message));
            }
        });
    }

    fn set_discovery_running(&self, running: bool) {
        let window = self.window.clone();
        let _ = invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                window.set_discovery_running(running);
            }
        });
    }

    fn finish_config_save(&self, revision: u64, generation: u64, service_id: String) {
        let window = self.window.clone();
        let shared = self.shared.clone();
        let context = self.clone();
        let _ = invoke_from_event_loop(move || {
            let Some(apply) = shared
                .lock()
                .unwrap()
                .complete_config_save(revision, generation, service_id)
            else {
                return;
            };
            if let Some(window) = window.upgrade() {
                window.set_config_saving(false);
                if apply {
                    window.set_config_dirty(false);
                    context.render_shared_async();
                }
            }
        });
    }

    fn finish_config_save_error(&self, generation: u64) {
        let window = self.window.clone();
        let shared = self.shared.clone();
        let _ = invoke_from_event_loop(move || {
            if shared.lock().unwrap().save_generation != generation {
                return;
            }
            if let Some(window) = window.upgrade() {
                window.set_config_saving(false);
            }
        });
    }

    fn render_logs_replace_async(&self) {
        let (selected, epoch) = {
            let shared = self.shared.lock().unwrap();
            (shared.selected_id.clone(), shared.logs.epoch())
        };
        let window = self.window.clone();
        let shared = self.shared.clone();
        let _ = invoke_from_event_loop(move || {
            if let Some(window) = window.upgrade() {
                replace_logs_from_shared(&window, &shared, &selected, epoch);
            }
        });
    }

    fn reset_log_view(&self) {
        self.shared.lock().unwrap().logs.invalidate();
        self.render_logs_replace_async();
    }

    fn flush_logs_async(&self) {
        let (selected, delta) = {
            let mut shared = self.shared.lock().unwrap();
            (shared.selected_id.clone(), shared.logs.take_delta())
        };
        let Some(delta) = delta else {
            return;
        };
        let window = self.window.clone();
        let shared = self.shared.clone();
        let _ = invoke_from_event_loop(move || {
            let Some(window) = window.upgrade() else {
                return;
            };
            let (total, replace) = {
                let shared = shared.lock().unwrap();
                if shared.selected_id != selected
                    || shared.logs.epoch() != delta.epoch
                    || window.get_selected_service_id() != selected
                    || window.get_log_epoch() != delta.epoch
                {
                    return;
                }
                let replace = delta.pending.len() + delta.evicted.len() > 512;
                (shared.logs.len(), replace)
            };
            if replace {
                replace_logs_from_shared(&window, &shared, &selected, delta.epoch);
                return;
            }
            if append_log_delta(&window, &delta) {
                window.set_log_count(total as i32);
                bump_log_generation(&window);
                return;
            }

            // A large eviction or an out-of-sync model is cheaper and safer to
            // replace once than to repair row by row.
            replace_logs_from_shared(&window, &shared, &selected, delta.epoch);
        });
    }

    fn clear_logs(&self) {
        self.shared.lock().unwrap().logs.clear();
        self.render_logs_replace_async();
    }
}

fn main() -> Result<()> {
    let Some(instance) = SingleInstance::acquire()? else {
        return Ok(());
    };
    let runtime = tokio::runtime::Runtime::new()?;
    let window = AppWindow::new()?;
    window.set_logs(ModelRc::new(VecModel::from(Vec::<LogLine>::new())));
    window.set_recent_logs(ModelRc::new(VecModel::from(Vec::<LogLine>::new())));
    let tray = AppTray::new()?;
    tray.show()?;
    instance.watch_activation(window.as_weak());
    window.set_app_version(env!("CARGO_PKG_VERSION").into());
    window.set_daemon_connected(false);
    window.set_daemon_status("正在连接守护进程…".into());
    let context = AppContext {
        runtime: runtime.handle().clone(),
        window: window.as_weak(),
        tray: tray.as_weak(),
        client: Arc::new(Mutex::new(None)),
        shared: Arc::new(Mutex::new(SharedState::new())),
    };
    context.render_now();

    tray.on_open_window({
        let window = window.as_weak();
        move || {
            if let Some(window) = window.upgrade() {
                let _ = window.show();
                window.window().set_minimized(false);
            }
        }
    });
    tray.on_refresh_requested({
        let context = context.clone();
        move || spawn_refresh_discovery(context.clone())
    });
    tray.on_quit_requested({
        let tray = tray.as_weak();
        move || {
            if let Some(tray) = tray.upgrade() {
                let _ = tray.hide();
            }
            let _ = slint::quit_event_loop();
        }
    });
    window.window().on_close_requested({
        let window_handle = window.as_weak();
        move || {
            let close_to_tray = window_handle
                .upgrade()
                .map(|window| window.get_close_to_tray())
                .unwrap_or(true);
            if close_to_tray {
                slint::CloseRequestResponse::HideWindow
            } else {
                let _ = slint::quit_event_loop();
                slint::CloseRequestResponse::HideWindow
            }
        }
    });

    window.on_select_service({
        let context = context.clone();
        let window_handle = window.as_weak();
        move |id| {
            if let Some(window) = window_handle.upgrade() {
                window.set_config_dirty(false);
                window.set_config_saving(false);
            }
            let id = id.to_string();
            {
                let mut shared = context.shared.lock().unwrap();
                shared.selected_id = id;
                shared.draft = false;
                shared.edit_config();
            }
            context.clear_logs();
            crate::metrics::reset(context.clone());
            context.render_now();
            let context = context.clone();
            context.clone().spawn(async move {
                if let Err(error) = load_selected_logs(context.clone()).await {
                    context.set_status(format!("读取日志失败：{error:#}"));
                }
            });
        }
    });
    window.on_start_service({
        let context = context.clone();
        move |id| {
            let id = ServiceId::from_string(id.to_string());
            spawn_operation(context.clone(), id, Operation::Start);
        }
    });
    window.on_stop_service({
        let context = context.clone();
        move |id| {
            let id = ServiceId::from_string(id.to_string());
            spawn_operation(context.clone(), id, Operation::Stop);
        }
    });
    window.on_restart_service({
        let context = context.clone();
        move |id| {
            let id = ServiceId::from_string(id.to_string());
            spawn_operation(context.clone(), id, Operation::Restart);
        }
    });
    window.on_refresh_discovery({
        let context = context.clone();
        move || spawn_refresh_discovery(context.clone())
    });
    window.on_create_service({
        let context = context.clone();
        let window_handle = window.as_weak();
        move |kind| {
            let Some(window) = window_handle.upgrade() else {
                return;
            };
            let is_wsl = kind == "wsl";
            let distribution = context
                .shared
                .lock()
                .unwrap()
                .snapshot
                .discovery
                .distributions
                .first()
                .map(|distribution| distribution.name.clone())
                .unwrap_or_else(|| "Ubuntu".into());
            let config = ServiceConfig {
                name: (if is_wsl {
                    "new-wsl-service"
                } else {
                    "new-windows-service"
                })
                .into(),
                runtime_kind: kind,
                type_label: (if is_wsl {
                    "WSL 脚本服务"
                } else {
                    "Windows 脚本服务"
                })
                .into(),
                origin_label: "手动创建".into(),
                state: "stopped".into(),
                health: "not-configured".into(),
                status_message: "尚未保存".into(),
                distribution: if is_wsl {
                    distribution.into()
                } else {
                    "".into()
                },
                working_directory: (if is_wsl { "/home" } else { "D:\\" }).into(),
                shell: (if is_wsl { "Bash" } else { "PowerShell" }).into(),
                script: "npm run dev".into(),
                health_check_kind: "none".into(),
                stop_timeout: 10,
                restart_policy: "不自动重启".into(),
                pid: "—".into(),
                uptime: "—".into(),
                cpu: "—".into(),
                memory: "—".into(),
                ..Default::default()
            };
            {
                let mut shared = context.shared.lock().unwrap();
                shared.selected_id.clear();
                shared.draft = true;
                shared.edit_config();
            }
            context.clear_logs();
            crate::metrics::reset(context.clone());
            window.set_selected_service_id("".into());
            window.set_selected_service(config);
            window.set_config_dirty(true);
            window.set_config_saving(false);
        }
    });
    window.on_config_edited({
        let context = context.clone();
        move || {
            let mut shared = context.shared.lock().unwrap();
            shared.edit_config();
        }
    });
    window.on_save_service({
        let context = context.clone();
        move |config| {
            let context = context.clone();
            let (revision, generation) = context.shared.lock().unwrap().begin_config_save();
            context.clone().spawn(async move {
                let result = save_service(context.clone(), config).await;
                match result {
                    Ok(service_id) => {
                        context.finish_config_save(revision, generation, service_id);
                        context.set_connection(true, "配置已保存".into());
                    }
                    Err(error) => {
                        context.finish_config_save_error(generation);
                        context.set_status(format!("保存失败：{error:#}"));
                    }
                }
            });
        }
    });
    window.on_search_changed({
        let context = context.clone();
        move |_| context.render_now()
    });
    window.on_filter_changed({
        let context = context.clone();
        move |_| context.render_now()
    });
    window.on_log_filter_changed({
        let context = context.clone();
        move |_| context.reset_log_view()
    });
    window.on_toggle_theme({
        let context = context.clone();
        move |dark| {
            let context = context.clone();
            context.clone().spawn(async move {
                let result = async {
                    let client = context.client()?;
                    let response = client.request(Command::GetSettings).await?;
                    let ResponseData::Settings(mut settings) = response else {
                        bail!("守护进程返回了无效设置");
                    };
                    settings.theme = if dark {
                        ThemePreference::Dark
                    } else {
                        ThemePreference::Light
                    };
                    client.request(Command::UpdateSettings(settings)).await?;
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(error) = result {
                    context.set_status(format!("主题保存失败：{error:#}"));
                }
            });
        }
    });
    window.on_tab_changed(|_| {});
    window.on_log_follow_toggled(|_| {});
    window.on_log_stream_changed({
        let context = context.clone();
        move |_| context.reset_log_view()
    });
    window.on_dismiss_toast(|| {});
    window.on_clear_logs({
        let context = context.clone();
        move || context.clear_logs()
    });

    context.clone().spawn(maintain_connection(context.clone()));
    context
        .clone()
        .spawn(crate::metrics::watch(context.clone()));

    window.run()?;
    Ok(())
}

async fn maintain_connection(context: AppContext) {
    let mut backoff = Duration::from_millis(250);
    loop {
        context.set_connection(false, "正在连接守护进程…".into());
        let connected = async {
            let client = DaemonClient::connect_or_start().await?;
            *context.client.lock().unwrap() = Some(client.clone());
            // The receiver is installed before this snapshot. Events arriving
            // while it is fetched remain queued for watch_events.
            let events = client.subscribe().await?;
            if let Err(error) = load_preferences(context.clone()).await {
                context.set_status(format!("读取界面设置失败：{error:#}"));
            }
            refresh_snapshot(context.clone()).await?;
            context.clear_logs();
            if let Err(error) = load_selected_logs(context.clone()).await {
                context.set_status(format!("读取日志失败：{error:#}"));
            }
            Ok::<_, anyhow::Error>((client, events))
        }
        .await;

        match connected {
            Ok((client, events)) => {
                backoff = Duration::from_millis(250);
                context.set_connection(true, "守护进程已连接".into());
                watch_events(context.clone(), client, events).await;
            }
            Err(error) => {
                context.set_connection(false, format!("守护进程连接失败：{error:#}"));
            }
        }

        *context.client.lock().unwrap() = None;
        context.set_connection(
            false,
            format!(
                "守护进程连接已断开，将在 {} 秒后重试",
                backoff.as_secs_f32()
            ),
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(5));
    }
}

async fn resync(context: AppContext) -> Result<()> {
    refresh_snapshot(context.clone()).await?;
    if let Err(error) = load_selected_logs(context.clone()).await {
        context.set_status(format!("读取日志失败：{error:#}"));
    }
    Ok(())
}

fn spawn_refresh_discovery(context: AppContext) {
    context.set_discovery_running(true);
    context.clone().spawn(async move {
        let result = async {
            let client = context.client()?;
            client.request(Command::RefreshDiscovery).await?;
            refresh_snapshot(context.clone()).await
        }
        .await;
        context.set_discovery_running(false);
        match result {
            Ok(()) => context.set_connection(true, "守护进程已连接".into()),
            Err(error) => context.set_status(format!("自动发现失败：{error:#}")),
        }
    });
}

#[derive(Clone, Copy)]
enum Operation {
    Start,
    Stop,
    Restart,
}

fn spawn_operation(context: AppContext, service_id: ServiceId, operation: Operation) {
    context.clone().spawn(async move {
        let label = match operation {
            Operation::Start => "启动",
            Operation::Stop => "停止",
            Operation::Restart => "重启",
        };
        let result = async {
            let client = context.client()?;
            let command = match operation {
                Operation::Start => Command::StartService {
                    service_id: service_id.clone(),
                },
                Operation::Stop => Command::StopService {
                    service_id: service_id.clone(),
                },
                Operation::Restart => Command::RestartService {
                    service_id: service_id.clone(),
                },
            };
            client.request(command).await?;
            refresh_snapshot(context.clone()).await
        }
        .await;
        match result {
            Ok(()) => context.set_connection(true, format!("{label}完成")),
            Err(error) => context.set_status(format!("{label}失败：{error:#}")),
        }
        if let Err(error) = load_selected_logs(context.clone()).await {
            context.set_status(format!("读取日志失败：{error:#}"));
        }
    });
}

async fn refresh_snapshot(context: AppContext) -> Result<()> {
    let client = context.client()?;
    let response = client.request(Command::GetSnapshot).await?;
    let ResponseData::Snapshot(snapshot) = response else {
        bail!("守护进程返回了无效快照");
    };
    context.shared.lock().unwrap().snapshot = snapshot.clone();
    context.render_async(snapshot);
    Ok(())
}

async fn load_preferences(context: AppContext) -> Result<()> {
    let client = context.client()?;
    let response = client.request(Command::GetSettings).await?;
    let ResponseData::Settings(settings) = response else {
        bail!("守护进程返回了无效设置");
    };
    let window = context.window.clone();
    let dark = settings.theme == ThemePreference::Dark;
    let close_to_tray = settings.close_to_tray;
    let _ = invoke_from_event_loop(move || {
        if let Some(window) = window.upgrade() {
            window.set_dark_theme(dark);
            window.set_close_to_tray(close_to_tray);
        }
    });
    Ok(())
}

async fn load_selected_logs(context: AppContext) -> Result<()> {
    let (service_id, load_seq) = {
        let mut shared = context.shared.lock().unwrap();
        (shared.selected_id.clone(), shared.logs.begin_load())
    };
    if service_id.is_empty() {
        context.clear_logs();
        return Ok(());
    }
    let service_id = ServiceId::from_string(service_id);
    let result = async {
        let client = context.client()?;
        let response = client
            .request(Command::GetLogs {
                service_id: service_id.clone(),
                limit: 500,
            })
            .await?;
        let ResponseData::Logs(logs) = response else {
            bail!("守护进程返回了无效日志");
        };
        Ok::<_, anyhow::Error>(logs)
    }
    .await;
    let logs = match result {
        Ok(logs) => logs,
        Err(error) => {
            context.shared.lock().unwrap().logs.cancel_load(load_seq);
            return Err(error);
        }
    };
    let loaded = {
        let mut shared = context.shared.lock().unwrap();
        if shared.selected_id == service_id.as_str() && shared.logs.load_should_apply(load_seq) {
            shared.logs.replace(logs);
            shared.logs.complete_load(load_seq);
            true
        } else {
            false
        }
    };
    if loaded {
        context.render_logs_replace_async();
    }
    Ok(())
}

async fn watch_events(
    context: AppContext,
    client: DaemonClient,
    mut events: tokio::sync::broadcast::Receiver<DaemonEvent>,
) {
    let mut disconnected = client.disconnected();
    if *disconnected.borrow() {
        return;
    }
    // Batch log deltas and service changes independently so neither event
    // stream can rebuild the UI once per daemon event.
    let mut log_flush = tokio::time::interval(Duration::from_millis(200));
    log_flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut service_flush = tokio::time::interval(Duration::from_millis(150));
    service_flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut logs_dirty = false;
    let mut services_dirty = false;
    loop {
        tokio::select! {
            event = events.recv() => {
                match event {
                    Ok(DaemonEvent::Log(log)) => {
                        let mut shared = context.shared.lock().unwrap();
                        if shared.selected_id == log.service_id.as_str() {
                            shared.logs.push(log);
                            drop(shared);
                            logs_dirty = true;
                        }
                    }
                    Ok(DaemonEvent::ServiceUpdated(view)) => {
                        let mut shared = context.shared.lock().unwrap();
                        if let Some(existing) = shared
                            .snapshot
                            .services
                            .iter_mut()
                            .find(|service| service.definition.id == view.definition.id)
                        {
                            *existing = *view;
                        } else {
                            shared.snapshot.services.push(*view);
                        }
                        drop(shared);
                        services_dirty = true;
                    }
                    Ok(DaemonEvent::ServiceRemoved(service_id)) => {
                        context
                            .shared
                            .lock()
                            .unwrap()
                            .snapshot
                            .services
                            .retain(|service| service.definition.id != service_id);
                        services_dirty = true;
                    }
                    Ok(DaemonEvent::OperationCompleted { .. })
                    | Ok(DaemonEvent::DiscoveryCompleted { .. }) => {
                        if refresh_snapshot(context.clone()).await.is_ok() {
                            services_dirty = false;
                        }
                    }
                    Ok(DaemonEvent::ResyncRequired)
                    | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        if resync(context.clone()).await.is_ok() {
                            services_dirty = false;
                            logs_dirty = false;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        context.set_connection(false, "守护进程事件连接已断开".into());
                        break;
                    }
                }
            }
            changed = disconnected.changed() => {
                if changed.is_err() || *disconnected.borrow() {
                    context.set_connection(false, "守护进程连接已断开".into());
                    break;
                }
            }
            _ = log_flush.tick() => {
                if logs_dirty {
                    logs_dirty = false;
                    context.flush_logs_async();
                }
            }
            _ = service_flush.tick() => {
                if services_dirty {
                    services_dirty = false;
                    context.render_shared_async();
                }
            }
        }
    }
}

async fn save_service(context: AppContext, config: ServiceConfig) -> Result<String> {
    if config.runtime_kind == "container" {
        bail!("容器由 Docker Engine 管理，不能通过脚本编辑器修改");
    }
    let snapshot = context.shared.lock().unwrap().snapshot.clone();
    let existing = snapshot
        .services
        .iter()
        .find(|service| service.definition.id.as_str() == config.id.as_str())
        .map(|service| service.definition.clone());
    let mut definition = match (&existing, config.runtime_kind.as_str()) {
        (Some(definition), _) => definition.clone(),
        (None, "wsl") => ServiceDefinition::wsl_script(
            config.name.to_string(),
            config.distribution.to_string(),
            config.working_directory.to_string(),
            config.script.to_string(),
        ),
        _ => ServiceDefinition::windows_script(
            config.name.to_string(),
            config.working_directory.to_string(),
            config.script.to_string(),
        ),
    };
    definition.name = config.name.trim().to_owned();
    definition.stop_timeout_seconds = u64::try_from(config.stop_timeout.max(1))?;
    definition.auto_start = config.auto_start;
    let environment = parse_environment(&config.environment)?;
    definition.runtime = match config.runtime_kind.as_str() {
        "wsl" => RuntimeConfig::WslScript {
            distribution: config.distribution.trim().to_owned(),
            working_directory: config.working_directory.to_string(),
            script: config.script.to_string(),
            environment,
        },
        "windows" => RuntimeConfig::WindowsScript {
            working_directory: config.working_directory.to_string(),
            shell: parse_windows_shell(&config.shell)?,
            script: config.script.to_string(),
            environment,
        },
        kind => bail!("不支持的服务类型：{kind}"),
    };
    definition.health_check = parse_health_check(&config)?;
    definition.origin = ServiceOrigin::User;
    let service_id = definition.id.clone();
    context
        .client()?
        .request(Command::UpsertService(Box::new(definition)))
        .await?;
    refresh_snapshot(context).await?;
    Ok(service_id.as_str().to_owned())
}

fn parse_windows_shell(value: &str) -> Result<WindowsShell> {
    match value {
        "PowerShell" => Ok(WindowsShell::PowerShell),
        "CMD" => Ok(WindowsShell::Cmd),
        _ => bail!("Windows Shell 必须是 PowerShell 或 CMD"),
    }
}

fn parse_environment(value: &str) -> Result<BTreeMap<String, String>> {
    let mut environment = BTreeMap::new();
    for line in value.lines().map(str::trim).filter(|line| !line.is_empty()) {
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| anyhow!("环境变量必须使用 KEY=VALUE：{line}"))?;
        environment.insert(key.trim().to_owned(), value.to_owned());
    }
    Ok(environment)
}

fn parse_health_check(config: &ServiceConfig) -> Result<Option<HealthCheckConfig>> {
    let target = config.health_target.trim();
    match config.health_check_kind.as_str() {
        "none" | "" => Ok(None),
        "http" => Ok(Some(HealthCheckConfig::Http {
            url: target.to_owned(),
            expected_status: None,
            interval_seconds: 10,
            timeout_ms: 3_000,
        })),
        "tcp" => {
            let (host, port) = target
                .rsplit_once(':')
                .ok_or_else(|| anyhow!("TCP 健康检查目标必须是 host:port"))?;
            Ok(Some(HealthCheckConfig::Tcp {
                host: host.trim_matches(['[', ']']).to_owned(),
                port: port.parse()?,
                interval_seconds: 10,
                timeout_ms: 3_000,
            }))
        }
        kind => bail!("不支持的健康检查类型：{kind}"),
    }
}

fn render_services(
    window: &AppWindow,
    snapshot: &DaemonSnapshot,
    preferred: &str,
    preserve_draft: bool,
) -> String {
    let query = window.get_search_text().to_string().to_lowercase();
    let mut services: Vec<ServiceItem> = snapshot
        .services
        .iter()
        .map(|view| {
            let (kind, distribution, subtitle) = service_item_metadata(snapshot, view);
            let matches_search = query.is_empty()
                || view.definition.name.to_lowercase().contains(&query)
                || distribution.to_lowercase().contains(&query)
                || subtitle.to_lowercase().contains(&query);
            ServiceItem {
                id: view.definition.id.as_str().into(),
                name: view.definition.name.clone().into(),
                kind: kind.into(),
                distribution: distribution.clone().into(),
                subtitle: subtitle.into(),
                state: state_name(view.status.state).into(),
                health: health_name(view.status.health).into(),
                matches_search,
            }
        })
        .collect();
    services.sort_by_key(|service| service.name.to_lowercase());
    let active_filter = window.get_active_filter().to_string();
    retain_visible_services(&mut services, &active_filter);
    let visible_service_count = services.len() as i32;
    let running_count = snapshot
        .services
        .iter()
        .filter(|service| {
            matches!(
                service.status.state,
                ServiceState::Running | ServiceState::Degraded
            )
        })
        .count() as i32;
    let issue_count = snapshot
        .services
        .iter()
        .filter(|service| {
            matches!(
                service.status.state,
                ServiceState::Failed
                    | ServiceState::Degraded
                    | ServiceState::Unknown
                    | ServiceState::Missing
            )
        })
        .count() as i32;
    let stopped_count = snapshot
        .services
        .iter()
        .filter(|service| service.status.state == ServiceState::Stopped)
        .count() as i32;
    let windows_count = snapshot
        .services
        .iter()
        .filter(|service| {
            matches!(
                service.definition.runtime,
                RuntimeConfig::WindowsScript { .. }
            )
        })
        .count() as i32;
    let distributions: Vec<DistributionItem> = snapshot
        .discovery
        .distributions
        .iter()
        .map(|distribution| DistributionItem {
            name: distribution.name.clone().into(),
            version: distribution
                .version
                .map(|version| format!("WSL {version}"))
                .unwrap_or_else(|| "WSL".into())
                .into(),
            state: match distribution.state {
                server_on_core::discovery::WslDistributionState::Running => "running",
                server_on_core::discovery::WslDistributionState::Stopped => "stopped",
                _ => "unknown",
            }
            .into(),
            engine_state: engine_name(distribution.docker.state).into(),
            engine_label: distribution
                .docker
                .message
                .clone()
                .unwrap_or_else(|| match distribution.docker.state {
                    DockerEngineState::Available => "Docker Engine 可用".into(),
                    DockerEngineState::NotInstalled => "Docker 未安装".into(),
                    DockerEngineState::Stopped => "Docker Engine 未运行".into(),
                    DockerEngineState::PermissionDenied => "Docker 权限不足".into(),
                    DockerEngineState::NotScanned => "尚未扫描".into(),
                    DockerEngineState::Unknown => "Docker 状态未知".into(),
                })
                .into(),
            container_count: distribution.containers.len() as i32,
        })
        .collect();
    let container_count: usize = snapshot
        .discovery
        .distributions
        .iter()
        .map(|distribution| distribution.containers.len())
        .sum();
    window.set_services(ModelRc::new(VecModel::from(services)));
    window.set_distributions(ModelRc::new(VecModel::from(distributions)));
    window.set_service_count(snapshot.services.len() as i32);
    window.set_running_count(running_count);
    window.set_issue_count(issue_count);
    window.set_stopped_count(stopped_count);
    window.set_windows_count(windows_count);
    window.set_visible_service_count(visible_service_count);
    window.set_container_count(container_count as i32);
    window.set_discovery_summary(
        format!(
            "{} 个发行版 · {container_count} 个容器",
            snapshot.discovery.distributions.len()
        )
        .into(),
    );
    window.set_last_discovery(
        snapshot
            .discovered_at
            .map(|time| time.with_timezone(&Local).format("%H:%M:%S").to_string())
            .unwrap_or_else(|| "尚未扫描".into())
            .into(),
    );

    let selected = if preserve_draft {
        String::new()
    } else if snapshot
        .services
        .iter()
        .any(|service| service.definition.id.as_str() == preferred)
    {
        preferred.to_owned()
    } else {
        snapshot
            .services
            .iter()
            .find(|service| {
                matches!(
                    service.status.state,
                    ServiceState::Running | ServiceState::Degraded
                )
            })
            .or_else(|| snapshot.services.first())
            .map(|service| service.definition.id.as_str().to_owned())
            .unwrap_or_default()
    };
    let editing = preserve_draft || window.get_config_dirty();
    window.set_selected_service_id(selected.clone().into());
    if !editing {
        if let Some(view) = snapshot
            .services
            .iter()
            .find(|service| service.definition.id.as_str() == selected)
        {
            window.set_selected_service(service_config(snapshot, view));
        } else {
            window.set_selected_service(ServiceConfig::default());
        }
    }
    selected
}

fn service_item_metadata(
    snapshot: &DaemonSnapshot,
    view: &ServiceView,
) -> (&'static str, String, String) {
    match &view.definition.runtime {
        RuntimeConfig::WindowsScript { shell, .. } => (
            "windows",
            String::new(),
            format!(
                "Windows · {}",
                if *shell == WindowsShell::Cmd {
                    "CMD"
                } else {
                    "PowerShell"
                }
            ),
        ),
        RuntimeConfig::WslScript { distribution, .. } => (
            "wsl",
            distribution.clone(),
            format!("{distribution} · Bash"),
        ),
        RuntimeConfig::DockerContainer {
            distribution,
            container_id,
            ..
        } => {
            let info = find_container(snapshot, distribution, container_id);
            (
                "container",
                distribution.clone(),
                info.map(|container| {
                    if container.ports.is_empty() {
                        container.image.clone()
                    } else {
                        format!("{} · {}", container.image, container.ports.join(", "))
                    }
                })
                .unwrap_or_else(|| "Docker 容器".into()),
            )
        }
    }
}

fn service_config(snapshot: &DaemonSnapshot, view: &ServiceView) -> ServiceConfig {
    let definition = &view.definition;
    let (
        runtime_kind,
        type_label,
        distribution,
        working_directory,
        shell,
        script,
        environment,
        detail,
        ports,
    ) = match &definition.runtime {
        RuntimeConfig::WindowsScript {
            working_directory,
            shell,
            script,
            environment,
        } => (
            "windows",
            "Windows 脚本服务",
            String::new(),
            working_directory.clone(),
            if *shell == WindowsShell::Cmd {
                "CMD"
            } else {
                "PowerShell"
            }
            .into(),
            script.clone(),
            environment_string(environment),
            "Windows Job Object".into(),
            String::new(),
        ),
        RuntimeConfig::WslScript {
            distribution,
            working_directory,
            script,
            environment,
        } => (
            "wsl",
            "WSL 脚本服务",
            distribution.clone(),
            working_directory.clone(),
            "Bash".into(),
            script.clone(),
            environment_string(environment),
            format!("{distribution} · Linux 进程组"),
            String::new(),
        ),
        RuntimeConfig::DockerContainer {
            distribution,
            container_id,
            ..
        } => {
            let info = find_container(snapshot, distribution, container_id);
            (
                "container",
                "Docker 容器",
                distribution.clone(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                info.map(|container| container.image.clone())
                    .unwrap_or_default(),
                info.map(|container| container.ports.join(", "))
                    .unwrap_or_default(),
            )
        }
    };
    let (health_check_kind, health_target) = match &definition.health_check {
        None => ("none", String::new()),
        Some(HealthCheckConfig::Http { url, .. }) => ("http", url.clone()),
        Some(HealthCheckConfig::Tcp { host, port, .. }) => ("tcp", format!("{host}:{port}")),
    };
    ServiceConfig {
        id: definition.id.as_str().into(),
        name: definition.name.clone().into(),
        runtime_kind: runtime_kind.into(),
        type_label: type_label.into(),
        origin_label: if definition.origin == ServiceOrigin::Discovered {
            "自动发现"
        } else {
            "手动创建"
        }
        .into(),
        state: state_name(view.status.state).into(),
        health: health_name(view.status.health).into(),
        detail: detail.into(),
        status_message: view
            .status
            .message
            .clone()
            .unwrap_or_else(|| "由守护进程管理".into())
            .into(),
        distribution: distribution.into(),
        working_directory: working_directory.into(),
        shell: shell.into(),
        script: script.into(),
        environment: environment.into(),
        health_check_kind: health_check_kind.into(),
        health_target: health_target.into(),
        stop_timeout: definition.stop_timeout_seconds as i32,
        auto_start: definition.auto_start,
        restart_policy: match definition.restart_policy {
            RestartPolicy::Never => "不自动重启".into(),
            RestartPolicy::OnFailure { max_attempts, .. } => {
                format!("失败时重启 · 最多 {max_attempts} 次").into()
            }
        },
        ports: ports.into(),
        pid: view
            .status
            .process_id
            .map(|pid| pid.to_string())
            .or_else(|| view.status.container_id.clone())
            .unwrap_or_else(|| "—".into())
            .into(),
        uptime: "—".into(),
        cpu: "—".into(),
        memory: "—".into(),
    }
}

fn find_container<'a>(
    snapshot: &'a DaemonSnapshot,
    distribution: &str,
    container_id: &str,
) -> Option<&'a ContainerInfo> {
    snapshot
        .discovery
        .distributions
        .iter()
        .find(|item| item.name == distribution)?
        .containers
        .iter()
        .find(|container| container.id == container_id)
}

fn environment_string(environment: &BTreeMap<String, String>) -> String {
    environment
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn replace_logs_from_shared(
    window: &AppWindow,
    shared: &Arc<Mutex<SharedState>>,
    selected: &str,
    expected_epoch: i32,
) {
    let (logs, epoch) = {
        let mut shared = shared.lock().unwrap();
        if shared.selected_id != selected || shared.logs.epoch() != expected_epoch {
            return;
        }
        // This snapshot includes pending events. Consume their deltas and
        // invalidate already queued appends so no line is rendered twice.
        let epoch = shared.logs.invalidate();
        (shared.logs.logs().to_vec(), epoch)
    };
    replace_log_models(window, &logs, epoch);
}

fn replace_log_models(window: &AppWindow, logs: &[LogEvent], epoch: i32) {
    let filter = window.get_log_filter().to_string().to_lowercase();
    let stream = window.get_log_stream_filter();
    let lines = logs::filtered_lines(logs, &filter, stream.as_str());
    let recent = logs::recent_tail(&lines);

    let logs_model = window.get_logs();
    let Some(logs_model) = logs_model.as_any().downcast_ref::<VecModel<LogLine>>() else {
        return;
    };
    logs_model.set_vec(lines);
    let recent_model = window.get_recent_logs();
    let Some(recent_model) = recent_model.as_any().downcast_ref::<VecModel<LogLine>>() else {
        return;
    };
    recent_model.set_vec(recent);
    window.set_log_count(logs.len() as i32);
    window.set_log_epoch(epoch);
    bump_log_generation(window);
}

fn append_log_delta(window: &AppWindow, delta: &logs::LogDelta) -> bool {
    let filter = window.get_log_filter().to_string().to_lowercase();
    let stream = window.get_log_stream_filter();
    let removed = delta
        .evicted
        .iter()
        .filter(|log| logs::matches(log, &filter, stream.as_str()))
        .count();
    if removed > 64 {
        return false;
    }
    let appended = logs::filtered_lines(&delta.pending, &filter, stream.as_str());
    let logs_model = window.get_logs();
    let Some(logs_model) = logs_model.as_any().downcast_ref::<VecModel<LogLine>>() else {
        return false;
    };
    if logs_model.row_count() + appended.len() < removed {
        return false;
    }
    logs_model.extend(appended);
    for _ in 0..removed {
        logs_model.remove(0);
    }
    update_recent_logs(window)
}

fn update_recent_logs(window: &AppWindow) -> bool {
    let logs_model = window.get_logs();
    let Some(logs_model) = logs_model.as_any().downcast_ref::<VecModel<LogLine>>() else {
        return false;
    };
    let start = logs_model.row_count().saturating_sub(logs::RECENT_CAP);
    let recent = (start..logs_model.row_count())
        .filter_map(|index| logs_model.row_data(index))
        .collect::<Vec<_>>();
    let recent_model = window.get_recent_logs();
    let Some(recent_model) = recent_model.as_any().downcast_ref::<VecModel<LogLine>>() else {
        return false;
    };
    recent_model.set_vec(recent);
    true
}

fn bump_log_generation(window: &AppWindow) {
    window.set_log_generation(window.get_log_generation().wrapping_add(1));
}

fn update_tray(tray: &Weak<AppTray>, snapshot: &DaemonSnapshot) {
    if let Some(tray) = tray.upgrade() {
        let running = snapshot
            .services
            .iter()
            .filter(|service| {
                matches!(
                    service.status.state,
                    ServiceState::Running | ServiceState::Degraded
                )
            })
            .count();
        let issues = snapshot
            .services
            .iter()
            .filter(|service| {
                matches!(
                    service.status.state,
                    ServiceState::Failed
                        | ServiceState::Degraded
                        | ServiceState::Unknown
                        | ServiceState::Missing
                )
            })
            .count();
        tray.set_summary(format!("ServerOn · {running} 运行中 · {issues} 需要关注").into());
    }
}

fn retain_visible_services(services: &mut Vec<ServiceItem>, active_filter: &str) {
    services.retain(|item| item.matches_search && filter_matches(&item.state, active_filter));
}

fn filter_matches(state: &str, filter: &str) -> bool {
    match filter {
        "running" => matches!(state, "running" | "degraded" | "starting" | "stopping"),
        "issues" => matches!(state, "failed" | "degraded" | "unknown" | "missing"),
        "stopped" => state == "stopped",
        _ => true,
    }
}

fn state_name(state: ServiceState) -> &'static str {
    match state {
        ServiceState::Stopped => "stopped",
        ServiceState::Starting => "starting",
        ServiceState::Running => "running",
        ServiceState::Stopping => "stopping",
        ServiceState::Failed => "failed",
        ServiceState::Degraded => "degraded",
        ServiceState::Unknown => "unknown",
        ServiceState::Missing => "missing",
    }
}

fn health_name(health: HealthState) -> &'static str {
    match health {
        HealthState::NotConfigured => "not-configured",
        HealthState::Checking => "checking",
        HealthState::Healthy => "healthy",
        HealthState::Unhealthy => "unhealthy",
    }
}

fn engine_name(state: DockerEngineState) -> &'static str {
    match state {
        DockerEngineState::Available => "available",
        DockerEngineState::NotInstalled => "not-installed",
        DockerEngineState::Stopped => "stopped",
        DockerEngineState::PermissionDenied => "permission-denied",
        DockerEngineState::NotScanned => "not-scanned",
        DockerEngineState::Unknown => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service(name: &str, state: &str) -> ServiceItem {
        ServiceItem {
            name: name.into(),
            state: state.into(),
            matches_search: true,
            ..ServiceItem::default()
        }
    }

    #[test]
    fn running_filter_removes_stopped_rows_between_visible_services() {
        let mut services = vec![
            service("any-llm", "running"),
            service("middle-stopped", "stopped"),
            service("postgres", "running"),
        ];

        retain_visible_services(&mut services, "running");

        let names: Vec<_> = services
            .iter()
            .map(|service| service.name.as_str())
            .collect();
        assert_eq!(names, ["any-llm", "postgres"]);
    }

    #[test]
    fn stale_save_completion_does_not_replace_newer_edits_or_selection() {
        let mut shared = SharedState::new();
        shared.selected_id = "service:a".into();
        let (revision, generation) = shared.begin_config_save();
        shared.edit_config();

        assert_eq!(
            shared.complete_config_save(revision, generation, "service:a".into()),
            Some(false)
        );
        assert_eq!(shared.selected_id, "service:a");

        shared.selected_id = "service:b".into();
        shared.edit_config();
        assert_eq!(
            shared.complete_config_save(revision, generation, "service:a".into()),
            Some(false)
        );
        assert_eq!(shared.selected_id, "service:b");
    }

    #[test]
    fn only_latest_save_generation_can_finish() {
        let mut shared = SharedState::new();
        let (_, first_generation) = shared.begin_config_save();
        let (revision, second_generation) = shared.begin_config_save();

        assert_eq!(
            shared.complete_config_save(revision, first_generation, "service:old".into()),
            None
        );
        assert_eq!(
            shared.complete_config_save(revision, second_generation, "service:new".into()),
            Some(true)
        );
        assert_eq!(shared.selected_id, "service:new");
    }

    #[test]
    fn windows_shell_is_strictly_validated() {
        assert_eq!(
            parse_windows_shell("PowerShell").unwrap(),
            WindowsShell::PowerShell
        );
        assert_eq!(parse_windows_shell("CMD").unwrap(), WindowsShell::Cmd);
        assert!(parse_windows_shell("cmd").is_err());
        assert!(parse_windows_shell("bash").is_err());
    }
}
