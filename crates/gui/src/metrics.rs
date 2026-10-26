use crate::AppContext;
use chrono::Local;
use server_on_core::metrics::ServiceMetrics;
use server_on_core::protocol::{Command, ResponseData};
use server_on_core::state::ServiceState;
use slint::{ComponentHandle, ModelRc, VecModel, invoke_from_event_loop};
use std::time::Duration;
use tokio::sync::oneshot;

pub async fn watch(context: AppContext) {
    let mut selected = String::new();
    let mut last_poll = std::time::Instant::now() - Duration::from_secs(10);
    let mut last_docker_poll = std::time::Instant::now() - Duration::from_secs(10);
    loop {
        let current = context.shared.lock().unwrap().selected_id.clone();
        if current != selected {
            selected = current;
            reset(context.clone());
        }
        if selected.is_empty() || context.shared.lock().unwrap().draft {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        if !window_visible(&context).await {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        let (is_container, is_running) = {
            let shared = context.shared.lock().unwrap();
            shared
                .snapshot
                .services
                .iter()
                .find(|service| service.definition.id.as_str() == selected)
                .map(|service| {
                    (
                        matches!(
                            service.definition.runtime,
                            server_on_core::config::RuntimeConfig::DockerContainer { .. }
                        ),
                        matches!(
                            service.status.state,
                            ServiceState::Running | ServiceState::Degraded | ServiceState::Starting
                        ),
                    )
                })
                .unwrap_or((false, false))
        };
        if !is_running {
            reset(context.clone());
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }
        let now = std::time::Instant::now();
        let interval = if is_container {
            Duration::from_secs(5)
        } else {
            Duration::from_secs(2)
        };
        let due = if is_container {
            now.duration_since(last_docker_poll) >= interval
        } else {
            now.duration_since(last_poll) >= interval
        };
        if !due {
            tokio::time::sleep(Duration::from_millis(300)).await;
            continue;
        }
        if is_container {
            last_docker_poll = now;
        } else {
            last_poll = now;
        }
        let service_id = server_on_core::config::ServiceId::from_string(selected.clone());
        let Ok(client) = context.client() else {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        };
        let response = tokio::time::timeout(
            Duration::from_secs(6),
            client.request(Command::GetMetrics {
                service_id: service_id.clone(),
            }),
        )
        .await;
        match response {
            Ok(Ok(ResponseData::Metrics(metrics))) => {
                if context.shared.lock().unwrap().selected_id == selected {
                    update_metrics(context.clone(), *metrics);
                }
            }
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                let message = format!("指标查询失败：{error}");
                update_message(context.clone(), message);
            }
            Err(_) => {
                update_message(context.clone(), "指标查询超时".into());
            }
        }
    }
}

pub fn reset(context: AppContext) {
    let should_update = {
        let shared = context.shared.lock().unwrap();
        shared.metrics.is_some()
            || !shared.cpu_history.is_empty()
            || !shared.memory_history.is_empty()
    };
    if !should_update {
        return;
    }
    {
        let mut shared = context.shared.lock().unwrap();
        shared.metrics = None;
        shared.cpu_history.clear();
        shared.memory_history.clear();
    }
    update_metrics_ui(context, None, Vec::new(), Vec::new());
}

fn update_metrics(context: AppContext, metrics: ServiceMetrics) {
    let (cpu_history, memory_history) = {
        let mut shared = context.shared.lock().unwrap();
        if let Some(cpu) = metrics.cpu_percent {
            shared.cpu_history.push(cpu as f32);
            if shared.cpu_history.len() > 60 {
                shared.cpu_history.remove(0);
            }
        }
        if let Some(memory) = metrics.memory_bytes {
            shared.memory_history.push(memory as f64);
            if shared.memory_history.len() > 60 {
                shared.memory_history.remove(0);
            }
        }
        shared.metrics = Some(metrics.clone());
        (shared.cpu_history.clone(), shared.memory_history.clone())
    };
    update_metrics_ui(context, Some(metrics), cpu_history, memory_history);
}

fn update_message(context: AppContext, message: String) {
    let window = context.window.clone();
    let _ = invoke_from_event_loop(move || {
        if let Some(window) = window.upgrade() {
            window.set_metrics_message(message.into());
        }
    });
}

fn update_metrics_ui(
    context: AppContext,
    metrics: Option<ServiceMetrics>,
    cpu_history: Vec<f32>,
    memory_history: Vec<f64>,
) {
    let window = context.window.clone();
    let _ = invoke_from_event_loop(move || {
        let Some(window) = window.upgrade() else {
            return;
        };
        match metrics {
            Some(metrics) => {
                window.set_selected_cpu(
                    metrics
                        .cpu_percent
                        .map(|value| format!("{value:.1}").into())
                        .unwrap_or_else(|| "采样中".into()),
                );
                window.set_selected_memory(
                    metrics
                        .memory_bytes
                        .map(|value| format!("{:.1}", value as f64 / 1_048_576.0).into())
                        .unwrap_or_else(|| "—".into()),
                );
                window.set_selected_process_count(metrics.process_count.unwrap_or(0) as i32);
                window.set_selected_uptime(
                    metrics
                        .uptime_seconds
                        .map(format_uptime)
                        .unwrap_or_else(|| "—".into())
                        .into(),
                );
                window.set_metrics_sampled_at(
                    metrics
                        .sampled_at
                        .with_timezone(&Local)
                        .format("%H:%M:%S")
                        .to_string()
                        .into(),
                );
                window.set_metrics_message(
                    metrics
                        .message
                        .clone()
                        .unwrap_or_else(|| {
                            if metrics.cpu_percent.is_none() {
                                "CPU 首次采样中".into()
                            } else {
                                "".into()
                            }
                        })
                        .into(),
                );
            }
            None => {
                window.set_selected_cpu("—".into());
                window.set_selected_memory("—".into());
                window.set_selected_process_count(0);
                window.set_selected_uptime("—".into());
                window.set_metrics_sampled_at("".into());
                window.set_metrics_message("服务未运行或指标不可用".into());
            }
        }
        let memory_max = memory_history
            .iter()
            .copied()
            .fold(0.0_f64, f64::max)
            .max(1.0);
        window.set_cpu_history(ModelRc::new(VecModel::from(
            cpu_history
                .into_iter()
                .rev()
                .take(12)
                .rev()
                .map(|value| crate::MetricPoint {
                    value: (value / 100.0).clamp(0.0, 1.0),
                })
                .collect::<Vec<_>>(),
        )));
        window.set_memory_history(ModelRc::new(VecModel::from(
            memory_history
                .into_iter()
                .rev()
                .take(12)
                .rev()
                .map(|value| crate::MetricPoint {
                    value: (value / memory_max) as f32,
                })
                .collect::<Vec<_>>(),
        )));
    });
}

fn format_uptime(seconds: u64) -> String {
    let hours = seconds / 3_600;
    let minutes = (seconds % 3_600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {seconds}s")
    } else {
        format!("{seconds}s")
    }
}

async fn window_visible(context: &AppContext) -> bool {
    let (sender, receiver) = oneshot::channel();
    let window = context.window.clone();
    if invoke_from_event_loop(move || {
        let visible = window
            .upgrade()
            .map(|window| window.window().is_visible())
            .unwrap_or(false);
        let _ = sender.send(visible);
    })
    .is_err()
    {
        return false;
    }
    receiver.await.unwrap_or(false)
}
