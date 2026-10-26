use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use thiserror::Error;
use uuid::Uuid;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ServiceId(String);

impl Default for ServiceId {
    fn default() -> Self {
        Self::new()
    }
}

impl ServiceId {
    pub fn new() -> Self {
        Self(format!("service:{}", Uuid::new_v4()))
    }

    pub fn for_container(distribution: &str, engine_id: &str, container_id: &str) -> Self {
        Self(format!(
            "container:{distribution}:{engine_id}:{container_id}"
        ))
    }

    pub fn from_string(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ServiceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ServiceOrigin {
    #[default]
    User,
    Discovered,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WindowsShell {
    #[default]
    PowerShell,
    Cmd,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RuntimeConfig {
    WindowsScript {
        working_directory: String,
        shell: WindowsShell,
        script: String,
        #[serde(default)]
        environment: BTreeMap<String, String>,
    },
    WslScript {
        distribution: String,
        working_directory: String,
        script: String,
        #[serde(default)]
        environment: BTreeMap<String, String>,
    },
    DockerContainer {
        distribution: String,
        engine_id: String,
        container_id: String,
        container_name: String,
        #[serde(default)]
        compose_project: Option<String>,
        #[serde(default)]
        compose_service: Option<String>,
    },
}

impl RuntimeConfig {
    pub fn environment(&self) -> Option<&BTreeMap<String, String>> {
        match self {
            Self::WindowsScript { environment, .. } | Self::WslScript { environment, .. } => {
                Some(environment)
            }
            Self::DockerContainer { .. } => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum HealthCheckConfig {
    Tcp {
        host: String,
        port: u16,
        #[serde(default = "default_health_interval")]
        interval_seconds: u64,
        #[serde(default = "default_health_timeout")]
        timeout_ms: u64,
    },
    Http {
        url: String,
        #[serde(default)]
        expected_status: Option<u16>,
        #[serde(default = "default_health_interval")]
        interval_seconds: u64,
        #[serde(default = "default_health_timeout")]
        timeout_ms: u64,
    },
}

const fn default_health_interval() -> u64 {
    10
}

const fn default_health_timeout() -> u64 {
    3_000
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum RestartPolicy {
    #[default]
    Never,
    OnFailure {
        #[serde(default = "default_restart_attempts")]
        max_attempts: u32,
        #[serde(default = "default_restart_delay")]
        delay_seconds: u64,
    },
}

const fn default_restart_attempts() -> u32 {
    3
}

const fn default_restart_delay() -> u64 {
    5
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "condition", rename_all = "kebab-case")]
pub enum ServiceDependency {
    Running { service_id: ServiceId },
    Healthy { service_id: ServiceId },
}

impl ServiceDependency {
    pub fn service_id(&self) -> &ServiceId {
        match self {
            Self::Running { service_id } | Self::Healthy { service_id } => service_id,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServiceDefinition {
    pub id: ServiceId,
    pub name: String,
    #[serde(default)]
    pub group: Option<String>,
    #[serde(default)]
    pub origin: ServiceOrigin,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    pub runtime: RuntimeConfig,
    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,
    #[serde(default = "default_stop_timeout")]
    pub stop_timeout_seconds: u64,
    #[serde(default)]
    pub auto_start: bool,
    #[serde(default)]
    pub restart_policy: RestartPolicy,
    #[serde(default)]
    pub depends_on: Vec<ServiceDependency>,
}

const fn default_enabled() -> bool {
    true
}

const fn default_stop_timeout() -> u64 {
    10
}

impl ServiceDefinition {
    pub fn windows_script(
        name: impl Into<String>,
        working_directory: impl Into<String>,
        script: impl Into<String>,
    ) -> Self {
        Self {
            id: ServiceId::new(),
            name: name.into(),
            group: None,
            origin: ServiceOrigin::User,
            enabled: true,
            runtime: RuntimeConfig::WindowsScript {
                working_directory: working_directory.into(),
                shell: WindowsShell::PowerShell,
                script: script.into(),
                environment: BTreeMap::new(),
            },
            health_check: None,
            stop_timeout_seconds: default_stop_timeout(),
            auto_start: false,
            restart_policy: RestartPolicy::Never,
            depends_on: Vec::new(),
        }
    }

    pub fn wsl_script(
        name: impl Into<String>,
        distribution: impl Into<String>,
        working_directory: impl Into<String>,
        script: impl Into<String>,
    ) -> Self {
        Self {
            id: ServiceId::new(),
            name: name.into(),
            group: None,
            origin: ServiceOrigin::User,
            enabled: true,
            runtime: RuntimeConfig::WslScript {
                distribution: distribution.into(),
                working_directory: working_directory.into(),
                script: script.into(),
                environment: BTreeMap::new(),
            },
            health_check: None,
            stop_timeout_seconds: default_stop_timeout(),
            auto_start: false,
            restart_policy: RestartPolicy::Never,
            depends_on: Vec::new(),
        }
    }

    pub fn docker_container(
        distribution: impl Into<String>,
        engine_id: impl Into<String>,
        container_id: impl Into<String>,
        container_name: impl Into<String>,
        compose_project: Option<String>,
        compose_service: Option<String>,
    ) -> Self {
        let distribution = distribution.into();
        let engine_id = engine_id.into();
        let container_id = container_id.into();
        let container_name = container_name.into();
        Self {
            id: ServiceId::for_container(&distribution, &engine_id, &container_id),
            name: container_name.clone(),
            group: compose_project.clone(),
            origin: ServiceOrigin::Discovered,
            enabled: true,
            runtime: RuntimeConfig::DockerContainer {
                distribution,
                engine_id,
                container_id,
                container_name,
                compose_project,
                compose_service,
            },
            health_check: None,
            stop_timeout_seconds: default_stop_timeout(),
            auto_start: false,
            restart_policy: RestartPolicy::Never,
            depends_on: Vec::new(),
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.id.as_str().trim().is_empty() {
            return Err("服务 ID 不能为空".into());
        }
        if self.name.trim().is_empty() {
            return Err("服务名称不能为空".into());
        }
        if self
            .group
            .as_ref()
            .is_some_and(|group| group.trim().is_empty())
        {
            return Err("分组名称不能为空".into());
        }
        if !(1..=600).contains(&self.stop_timeout_seconds) {
            return Err("停止超时必须在 1 到 600 秒之间".into());
        }
        if self
            .depends_on
            .iter()
            .any(|dependency| dependency.service_id() == &self.id)
        {
            return Err("服务不能依赖自身".into());
        }

        match &self.runtime {
            RuntimeConfig::WindowsScript {
                working_directory,
                script,
                environment,
                ..
            }
            | RuntimeConfig::WslScript {
                working_directory,
                script,
                environment,
                ..
            } => {
                if working_directory.trim().is_empty() {
                    return Err("工作目录不能为空".into());
                }
                if matches!(&self.runtime, RuntimeConfig::WslScript { .. })
                    && !(working_directory.starts_with('/') || working_directory.starts_with('~'))
                {
                    return Err("WSL 工作目录必须是 Linux 绝对路径，例如 /home/user/project".into());
                }
                if script.trim().is_empty() {
                    return Err("启动脚本不能为空".into());
                }
                validate_environment(environment)?;
            }
            RuntimeConfig::DockerContainer {
                distribution,
                engine_id,
                container_id,
                container_name,
                ..
            } => {
                if distribution.trim().is_empty()
                    || engine_id.trim().is_empty()
                    || container_id.trim().is_empty()
                    || container_name.trim().is_empty()
                {
                    return Err("容器发行版、Engine、ID 和名称不能为空".into());
                }
            }
        }

        if let Some(health_check) = &self.health_check {
            match health_check {
                HealthCheckConfig::Tcp {
                    host,
                    interval_seconds,
                    timeout_ms,
                    ..
                } => {
                    if host.trim().is_empty() {
                        return Err("TCP 健康检查主机不能为空".into());
                    }
                    validate_health_timing(*interval_seconds, *timeout_ms)?;
                }
                HealthCheckConfig::Http {
                    url,
                    expected_status,
                    interval_seconds,
                    timeout_ms,
                } => {
                    if !(url.starts_with("http://") || url.starts_with("https://")) {
                        return Err("HTTP 健康检查必须使用 http:// 或 https://".into());
                    }
                    if expected_status.is_some_and(|status| !(100..=599).contains(&status)) {
                        return Err("HTTP 期望状态码无效".into());
                    }
                    validate_health_timing(*interval_seconds, *timeout_ms)?;
                }
            }
        }

        Ok(())
    }
}

fn validate_environment(environment: &BTreeMap<String, String>) -> Result<(), String> {
    for (key, value) in environment {
        if key.trim().is_empty() || key.contains('=') || key.contains('\0') {
            return Err(format!("环境变量名无效：{key:?}"));
        }
        if value.contains('\0') {
            return Err(format!("环境变量 {key} 包含 NUL 字符"));
        }
    }
    Ok(())
}

fn validate_health_timing(interval_seconds: u64, timeout_ms: u64) -> Result<(), String> {
    if interval_seconds == 0 || timeout_ms == 0 {
        return Err("健康检查间隔和超时必须大于零".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoverySettings {
    #[serde(default)]
    pub scan_stopped_distributions: bool,
    #[serde(default = "default_discovery_interval")]
    pub poll_interval_seconds: u64,
}

const fn default_discovery_interval() -> u64 {
    15
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSettings {
    #[serde(default = "default_log_file_size")]
    pub max_file_bytes: u64,
    #[serde(default = "default_log_files")]
    pub max_files: u32,
    #[serde(default = "default_log_retention")]
    pub retention_days: u32,
    #[serde(default = "default_log_lines")]
    pub memory_lines: usize,
}

const fn default_log_file_size() -> u64 {
    4 * 1024 * 1024
}

const fn default_log_files() -> u32 {
    5
}

const fn default_log_retention() -> u32 {
    7
}

const fn default_log_lines() -> usize {
    2_000
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThemePreference {
    #[default]
    Dark,
    Light,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppSettings {
    #[serde(default = "default_enabled")]
    pub close_to_tray: bool,
    #[serde(default)]
    pub theme: ThemePreference,
    #[serde(default)]
    pub start_daemon_on_login: bool,
    #[serde(default = "default_discovery_settings")]
    pub discovery: DiscoverySettings,
    #[serde(default = "default_log_settings")]
    pub logs: LogSettings,
}

fn default_discovery_settings() -> DiscoverySettings {
    DiscoverySettings {
        scan_stopped_distributions: false,
        poll_interval_seconds: default_discovery_interval(),
    }
}

fn default_log_settings() -> LogSettings {
    LogSettings {
        max_file_bytes: default_log_file_size(),
        max_files: default_log_files(),
        retention_days: default_log_retention(),
        memory_lines: default_log_lines(),
    }
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            theme: ThemePreference::Dark,
            start_daemon_on_login: false,
            discovery: default_discovery_settings(),
            logs: default_log_settings(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "current_schema_version")]
    pub schema_version: u32,
    #[serde(default)]
    pub settings: AppSettings,
    #[serde(default)]
    pub services: Vec<ServiceDefinition>,
}

const fn current_schema_version() -> u32 {
    CURRENT_SCHEMA_VERSION
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            settings: AppSettings::default(),
            services: Vec::new(),
        }
    }
}

impl AppConfig {
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.schema_version == 0 || self.schema_version > CURRENT_SCHEMA_VERSION {
            return Err(ConfigError::UnsupportedSchema(self.schema_version));
        }
        if self.settings.discovery.poll_interval_seconds == 0
            || self.settings.logs.max_file_bytes == 0
            || self.settings.logs.max_files == 0
            || self.settings.logs.memory_lines == 0
        {
            return Err(ConfigError::InvalidSettings(
                "发现间隔、日志大小、文件数量和缓存行数必须大于零".into(),
            ));
        }

        let mut ids = BTreeSet::new();
        for service in &self.services {
            service
                .validate()
                .map_err(|reason| ConfigError::InvalidService {
                    service_id: service.id.to_string(),
                    reason,
                })?;
            if !ids.insert(service.id.clone()) {
                return Err(ConfigError::DuplicateService(service.id.to_string()));
            }
        }

        for service in &self.services {
            for dependency in &service.depends_on {
                if !ids.contains(dependency.service_id()) {
                    return Err(ConfigError::UnknownDependency {
                        service_id: service.id.to_string(),
                        dependency_id: dependency.service_id().to_string(),
                    });
                }
            }
            detect_dependency_cycle(service, &self.services, &mut Vec::new())?;
        }
        Ok(())
    }
}

fn detect_dependency_cycle(
    service: &ServiceDefinition,
    services: &[ServiceDefinition],
    path: &mut Vec<ServiceId>,
) -> Result<(), ConfigError> {
    if path.contains(&service.id) {
        return Err(ConfigError::DependencyCycle(service.id.to_string()));
    }
    path.push(service.id.clone());
    for dependency in &service.depends_on {
        if let Some(parent) = services
            .iter()
            .find(|candidate| &candidate.id == dependency.service_id())
        {
            detect_dependency_cycle(parent, services, path)?;
        }
    }
    path.pop();
    Ok(())
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("不支持的配置版本：{0}")]
    UnsupportedSchema(u32),
    #[error("服务 {service_id} 配置无效：{reason}")]
    InvalidService { service_id: String, reason: String },
    #[error("服务 ID 重复：{0}")]
    DuplicateService(String),
    #[error("服务 {service_id} 依赖不存在的对象 {dependency_id}")]
    UnknownDependency {
        service_id: String,
        dependency_id: String,
    },
    #[error("服务依赖存在循环：{0}")]
    DependencyCycle(String),
    #[error("应用设置无效：{0}")]
    InvalidSettings(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_script_service() {
        let service = ServiceDefinition::windows_script("api", "D:\\work\\api", "npm run dev");
        assert!(
            AppConfig {
                services: vec![service],
                ..AppConfig::default()
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn rejects_duplicate_service_ids() {
        let service = ServiceDefinition::windows_script("api", "D:\\work\\api", "npm run dev");
        let result = AppConfig {
            services: vec![service.clone(), service],
            ..AppConfig::default()
        }
        .validate();
        assert!(matches!(result, Err(ConfigError::DuplicateService(_))));
    }

    #[test]
    fn rejects_dependency_cycles() {
        let mut first = ServiceDefinition::windows_script("api", "D:\\work\\api", "npm run dev");
        let mut second = ServiceDefinition::windows_script("web", "D:\\work\\web", "npm run dev");
        first.depends_on = vec![ServiceDependency::Running {
            service_id: second.id.clone(),
        }];
        second.depends_on = vec![ServiceDependency::Healthy {
            service_id: first.id.clone(),
        }];
        let result = AppConfig {
            services: vec![first, second],
            ..AppConfig::default()
        }
        .validate();
        assert!(matches!(result, Err(ConfigError::DependencyCycle(_))));
    }
}
