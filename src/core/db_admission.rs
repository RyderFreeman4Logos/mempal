//! Cross-process admission for SQLite holders in one mempal profile.
//!
//! SQLite page-cache limits are configured per connection. They do not compose
//! when daemon, MCP, REST, CLI, and hook processes each open independent pools.
//! This module places a small file contract beside the database and serializes
//! registration with `flock`, so budget admission happens before an expensive
//! SQLite holder is created. A process-birth identity makes stale PID records
//! reclaimable without treating PID reuse as ownership.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const DEFAULT_MAX_HOLDERS: usize = 16;
const DEFAULT_MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const ADMISSION_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const ADMISSION_LOCK_RETRY: Duration = Duration::from_millis(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DbHolderClass {
    Daemon,
    Mcp,
    Api,
    Cli,
    Hook,
    Unknown,
}

impl fmt::Display for DbHolderClass {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::Daemon => "daemon",
            Self::Mcp => "mcp",
            Self::Api => "api",
            Self::Cli => "cli",
            Self::Hook => "hook",
            Self::Unknown => "unknown",
        };
        formatter.write_str(value)
    }
}

impl DbHolderClass {
    /// Infer the privacy-safe surface class without retaining command arguments.
    pub fn current_process() -> Self {
        let mut args = std::env::args().skip(1).take(3);
        match args.find(|arg| matches!(arg.as_str(), "daemon" | "serve" | "hook")) {
            Some(command) if command == "daemon" => Self::Daemon,
            Some(command) if command == "serve" => Self::Mcp,
            Some(command) if command == "hook" => Self::Hook,
            _ => Self::Cli,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbAdmissionConfig {
    pub max_holders: usize,
    pub max_cache_bytes: u64,
}

impl DbAdmissionConfig {
    pub const fn new(max_holders: usize, max_cache_bytes: u64) -> Self {
        Self {
            max_holders,
            max_cache_bytes,
        }
    }
}

impl Default for DbAdmissionConfig {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_HOLDERS, DEFAULT_MAX_CACHE_BYTES)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbAdmissionRequest {
    pub holder_class: DbHolderClass,
    pub connection_count: usize,
    pub configured_cache_bytes: u64,
}

impl DbAdmissionRequest {
    pub const fn new(
        holder_class: DbHolderClass,
        connection_count: usize,
        configured_cache_bytes: u64,
    ) -> Self {
        Self {
            holder_class,
            connection_count,
            configured_cache_bytes,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DbAdmissionError {
    #[error("invalid database admission request: {0}")]
    InvalidRequest(&'static str),
    #[error("database admission state is busy after {timeout_ms}ms: {path}")]
    Busy { path: PathBuf, timeout_ms: u64 },
    #[error(
        "profile database holder budget exceeded: active_holders={active_holders}/{max_holders}, active_cache_bytes={active_cache_bytes}/{max_cache_bytes}, requested_cache_bytes={requested_cache_bytes}"
    )]
    BudgetExceeded {
        active_holders: usize,
        max_holders: usize,
        active_cache_bytes: u64,
        max_cache_bytes: u64,
        requested_cache_bytes: u64,
    },
    #[error("database admission I/O failed for {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("database admission state is invalid at {path}: {source}")]
    InvalidState {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbAdmissionHolder {
    pub holder_class: DbHolderClass,
    pub owner_identity: String,
    pub pid: u32,
    pub generation: u64,
    pub acquired_at_unix_secs: u64,
    pub connection_count: usize,
    pub configured_cache_bytes: u64,
    token: String,
    process_identity: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbAdmissionSnapshot {
    pub holders: Vec<DbAdmissionHolder>,
    pub active_holders: usize,
    pub configured_holder_limit: usize,
    pub configured_cache_bytes: u64,
    pub active_cache_bytes: u64,
    pub available_cache_bytes: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct AdmissionState {
    next_generation: u64,
    holders: Vec<DbAdmissionHolder>,
}

#[derive(Debug)]
pub struct ProfileDbAdmission {
    state_path: PathBuf,
    lock_path: PathBuf,
    owner_identity: String,
    token: String,
    generation: u64,
}

impl ProfileDbAdmission {
    pub fn acquire(db_path: &Path, request: DbAdmissionRequest) -> Result<Self, DbAdmissionError> {
        Self::acquire_with_config(db_path, request, DbAdmissionConfig::default())
    }

    pub fn acquire_with_config(
        db_path: &Path,
        request: DbAdmissionRequest,
        config: DbAdmissionConfig,
    ) -> Result<Self, DbAdmissionError> {
        validate_request(request, config)?;
        let paths = AdmissionPaths::new(db_path)?;
        let _lock = lock_state(&paths.lock_path)?;
        let mut state = load_state(&paths.state_path)?;
        state.holders.retain(holder_is_live);

        let active_cache_bytes = state.holders.iter().fold(0_u64, |total, holder| {
            total.saturating_add(holder.configured_cache_bytes)
        });
        if state.holders.len() >= config.max_holders
            || active_cache_bytes.saturating_add(request.configured_cache_bytes)
                > config.max_cache_bytes
        {
            return Err(DbAdmissionError::BudgetExceeded {
                active_holders: state.holders.len(),
                max_holders: config.max_holders,
                active_cache_bytes,
                max_cache_bytes: config.max_cache_bytes,
                requested_cache_bytes: request.configured_cache_bytes,
            });
        }

        state.next_generation = state.next_generation.saturating_add(1).max(1);
        let generation = state.next_generation;
        let pid = std::process::id();
        let process_identity = super::process_identity::current_process_identity().to_string();
        let owner_identity = format!("mempal-{}-{pid}-{process_identity}", request.holder_class);
        let acquired_at_unix_secs = unix_secs_now();
        let token = admission_token(&owner_identity, generation, acquired_at_unix_secs);
        state.holders.push(DbAdmissionHolder {
            holder_class: request.holder_class,
            owner_identity: owner_identity.clone(),
            pid,
            generation,
            acquired_at_unix_secs,
            connection_count: request.connection_count,
            configured_cache_bytes: request.configured_cache_bytes,
            token: token.clone(),
            process_identity,
        });
        state
            .holders
            .sort_by_key(|holder| (holder.generation, holder.pid));
        save_state(&paths.state_path, &state)?;

        Ok(Self {
            state_path: paths.state_path,
            lock_path: paths.lock_path,
            owner_identity,
            token,
            generation,
        })
    }

    pub fn snapshot(db_path: &Path) -> Result<DbAdmissionSnapshot, DbAdmissionError> {
        Self::snapshot_with_config(db_path, DbAdmissionConfig::default())
    }

    pub fn snapshot_with_config(
        db_path: &Path,
        config: DbAdmissionConfig,
    ) -> Result<DbAdmissionSnapshot, DbAdmissionError> {
        let paths = AdmissionPaths::new(db_path)?;
        let _lock = lock_state(&paths.lock_path)?;
        let mut state = load_state(&paths.state_path)?;
        let previous_len = state.holders.len();
        state.holders.retain(holder_is_live);
        if state.holders.len() != previous_len {
            save_state(&paths.state_path, &state)?;
        }
        let active_cache_bytes = state.holders.iter().fold(0_u64, |total, holder| {
            total.saturating_add(holder.configured_cache_bytes)
        });
        Ok(DbAdmissionSnapshot {
            active_holders: state.holders.len(),
            holders: state.holders,
            configured_holder_limit: config.max_holders,
            configured_cache_bytes: config.max_cache_bytes,
            active_cache_bytes,
            available_cache_bytes: config.max_cache_bytes.saturating_sub(active_cache_bytes),
        })
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn owner_identity(&self) -> &str {
        &self.owner_identity
    }

    fn release(&self) -> Result<bool, DbAdmissionError> {
        let _lock = lock_state(&self.lock_path)?;
        let mut state = load_state(&self.state_path)?;
        let before = state.holders.len();
        state.holders.retain(|holder| {
            holder.token != self.token
                || holder.generation != self.generation
                || holder.owner_identity != self.owner_identity
        });
        let released = state.holders.len() != before;
        if released {
            save_state(&self.state_path, &state)?;
        }
        Ok(released)
    }
}

impl Drop for ProfileDbAdmission {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

fn validate_request(
    request: DbAdmissionRequest,
    config: DbAdmissionConfig,
) -> Result<(), DbAdmissionError> {
    if request.connection_count == 0 {
        return Err(DbAdmissionError::InvalidRequest(
            "connection_count must be positive",
        ));
    }
    if request.configured_cache_bytes == 0 {
        return Err(DbAdmissionError::InvalidRequest(
            "configured_cache_bytes must be positive",
        ));
    }
    if config.max_holders == 0 || config.max_cache_bytes == 0 {
        return Err(DbAdmissionError::InvalidRequest(
            "profile limits must be positive",
        ));
    }
    Ok(())
}

struct AdmissionPaths {
    state_path: PathBuf,
    lock_path: PathBuf,
}

impl AdmissionPaths {
    fn new(db_path: &Path) -> Result<Self, DbAdmissionError> {
        let parent = db_path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|source| DbAdmissionError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
        let file_name = db_path
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .ok_or(DbAdmissionError::InvalidRequest(
                "database path must have a UTF-8 file name",
            ))?;
        Ok(Self {
            state_path: parent.join(format!(".{file_name}.admission.json")),
            lock_path: parent.join(format!(".{file_name}.admission.lock")),
        })
    }
}

fn lock_state(path: &Path) -> Result<File, DbAdmissionError> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|source| DbAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    let started = std::time::Instant::now();
    loop {
        match imp::try_lock_exclusive(&file) {
            Ok(true) => return Ok(file),
            Ok(false) if started.elapsed() < ADMISSION_LOCK_TIMEOUT => {
                std::thread::sleep(ADMISSION_LOCK_RETRY);
            }
            Ok(false) => {
                return Err(DbAdmissionError::Busy {
                    path: path.to_path_buf(),
                    timeout_ms: ADMISSION_LOCK_TIMEOUT.as_millis() as u64,
                });
            }
            Err(source) => {
                return Err(DbAdmissionError::Io {
                    path: path.to_path_buf(),
                    source,
                });
            }
        }
    }
}

fn load_state(path: &Path) -> Result<AdmissionState, DbAdmissionError> {
    match fs::read(path) {
        Ok(bytes) if bytes.is_empty() => Ok(AdmissionState::default()),
        Ok(bytes) => {
            serde_json::from_slice(&bytes).map_err(|source| DbAdmissionError::InvalidState {
                path: path.to_path_buf(),
                source,
            })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(AdmissionState::default()),
        Err(source) => Err(DbAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn save_state(path: &Path, state: &AdmissionState) -> Result<(), DbAdmissionError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut staged =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| DbAdmissionError::Io {
            path: parent.to_path_buf(),
            source,
        })?;
    serde_json::to_writer(staged.as_file_mut(), state).map_err(|source| {
        DbAdmissionError::InvalidState {
            path: path.to_path_buf(),
            source,
        }
    })?;
    staged
        .as_file()
        .sync_all()
        .map_err(|source| DbAdmissionError::Io {
            path: path.to_path_buf(),
            source,
        })?;
    staged.persist(path).map_err(|error| DbAdmissionError::Io {
        path: path.to_path_buf(),
        source: error.error,
    })?;
    Ok(())
}

fn holder_is_live(holder: &DbAdmissionHolder) -> bool {
    #[cfg(target_os = "linux")]
    {
        super::process_identity::process_identity_matches(holder.pid, &holder.process_identity)
    }
    #[cfg(not(target_os = "linux"))]
    {
        holder.pid == std::process::id()
            && holder.process_identity == super::process_identity::current_process_identity()
    }
}

fn admission_token(owner_identity: &str, generation: u64, acquired_at_unix_secs: u64) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(owner_identity.as_bytes());
    hasher.update(b"\0");
    hasher.update(generation.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(acquired_at_unix_secs.to_string().as_bytes());
    hasher.finalize().to_hex()[..24].to_string()
}

fn unix_secs_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

#[cfg(unix)]
mod imp {
    use std::fs::File;
    use std::io;
    use std::os::fd::AsRawFd;

    const LOCK_EX: i32 = 2;
    const LOCK_NB: i32 = 4;

    pub fn try_lock_exclusive(file: &File) -> Result<bool, io::Error> {
        let fd = file.as_raw_fd();
        // SAFETY: `fd` belongs to the live `file` for this call. `flock` does
        // not retain the descriptor or access Rust memory.
        let result = unsafe { libc::flock(fd, LOCK_EX | LOCK_NB) };
        if result == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            Some(code) if code == libc::EAGAIN || code == libc::EWOULDBLOCK => Ok(false),
            _ => Err(error),
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::fs::File;
    use std::io;

    pub fn try_lock_exclusive(_file: &File) -> Result<bool, io::Error> {
        Ok(true)
    }
}
