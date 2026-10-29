use crate::config::ServiceId;
use crate::state::{OperationKind, ServiceView};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogStream {
    Stdout,
    Stderr,
    System,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct LogEvent {
    pub service_id: ServiceId,
    pub timestamp: DateTime<Utc>,
    pub stream: LogStream,
    pub message: String,
    #[serde(default)]
    pub sequence: u64,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
pub enum DaemonEvent {
    ServiceUpdated(Box<ServiceView>),
    ServiceRemoved(ServiceId),
    Log(LogEvent),
    DiscoveryCompleted {
        discovered_at: DateTime<Utc>,
    },
    OperationCompleted {
        service_id: ServiceId,
        operation: OperationKind,
        success: bool,
        message: Option<String>,
    },
    /// The sender dropped events, so incremental state may no longer be complete.
    ResyncRequired,
}
