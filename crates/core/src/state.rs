use crate::config::{ServiceDefinition, ServiceId};
use crate::discovery::DiscoverySnapshot;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceState {
    #[default]
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed,
    Degraded,
    Unknown,
    Missing,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HealthState {
    #[default]
    NotConfigured,
    Checking,
    Healthy,
    Unhealthy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OperationKind {
    Start,
    Stop,
    Restart,
    RefreshDiscovery,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServiceStatus {
    pub service_id: ServiceId,
    pub state: ServiceState,
    pub health: HealthState,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub process_id: Option<u32>,
    #[serde(default)]
    pub container_id: Option<String>,
    #[serde(default)]
    pub last_exit_code: Option<i32>,
    #[serde(default)]
    pub active_operation: Option<OperationKind>,
    pub updated_at: DateTime<Utc>,
}

impl ServiceStatus {
    pub fn new(service_id: ServiceId) -> Self {
        Self {
            service_id,
            state: ServiceState::Stopped,
            health: HealthState::NotConfigured,
            message: None,
            process_id: None,
            container_id: None,
            last_exit_code: None,
            active_operation: None,
            updated_at: Utc::now(),
        }
    }

    pub fn unknown(service_id: ServiceId, message: impl Into<String>) -> Self {
        Self {
            state: ServiceState::Unknown,
            message: Some(message.into()),
            ..Self::new(service_id)
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServiceView {
    pub definition: ServiceDefinition,
    pub status: ServiceStatus,
}

impl ServiceView {
    pub fn new(definition: ServiceDefinition) -> Self {
        let status = ServiceStatus::new(definition.id.clone());
        Self { definition, status }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonSnapshot {
    pub daemon_version: String,
    pub services: Vec<ServiceView>,
    pub discovery: DiscoverySnapshot,
    pub discovered_at: Option<DateTime<Utc>>,
}
