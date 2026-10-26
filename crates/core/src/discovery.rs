use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WslDistributionState {
    Running,
    Stopped,
    Installing,
    #[default]
    Unknown,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DockerEngineState {
    Available,
    NotInstalled,
    Stopped,
    PermissionDenied,
    NotScanned,
    #[default]
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DockerEngineInfo {
    pub state: DockerEngineState,
    #[serde(default)]
    pub engine_id: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

impl DockerEngineInfo {
    pub fn unavailable(state: DockerEngineState, message: impl Into<String>) -> Self {
        Self {
            state,
            engine_id: None,
            message: Some(message.into()),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContainerInfo {
    pub id: String,
    pub name: String,
    pub image: String,
    pub state: String,
    #[serde(default)]
    pub health: Option<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub compose_project: Option<String>,
    #[serde(default)]
    pub compose_service: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DistributionInfo {
    pub name: String,
    pub is_default: bool,
    pub state: WslDistributionState,
    #[serde(default)]
    pub version: Option<u32>,
    pub docker: DockerEngineInfo,
    #[serde(default)]
    pub containers: Vec<ContainerInfo>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverySnapshot {
    pub distributions: Vec<DistributionInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_state_uses_stable_wire_names() {
        let value = serde_json::to_string(&DockerEngineState::PermissionDenied).unwrap();
        assert_eq!(value, "\"permission-denied\"");
    }
}
