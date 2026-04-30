use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result};

use crate::core::config::Config;

const MAX_MCP_LOG_BYTES: u64 = 10 * 1024 * 1024;

static LOG_INIT: OnceLock<std::result::Result<(), String>> = OnceLock::new();

pub(super) fn init_stdio_log_sink(config: &Config) -> Result<()> {
    match LOG_INIT.get_or_init(|| init_stdio_log_sink_inner(config).map_err(|e| format!("{e:#}"))) {
        Ok(()) => Ok(()),
        Err(error) => anyhow::bail!(error.clone()),
    }
}

fn init_stdio_log_sink_inner(config: &Config) -> Result<()> {
    let log_path = expand_home_path(&config.mcp.log_path);
    if let Some(parent) = log_path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    prepare_log_file(&log_path)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("failed to open {}", log_path.display()))?;
    let writer = SharedFileWriter::new(file);
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .try_init()
        .map_err(|error| anyhow::anyhow!("failed to initialize MCP tracing subscriber: {error}"))?;
    tracing::info!("mcp stdio log path: {}", log_path.display());
    Ok(())
}

fn prepare_log_file(path: &Path) -> Result<()> {
    let size = fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
    if size <= MAX_MCP_LOG_BYTES {
        return Ok(());
    }
    OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(path)
        .with_context(|| format!("failed to truncate {}", path.display()))?;
    Ok(())
}

fn expand_home_path(path: &str) -> PathBuf {
    crate::core::utils::expand_home(path)
}

#[derive(Clone)]
struct SharedFileWriter {
    file: Arc<Mutex<File>>,
}

impl SharedFileWriter {
    fn new(file: File) -> Self {
        Self {
            file: Arc::new(Mutex::new(file)),
        }
    }
}

impl Write for SharedFileWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut guard = self
            .file
            .lock()
            .map_err(|_| io::Error::other("mcp log file mutex poisoned"))?;
        guard.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut guard = self
            .file
            .lock()
            .map_err(|_| io::Error::other("mcp log file mutex poisoned"))?;
        guard.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_MCP_LOG_BYTES, prepare_log_file};
    use std::fs;

    #[test]
    fn prepare_log_file_truncates_oversized_log() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mcp.log");
        fs::write(&path, vec![b'x'; (MAX_MCP_LOG_BYTES as usize) + 128]).expect("write log");

        prepare_log_file(&path).expect("prepare");

        let size = fs::metadata(&path).expect("metadata").len();
        assert_eq!(size, 0);
    }
}
