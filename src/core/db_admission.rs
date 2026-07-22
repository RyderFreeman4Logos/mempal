//! Cross-process admission for SQLite holders in one mempal profile.
//!
//! SQLite page-cache limits are configured per connection. They do not compose
//! when daemon, MCP, REST, CLI, and hook processes each open independent pools.
//! This module places a small file contract beside the database and serializes
//! registration with `flock`, so budget admission happens before an expensive
//! SQLite holder is created. Each registration owns a kernel-backed lease for
//! namespace-independent crash recovery; process birth metadata remains for
//! legacy fail-closed diagnostics without treating PID reuse as ownership.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use super::db_admission_budget as budget;
#[cfg(test)]
use super::db_admission_fault_injection::{self as fault_injection, CrashPoint};
use super::db_admission_lease::{
    HolderLiveness, create_holder_lease, holder_lease_liveness, remove_holder_lease,
    sweep_unreferenced_holder_leases,
};

pub(super) use super::db_admission_paths::AdmissionPaths;
pub use budget::BudgetExceededReason;

const DEFAULT_MAX_HOLDERS: usize = 16;
/// Slots reserved for long-lived service holders (daemon + MCP). Transient
/// CLI/hook/api processes cannot fill the last reserved seats, so an active
/// MCP/daemon process can still open its async pool under load (#809).
const DEFAULT_RESERVED_SERVICE_HOLDERS: usize = 2;
const DEFAULT_MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;
const ADMISSION_LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const ADMISSION_LOCK_RETRY: Duration = Duration::from_millis(2);
pub(super) const ADMISSION_RELEASE_MAX_ATTEMPTS: u8 = 3;
pub(super) const ADMISSION_RELEASE_RETRY_DELAY: Duration = Duration::from_millis(50);
pub(super) const HOLDER_LEASE_VERSION: u8 = 1;

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
        // Use args_os() to avoid panicking on non-UTF-8 argv, which is
        // valid on Unix (e.g., non-UTF-8 file paths as CLI arguments).
        let mut args = std::env::args_os().skip(1).take(3);
        match args.find(|arg| {
            arg.to_str()
                .is_some_and(|s| matches!(s, "daemon" | "serve" | "hook"))
        }) {
            Some(command) if command.to_str() == Some("daemon") => Self::Daemon,
            Some(command) if command.to_str() == Some("serve") => Self::Mcp,
            Some(command) if command.to_str() == Some("hook") => Self::Hook,
            _ => Self::Cli,
        }
    }

    /// Long-lived service surfaces that may consume reserved holder seats.
    pub const fn is_service_holder(self) -> bool {
        matches!(self, Self::Daemon | Self::Mcp)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DbAdmissionConfig {
    pub max_holders: usize,
    pub max_cache_bytes: u64,
    /// Seats reserved for [`DbHolderClass::is_service_holder`] processes.
    pub reserved_service_holders: usize,
}

impl DbAdmissionConfig {
    pub const fn new(max_holders: usize, max_cache_bytes: u64) -> Self {
        let reserved_service_holders = if DEFAULT_RESERVED_SERVICE_HOLDERS < max_holders {
            DEFAULT_RESERVED_SERVICE_HOLDERS
        } else {
            max_holders
        };
        Self {
            max_holders,
            max_cache_bytes,
            reserved_service_holders,
        }
    }

    pub const fn with_reserved_service_holders(mut self, reserved_service_holders: usize) -> Self {
        self.reserved_service_holders = if reserved_service_holders < self.max_holders {
            reserved_service_holders
        } else {
            self.max_holders
        };
        self
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
        "profile database holder budget exceeded: active_holders={active_holders}/{max_holders}, active_cache_bytes={active_cache_bytes}/{max_cache_bytes}, requested_cache_bytes={requested_cache_bytes}, reaped_stale_holders={reaped_stale_holders}, reserved_service_holders={reserved_service_holders}, service_holders={service_holders}, reason={reason}"
    )]
    BudgetExceeded {
        active_holders: usize,
        max_holders: usize,
        active_cache_bytes: u64,
        max_cache_bytes: u64,
        requested_cache_bytes: u64,
        reaped_stale_holders: usize,
        reserved_service_holders: usize,
        service_holders: usize,
        reason: BudgetExceededReason,
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
    pub(super) token: String,
    process_identity: String,
    #[serde(default)]
    pid_namespace: Option<String>,
    #[serde(default)]
    pub(super) lease_version: u8,
}

/// Why a holder remains fail-closed when its liveness cannot be established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnknownHolderReason {
    UnknownLeaseVersion,
    LeaseOpenUnavailable,
    LeaseLockUnavailable,
    LegacyProcessIdentityUnverifiable,
}

