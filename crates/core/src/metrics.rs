use crate::config::ServiceId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ServiceMetrics {
    pub service_id: ServiceId,
    pub sampled_at: DateTime<Utc>,
    #[serde(default)]
    pub cpu_percent: Option<f64>,
    #[serde(default)]
    pub memory_bytes: Option<u64>,
    #[serde(default)]
    pub process_count: Option<u32>,
    #[serde(default)]
    pub uptime_seconds: Option<u64>,
    #[serde(default)]
    pub message: Option<String>,
}

impl ServiceMetrics {
    pub fn unavailable(service_id: ServiceId, message: impl Into<String>) -> Self {
        Self {
            service_id,
            sampled_at: Utc::now(),
            cpu_percent: None,
            memory_bytes: None,
            process_count: None,
            uptime_seconds: None,
            message: Some(message.into()),
        }
    }
}
