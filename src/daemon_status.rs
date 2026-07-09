use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::core::config::Config;

pub const DAEMON_EMBEDDER_STATUS_FILE: &str = "daemon-embedder-status.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonEmbedderRuntimeStatus {
    pub pid: u32,
    pub updated_at_unix_secs: u64,
    pub cache_loaded: bool,
    pub mode: String,
    pub backend: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimensions: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback: Option<String>,
    pub source: String,
}

impl DaemonEmbedderRuntimeStatus {
    pub fn unloaded_from_config(config: &Config, source: impl Into<String>) -> Self {
        let daemon_config = config.daemon_embedder_config();
        let model = daemon_config.embed.effective_model_summary();
        Self {
            pid: std::process::id(),
            updated_at_unix_secs: unix_secs(),
            cache_loaded: false,
            mode: config.daemon.embedder_mode.as_str().to_string(),
            backend: daemon_config.embed.backend,
            model,
            dimensions: None,
            fallback: daemon_config.embed.fallback,
            source: source.into(),
        }
    }

    pub fn loaded_from_config(
        config: &Config,
        dimensions: usize,
        fallback: Option<String>,
        source: impl Into<String>,
    ) -> Self {
        let daemon_config = config.daemon_embedder_config();
        let model = daemon_config.embed.effective_model_summary();
        Self {
            pid: std::process::id(),
            updated_at_unix_secs: unix_secs(),
            cache_loaded: true,
            mode: config.daemon.embedder_mode.as_str().to_string(),
            backend: daemon_config.embed.backend,
            model,
            dimensions: Some(dimensions),
            fallback,
            source: source.into(),
        }
    }
}

pub fn embedder_status_path(mempal_home: &Path) -> PathBuf {
    mempal_home.join(DAEMON_EMBEDDER_STATUS_FILE)
}

pub fn write_embedder_status_atomic(
    mempal_home: &Path,
    status: &DaemonEmbedderRuntimeStatus,
) -> io::Result<()> {
    fs::create_dir_all(mempal_home)?;
    let path = embedder_status_path(mempal_home);
    let tmp_path = mempal_home.join(format!(
        "{DAEMON_EMBEDDER_STATUS_FILE}.tmp.{}",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&tmp_path)?;
    serde_json::to_writer_pretty(&mut file, status).map_err(io::Error::other)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    fs::rename(tmp_path, path)
}

pub fn read_embedder_status(mempal_home: &Path) -> io::Result<Option<DaemonEmbedderRuntimeStatus>> {
    let path = embedder_status_path(mempal_home);
    let bytes = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let status =
        serde_json::from_slice::<DaemonEmbedderRuntimeStatus>(&bytes).map_err(io::Error::other)?;
    Ok(process_is_running(status.pid).then_some(status))
}

#[cfg(unix)]
fn process_is_running(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    if pid <= 0 {
        return false;
    }
    // SAFETY: kill(pid, 0) checks process existence and permissions without
    // delivering a signal or dereferencing Rust memory.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

#[cfg(not(unix))]
fn process_is_running(pid: u32) -> bool {
    pid == std::process::id()
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_daemon_embedder_status_round_trips_without_raw_content() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let status = DaemonEmbedderRuntimeStatus {
            pid: std::process::id(),
            updated_at_unix_secs: 123,
            cache_loaded: true,
            mode: "remote".to_string(),
            backend: "openai_compat".to_string(),
            model: Some("legacy=Qwen/Qwen3-Embedding-8B".to_string()),
            dimensions: Some(4096),
            fallback: None,
            source: "daemon-rest".to_string(),
        };

        write_embedder_status_atomic(tmp.path(), &status).expect("write status");
        let read = read_embedder_status(tmp.path())
            .expect("read status")
            .expect("status exists");

        assert_eq!(read, status);
        let raw = std::fs::read_to_string(embedder_status_path(tmp.path())).expect("raw status");
        assert!(!raw.contains("Bearer"));
        assert!(!raw.contains("prompt"));
    }

    #[test]
    fn test_dead_daemon_embedder_status_is_ignored() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let status = DaemonEmbedderRuntimeStatus {
            pid: u32::MAX,
            updated_at_unix_secs: 123,
            cache_loaded: true,
            mode: "remote".to_string(),
            backend: "openai_compat".to_string(),
            model: Some("legacy=Qwen/Qwen3-Embedding-8B".to_string()),
            dimensions: Some(4096),
            fallback: None,
            source: "daemon-rest".to_string(),
        };

        write_embedder_status_atomic(tmp.path(), &status).expect("write stale status");

        assert_eq!(
            read_embedder_status(tmp.path()).expect("read stale status"),
            None
        );
    }
}