/// Privacy-safe liveness evidence for an unreaped holder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownHolderDiagnostic {
    pub generation: u64,
    pub reason: UnknownHolderReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbAdmissionSnapshot {
    pub holders: Vec<DbAdmissionHolder>,
    pub active_holders: usize,
    pub reaped_stale_holders_this_snapshot: usize,
    pub unknown_holders: usize,
    pub unknown_holder_generations: Vec<u64>,
    pub unknown_holder_diagnostics: Vec<UnknownHolderDiagnostic>,
    pub configured_holder_limit: usize,
    pub reserved_service_holders: usize,
    pub service_holders: usize,
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
    database_path: PathBuf,
    state_path: PathBuf,
    lock_path: PathBuf,
    owner_identity: String,
    token: String,
    generation: u64,
    lease_path: PathBuf,
    _lease: File,
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
        sweep_unreferenced_holder_leases(&paths, &state.holders)?;
        let reaped = reap_stale_holders(&paths, &mut state);
        persist_reaped_holders(&paths, &state, &reaped)?;

        let active_cache_bytes = state.holders.iter().fold(0_u64, |total, holder| {
            total.saturating_add(holder.configured_cache_bytes)
        });
        let service_holders = state
            .holders
            .iter()
            .filter(|holder| holder.holder_class.is_service_holder())
            .count();
        if let Some(reason) =
            budget::budget_exceeded_reason(state.holders.len(), request, config, active_cache_bytes)
        {
            return Err(DbAdmissionError::BudgetExceeded {
                active_holders: state.holders.len(),
                max_holders: config.max_holders,
                active_cache_bytes,
                max_cache_bytes: config.max_cache_bytes,
                requested_cache_bytes: request.configured_cache_bytes,
                reaped_stale_holders: reaped.reaped_stale_holders_this_snapshot,
                reserved_service_holders: config.reserved_service_holders,
                service_holders,
                reason,
            });
        }

        state.next_generation = state.next_generation.saturating_add(1).max(1);
        let generation = state.next_generation;
        let pid = std::process::id();
        let process_identity = super::process_identity::current_process_identity().to_string();
        let pid_namespace = super::process_identity::current_pid_namespace();
        let owner_identity = format!("mempal-{}-{pid}-{process_identity}", request.holder_class);
        let acquired_at_unix_secs = unix_secs_now();
        let token = admission_token(&owner_identity, generation, acquired_at_unix_secs);
        let lease_path = paths.holder_lease_path(&token);
        let lease = create_holder_lease(&lease_path)?;
        #[cfg(test)]
        fault_injection::exit_if(CrashPoint::LeaseCreatedBeforeStatePublish);
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
            pid_namespace,
            lease_version: HOLDER_LEASE_VERSION,
        });
        state
            .holders
            .sort_by_key(|holder| (holder.generation, holder.pid));
        if let Err(error) = save_state(&paths.state_path, &state) {
            drop(lease);
            if let Err(cleanup_error) = remove_holder_lease(&lease_path) {
                tracing::warn!(%cleanup_error, "failed to clean up unpublished holder lease");
            }
            return Err(error);
        }
        Ok(Self {
            database_path: paths.database_path,
            state_path: paths.state_path,
            lock_path: paths.lock_path,
            owner_identity,
            token,
            generation,
            lease_path,
            _lease: lease,
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
        sweep_unreferenced_holder_leases(&paths, &state.holders)?;
        let reaped = reap_stale_holders(&paths, &mut state);
        persist_reaped_holders(&paths, &state, &reaped)?;
        let active_cache_bytes = state.holders.iter().fold(0_u64, |total, holder| {
            total.saturating_add(holder.configured_cache_bytes)
        });
        let service_holders = state
            .holders
            .iter()
            .filter(|holder| holder.holder_class.is_service_holder())
            .count();
        Ok(DbAdmissionSnapshot {
            active_holders: state.holders.len(),
            holders: state.holders,
            reaped_stale_holders_this_snapshot: reaped.reaped_stale_holders_this_snapshot,
            unknown_holders: reaped.unknown_holder_diagnostics.len(),
            unknown_holder_generations: reaped
                .unknown_holder_diagnostics
                .iter()
                .map(|diagnostic| diagnostic.generation)
                .collect(),
            unknown_holder_diagnostics: reaped.unknown_holder_diagnostics,
            configured_holder_limit: config.max_holders,
            reserved_service_holders: config.reserved_service_holders,
            service_holders,
            configured_cache_bytes: config.max_cache_bytes,
            active_cache_bytes,
            available_cache_bytes: config.max_cache_bytes.saturating_sub(active_cache_bytes),
        })
    }

    pub(crate) fn resolve_database_path(db_path: &Path) -> Result<PathBuf, DbAdmissionError> {
        AdmissionPaths::new(db_path).map(|paths| paths.database_path)
    }

    pub const fn generation(&self) -> u64 {
        self.generation
    }

    pub fn owner_identity(&self) -> &str {
        &self.owner_identity
    }

    /// The resolved database path whose identity this admission accounts for.
    pub fn database_path(&self) -> &Path {
        &self.database_path
    }

    pub(super) fn release(&self) -> Result<bool, DbAdmissionError> {
        let _lock = lock_state(&self.lock_path)?;
        let mut state = load_state(&self.state_path)?;
        let paths = AdmissionPaths {
            database_path: self.database_path.clone(),
            state_path: self.state_path.clone(),
            lock_path: self.lock_path.clone(),
        };
        sweep_unreferenced_holder_leases(&paths, &state.holders)?;
        let before = state.holders.len();
        state.holders.retain(|holder| {
            holder.token != self.token
                || holder.generation != self.generation
                || holder.owner_identity != self.owner_identity
        });
        let released = state.holders.len() != before;
        if released {
            save_state(&self.state_path, &state)?;
            #[cfg(test)]
            fault_injection::exit_if(CrashPoint::ReleaseStateSavedBeforeLeaseUnlink);
        }
        if state
            .holders
            .iter()
            .all(|holder| holder.token != self.token)
        {
            remove_holder_lease(&self.lease_path)?;
        }
        Ok(released)
    }
}

