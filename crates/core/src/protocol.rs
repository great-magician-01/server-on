use crate::config::{ServiceDefinition, ServiceId};
use crate::event::{DaemonEvent, LogEvent};
use crate::state::DaemonSnapshot;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClientRequest {
    pub request_id: Uuid,
    pub command: Command,
}

impl ClientRequest {
    pub fn new(command: Command) -> Self {
        Self {
            request_id: Uuid::new_v4(),
            command,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
pub enum Command {
    Ping,
    GetSnapshot,
    Subscribe,
    UpsertService(Box<ServiceDefinition>),
    RemoveService { service_id: ServiceId },
    StartService { service_id: ServiceId },
    StopService { service_id: ServiceId },
    RestartService { service_id: ServiceId },
    RefreshDiscovery,
    GetLogs { service_id: ServiceId, limit: usize },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorInfo {
    pub code: String,
    pub message: String,
}

impl ErrorInfo {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
pub enum ResponseData {
    Pong,
    Empty,
    Snapshot(DaemonSnapshot),
    Logs(Vec<LogEvent>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "kebab-case")]
pub enum ServerMessage {
    Response {
        request_id: Uuid,
        success: bool,
        data: Option<ResponseData>,
        error: Option<ErrorInfo>,
    },
    Event(Box<DaemonEvent>),
}

impl ServerMessage {
    pub fn success(request_id: Uuid, data: ResponseData) -> Self {
        Self::Response {
            request_id,
            success: true,
            data: Some(data),
            error: None,
        }
    }

    pub fn failure(request_id: Uuid, code: impl Into<String>, error: impl Into<String>) -> Self {
        Self::Response {
            request_id,
            success: false,
            data: None,
            error: Some(ErrorInfo::new(code, error)),
        }
    }
}
