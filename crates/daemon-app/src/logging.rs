use server_on_core::paths::AppPaths;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tracing_subscriber::fmt::MakeWriter;

const MAX_FILE_BYTES: u64 = 2 * 1024 * 1024;
const MAX_FILES: usize = 3;

pub fn writer(paths: &AppPaths) -> io::Result<DaemonLogMakeWriter> {
    std::fs::create_dir_all(&paths.logs_dir)?;
    Ok(DaemonLogMakeWriter {
        state: Arc::new(Mutex::new(RotatingFile::open(
            paths.logs_dir.join("daemon.log"),
            MAX_FILE_BYTES,
            MAX_FILES,
        )?)),
    })
}

#[derive(Clone)]
pub struct DaemonLogMakeWriter {
    state: Arc<Mutex<RotatingFile>>,
}

pub struct DaemonLogWriter {
    state: Arc<Mutex<RotatingFile>>,
}

impl<'a> MakeWriter<'a> for DaemonLogMakeWriter {
    type Writer = DaemonLogWriter;

    fn make_writer(&'a self) -> Self::Writer {
        DaemonLogWriter {
            state: self.state.clone(),
        }
    }
}

impl Write for DaemonLogWriter {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        let file_result = self
            .state
            .lock()
            .map_err(|_| io::Error::other("daemon 日志锁已中毒"))?
            .append(buffer);
        let _ = std::io::stderr().write_all(buffer);
        file_result.map(|()| buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.state
            .lock()
            .map_err(|_| io::Error::other("daemon 日志锁已中毒"))?
            .flush()
    }
}

struct RotatingFile {
    path: PathBuf,
    file: Option<File>,
    length: u64,
    max_bytes: u64,
    max_files: usize,
}

impl RotatingFile {
    fn open(path: PathBuf, max_bytes: u64, max_files: usize) -> io::Result<Self> {
        let file = open_append(&path)?;
        let length = file.metadata()?.len();
        Ok(Self {
            path,
            file: Some(file),
            length,
            max_bytes,
            max_files,
        })
    }

    fn append(&mut self, buffer: &[u8]) -> io::Result<()> {
        if self.length > 0 && self.length.saturating_add(buffer.len() as u64) > self.max_bytes {
            self.rotate()?;
        }
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| io::Error::other("daemon 日志文件未打开"))?;
        file.write_all(buffer)?;
        self.length = self.length.saturating_add(buffer.len() as u64);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut file) = self.file.take() {
            file.flush()?;
        }
        let result = self.rotate_closed_files();
        let file = open_append(&self.path)?;
        self.length = file.metadata()?.len();
        self.file = Some(file);
        result
    }

    fn rotate_closed_files(&self) -> io::Result<()> {
        if self.max_files <= 1 {
            return Ok(());
        }
        let oldest = rotated_path(&self.path, self.max_files - 1);
        if oldest.exists() {
            std::fs::remove_file(oldest)?;
        }
        for index in (1..self.max_files - 1).rev() {
            let source = rotated_path(&self.path, index);
            if source.exists() {
                std::fs::rename(&source, rotated_path(&self.path, index + 1))?;
            }
        }
        if self.path.exists() {
            std::fs::rename(&self.path, rotated_path(&self.path, 1))?;
        }
        Ok(())
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn rotated_path(path: &Path, index: usize) -> PathBuf {
    path.with_extension(format!("{index}.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_bounds_file_count_and_preserves_recent_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("daemon.log");
        let mut log = RotatingFile::open(path.clone(), 16, 3).unwrap();
        for value in ["first-old", "second-old", "third-new", "fourth-new"] {
            log.append(format!("{value}\n").as_bytes()).unwrap();
        }
        log.flush().unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fourth-new\n");
        assert!(rotated_path(&path, 1).exists());
        assert!(rotated_path(&path, 2).exists());
        assert!(!rotated_path(&path, 3).exists());
        let retained = std::fs::read_to_string(rotated_path(&path, 1)).unwrap()
            + &std::fs::read_to_string(rotated_path(&path, 2)).unwrap();
        assert!(retained.contains("third-new"));
    }
}