fn validate_request(
    request: DbAdmissionRequest,
    config: DbAdmissionConfig,
) -> Result<(), DbAdmissionError> {
    budget::validate_request(request, config)
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

#[derive(Debug, Default)]
struct ReapedHolders {
    reaped_stale_holders_this_snapshot: usize,
    unknown_holder_diagnostics: Vec<UnknownHolderDiagnostic>,
}

fn reap_stale_holders(paths: &AdmissionPaths, state: &mut AdmissionState) -> ReapedHolders {
    let mut result = ReapedHolders::default();
    state
        .holders
        .retain(|holder| match holder_liveness(paths, holder) {
            HolderLiveness::Live => true,
            HolderLiveness::Unknown(reason) => {
                result
                    .unknown_holder_diagnostics
                    .push(UnknownHolderDiagnostic {
                        generation: holder.generation,
                        reason,
                    });
                true
            }
            HolderLiveness::Dead => {
                result.reaped_stale_holders_this_snapshot =
                    result.reaped_stale_holders_this_snapshot.saturating_add(1);
                false
            }
        });
    result
}

fn persist_reaped_holders(
    paths: &AdmissionPaths,
    state: &AdmissionState,
    reaped: &ReapedHolders,
) -> Result<(), DbAdmissionError> {
    if reaped.reaped_stale_holders_this_snapshot == 0 {
        return Ok(());
    }
    save_state(&paths.state_path, state)?;
    #[cfg(test)]
    fault_injection::exit_if(CrashPoint::ReapStateSavedBeforeOrphanSweep);
    sweep_unreferenced_holder_leases(paths, &state.holders)?;
    Ok(())
}

fn holder_liveness(paths: &AdmissionPaths, holder: &DbAdmissionHolder) -> HolderLiveness {
    match holder.lease_version {
        HOLDER_LEASE_VERSION => holder_lease_liveness(&paths.holder_lease_path(&holder.token)),
        0 if paths.holder_lease_path(&holder.token).exists() => {
            holder_lease_liveness(&paths.holder_lease_path(&holder.token))
        }
        0 => legacy_holder_liveness(holder),
        _ => HolderLiveness::Unknown(UnknownHolderReason::UnknownLeaseVersion),
    }
}

#[cfg(test)]
fn holder_is_live(holder: &DbAdmissionHolder) -> bool {
    !matches!(legacy_holder_liveness(holder), HolderLiveness::Dead)
}

fn legacy_holder_liveness(holder: &DbAdmissionHolder) -> HolderLiveness {
    #[cfg(target_os = "linux")]
    {
        match super::process_identity::process_identity_liveness(
            holder.pid,
            &holder.process_identity,
            holder.pid_namespace.as_deref(),
        ) {
            super::process_identity::ProcessLiveness::Live => HolderLiveness::Live,
            super::process_identity::ProcessLiveness::Dead => HolderLiveness::Dead,
            super::process_identity::ProcessLiveness::Unverifiable => {
                HolderLiveness::Unknown(UnknownHolderReason::LegacyProcessIdentityUnverifiable)
            }
        }
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        if holder.pid == std::process::id() {
            return if holder.process_identity == super::process_identity::current_process_identity()
            {
                HolderLiveness::Live
            } else {
                HolderLiveness::Dead
            };
        }
        match imp::foreign_process_liveness(holder.pid) {
            ProcessLiveness::Dead => HolderLiveness::Dead,
            ProcessLiveness::Unverifiable => {
                HolderLiveness::Unknown(UnknownHolderReason::LegacyProcessIdentityUnverifiable)
            }
        }
    }
    #[cfg(not(unix))]
    {
        if holder.pid == std::process::id() {
            return if holder.process_identity == super::process_identity::current_process_identity()
            {
                HolderLiveness::Live
            } else {
                HolderLiveness::Dead
            };
        }
        HolderLiveness::Unknown(UnknownHolderReason::LegacyProcessIdentityUnverifiable)
    }
}

#[cfg(any(test, not(target_os = "linux")))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProcessLiveness {
    Dead,
    Unverifiable,
}

#[cfg(any(test, not(target_os = "linux")))]
const fn retain_holder_for_liveness(liveness: ProcessLiveness) -> bool {
    matches!(liveness, ProcessLiveness::Unverifiable)
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
pub(super) mod imp {
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

    #[cfg(not(target_os = "linux"))]
    pub(super) fn foreign_process_liveness(pid: u32) -> super::ProcessLiveness {
        let Ok(pid) = libc::pid_t::try_from(pid) else {
            return super::ProcessLiveness::Unverifiable;
        };
        // SAFETY: Signal 0 performs an existence/permission probe only. The
        // converted PID is passed by value and no Rust memory is exposed.
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 {
            return super::ProcessLiveness::Unverifiable;
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(code) if code == libc::ESRCH => super::ProcessLiveness::Dead,
            _ => super::ProcessLiveness::Unverifiable,
        }
    }
}

#[cfg(not(unix))]
pub(super) mod imp {
    use std::fs::File;
    use std::io;

    pub fn try_lock_exclusive(_file: &File) -> Result<bool, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "profile database admission requires cross-process file locking",
        ))
    }
}

#[cfg(test)]
#[path = "db_admission_tests.rs"]
mod tests;
