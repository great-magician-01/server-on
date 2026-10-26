use anyhow::{Context, Result, anyhow};
use chrono::Utc;
use server_on_core::config::AppConfig;
use server_on_core::event::LogEvent;
use server_on_core::paths::{AppPaths, PersistedState};
use sha2::{Digest, Sha256};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;

const MAX_CONFIG_BACKUPS: usize = 10;

#[derive(Clone)]
pub struct ConfigStore {
    paths: AppPaths,
}

impl ConfigStore {
    pub fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn load(&self) -> Result<AppConfig> {
        self.ensure_directories()?;
        if !self.paths.config_file.exists() {
            let config = AppConfig::default();
            self.save(&config)?;
            return Ok(config);
        }

        match self.read_config(&self.paths.config_file) {
            Ok(config) => Ok(config),
            Err(primary_error) => {
                let mut backups = self.backup_files()?;
                backups.sort_by(|a, b| b.cmp(a));
                for backup in backups {
                    match self.read_config(&backup) {
                        Ok(config) => {
                            tracing::warn!(
                                backup = %backup.display(),
                                error = %primary_error,
                                "配置损坏，已从备份恢复"
                            );
                            self.save(&config)?;
                            return Ok(config);
                        }
                        Err(error) => {
                            tracing::warn!(backup = %backup.display(), %error, "配置备份不可用");
                        }
                    }
                }
                Err(primary_error)
            }
        }
    }

    pub fn save(&self, config: &AppConfig) -> Result<()> {
        config.validate()?;
        self.ensure_directories()?;
        let bytes = serde_json::to_vec_pretty(config)?;
        atomic_write(&self.paths.config_file, &bytes)?;
        let backup = self.paths.backups_dir.join(format!(
            "config-{}-{}.json",
            Utc::now().format("%Y%m%dT%H%M%S%.3fZ"),
            Uuid::new_v4().simple()
        ));
        atomic_write(&backup, &bytes)?;
        self.prune_backups()?;
        Ok(())
    }

    fn read_config(&self, path: &Path) -> Result<AppConfig> {
        let bytes = fs::read(path).with_context(|| format!("无法读取 {}", path.display()))?;
        let config: AppConfig = serde_json::from_slice(&bytes)
            .with_context(|| format!("配置 JSON 无效：{}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    fn ensure_directories(&self) -> Result<()> {
        for path in [
            &self.paths.root,
            &self.paths.logs_dir,
            &self.paths.backups_dir,
        ] {
            fs::create_dir_all(path).with_context(|| format!("无法创建目录 {}", path.display()))?;
        }
        Ok(())
    }

    fn backup_files(&self) -> Result<Vec<PathBuf>> {
        if !self.paths.backups_dir.exists() {
            return Ok(Vec::new());
        }
        let mut paths = Vec::new();
        for entry in fs::read_dir(&self.paths.backups_dir)? {
            let path = entry?.path();
            if path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                paths.push(path);
            }
        }
        Ok(paths)
    }

    fn prune_backups(&self) -> Result<()> {
        let mut backups = self.backup_files()?;
        backups.sort_by(|a, b| b.cmp(a));
        for path in backups.into_iter().skip(MAX_CONFIG_BACKUPS) {
            fs::remove_file(path)?;
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(paths: &AppPaths) -> Self {
        Self {
            path: paths.state_file.clone(),
        }
    }

    pub fn load(&self) -> Result<PersistedState> {
        if !self.path.exists() {
            return Ok(PersistedState::default());
        }
        let bytes = fs::read(&self.path)?;
        serde_json::from_slice(&bytes).context("无法解析运行状态")
    }

    pub fn save(&self, state: &PersistedState) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        atomic_write(&self.path, &serde_json::to_vec_pretty(state)?)
    }
}

pub struct LogStore {
    root: PathBuf,
    max_file_bytes: u64,
    max_files: u32,
    retention_days: u32,
}

impl LogStore {
    pub fn new(root: PathBuf, max_file_bytes: u64, max_files: u32, retention_days: u32) -> Self {
        Self {
            root,
            max_file_bytes: max_file_bytes.max(1),
            max_files: max_files.max(1),
            retention_days,
        }
    }

    pub fn append(&self, event: &LogEvent) -> Result<()> {
        let directory = self.service_directory(&event.service_id);
        fs::create_dir_all(&directory)?;
        let path = directory.join("current.jsonl");
        let mut bytes = serde_json::to_vec(event)?;
        bytes.push(b'\n');
        let current_size = fs::metadata(&path)
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        if current_size > 0 && current_size + bytes.len() as u64 > self.max_file_bytes {
            self.rotate(&directory)?;
        }
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        file.write_all(&bytes)?;
        Ok(())
    }

    pub fn recent(
        &self,
        service_id: &server_on_core::config::ServiceId,
        limit: usize,
    ) -> Result<Vec<LogEvent>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let directory = self.service_directory(service_id);
        let mut files = Vec::new();
        for index in (1..self.max_files).rev() {
            let path = directory.join(format!("{index}.jsonl"));
            if path.exists() {
                files.push(path);
            }
        }
        let current = directory.join("current.jsonl");
        if current.exists() {
            files.push(current);
        }

        let mut events = VecDeque::with_capacity(limit);
        for path in files {
            let mut contents = String::new();
            File::open(path)?.read_to_string(&mut contents)?;
            for line in contents.lines() {
                if let Ok(event) = serde_json::from_str::<LogEvent>(line) {
                    if events.len() == limit {
                        events.pop_front();
                    }
                    events.push_back(event);
                }
            }
        }
        Ok(events.into_iter().collect())
    }

    pub fn cleanup_expired(&self) -> Result<()> {
        if !self.root.exists() {
            return Ok(());
        }
        let cutoff = Utc::now() - chrono::Duration::days(i64::from(self.retention_days));
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            for file in fs::read_dir(entry.path())? {
                let file = file?;
                let modified = file.metadata()?.modified()?;
                if chrono::DateTime::<Utc>::from(modified) < cutoff {
                    fs::remove_file(file.path())?;
                }
            }
        }
        Ok(())
    }

