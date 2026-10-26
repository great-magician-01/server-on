use crate::config::ServiceId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct AppPaths {
    pub root: PathBuf,
    pub config_file: PathBuf,
    pub state_file: PathBuf,
    pub logs_dir: PathBuf,
    pub backups_dir: PathBuf,
}

impl AppPaths {
    pub fn local() -> Self {
        let base = std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        Self::from_root(base.join("ServerOn"))
    }

    pub fn from_root(root: PathBuf) -> Self {
        Self {
            config_file: root.join("config.json"),
            state_file: root.join("state.json"),
            logs_dir: root.join("logs"),
            backups_dir: root.join("backups"),
            root,
        }
    }

    pub fn pipe_name() -> String {
        let user = std::env::var("USERNAME").unwrap_or_else(|_| "unknown".into());
        format!(r"\\.\pipe\server-on-{user}")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PersistedState {
    #[serde(default)]
    pub instances: Vec<ServiceInstanceState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ServiceInstanceState {
    pub service_id: ServiceId,
    pub instance_id: uuid::Uuid,
    pub process_id: Option<u32>,
    pub process_group_id: Option<u32>,
    #[serde(default)]
    pub job_name: Option<String>,
    #[serde(default)]
    pub unit_name: Option<String>,
    pub process_started_at: Option<chrono::DateTime<chrono::Utc>>,
}