    fn service_directory(&self, service_id: &server_on_core::config::ServiceId) -> PathBuf {
        let digest = Sha256::digest(service_id.as_str().as_bytes());
        self.root.join(format!("{digest:x}"))
    }

    fn rotate(&self, directory: &Path) -> Result<()> {
        if self.max_files <= 1 {
            fs::remove_file(directory.join("current.jsonl"))?;
            return Ok(());
        }
        let oldest = directory.join(format!("{}.jsonl", self.max_files - 1));
        if oldest.exists() {
            fs::remove_file(oldest)?;
        }
        for index in (1..self.max_files - 1).rev() {
            let source = directory.join(format!("{index}.jsonl"));
            if source.exists() {
                fs::rename(source, directory.join(format!("{}.jsonl", index + 1)))?;
            }
        }
        fs::rename(directory.join("current.jsonl"), directory.join("1.jsonl"))?;
        Ok(())
    }
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("文件路径没有父目录：{}", path.display()))?;
    fs::create_dir_all(parent)?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("文件路径无效：{}", path.display()))?;
    let temporary = parent.join(format!(".{file_name}.{}.tmp", Uuid::new_v4().simple()));
    let result = (|| -> Result<()> {
        let mut file = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        replace_file(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    fs::rename(source, destination)?;
    Ok(())
}

#[cfg(windows)]
fn replace_file(source: &Path, destination: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let source_wide: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let destination_wide: Vec<u16> = destination
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    unsafe {
        MoveFileExW(
            PCWSTR(source_wide.as_ptr()),
            PCWSTR(destination_wide.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use server_on_core::config::{ServiceDefinition, ServiceId};
    use server_on_core::event::LogStream;

    #[test]
    fn config_round_trip_and_backup_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigStore::new(AppPaths::from_root(directory.path().to_path_buf()));
        let config = AppConfig {
            services: vec![ServiceDefinition::windows_script(
                "api",
                "D:\\work\\api",
                "npm run dev",
            )],
            ..AppConfig::default()
        };
        store.save(&config).unwrap();
        assert_eq!(store.load().unwrap(), config);
        fs::write(&store.paths.config_file, b"not json").unwrap();
        assert_eq!(store.load().unwrap(), config);
    }

    #[test]
    fn logs_rotate_and_return_recent_events() {
        let directory = tempfile::tempdir().unwrap();
        let store = LogStore::new(directory.path().to_path_buf(), 600, 3, 7);
        let service_id = ServiceId::new();
        for index in 0..12 {
            store
                .append(&LogEvent {
                    service_id: service_id.clone(),
                    timestamp: Utc::now(),
                    stream: LogStream::Stdout,
                    message: format!("message-{index:02}-padding-padding"),
                })
                .unwrap();
        }
        let events = store.recent(&service_id, 5).unwrap();
        assert_eq!(events.len(), 5);
        assert_eq!(events.last().unwrap().message, "message-11-padding-padding");
    }
}
