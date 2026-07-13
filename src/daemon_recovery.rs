//! Persistent restart-budget state for daemon self-recovery.
//!
//! Supervisors may restart a failed daemon indefinitely. This module keeps a
//! profile-local rolling fault window so each daemon generation shares one
//! finite restart budget. Recovery state contains only fault classes and
//! timestamps; it never stores database content, command lines, or endpoints.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub const RESTART_WINDOW_SECS: u64 = 10 * 60;
pub const RESTART_COOLDOWN_SECS: u64 = 15 * 60;
pub const MAX_RESTARTS_PER_WINDOW: usize = 3;

const LOCK_TIMEOUT: Duration = Duration::from_millis(250);
const LOCK_RETRY: Duration = Duration::from_millis(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryFault {
    DatabaseLocked,
    WriteStall,
    WriterLeaseLost,
}

impl RecoveryFault {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::DatabaseLocked => "database_locked",
            Self::WriteStall => "write_stall",
            Self::WriterLeaseLost => "writer_lease_lost",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryPhase {
    #[default]
    Healthy,
    Recovering,
    Cooldown,
}

impl RecoveryPhase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::Recovering => "recovering",
            Self::Cooldown => "cooldown",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartDecision {
    RestartAllowed,
    CooldownRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonRecoverySnapshot {
    pub phase: RecoveryPhase,
    pub recent_fault_count: usize,
    pub restart_budget_remaining: usize,
    pub cooldown_remaining_secs: u64,
    pub last_fault: Option<RecoveryFault>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonRecoveryState {
    phase: RecoveryPhase,
    faults: Vec<FaultRecord>,
    cooldown_until_unix_secs: u64,
    last_fault: Option<RecoveryFault>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FaultRecord {
    fault: RecoveryFault,
    occurred_at_unix_secs: u64,
}

impl DaemonRecoveryState {
    pub fn record_fault(&mut self, fault: RecoveryFault, now_secs: u64) -> RestartDecision {
        self.prune(now_secs);
        self.last_fault = Some(fault);
        self.faults.push(FaultRecord {
            fault,
            occurred_at_unix_secs: now_secs,
        });
        if self.faults.len() >= MAX_RESTARTS_PER_WINDOW {
            self.phase = RecoveryPhase::Cooldown;
            self.cooldown_until_unix_secs = now_secs.saturating_add(RESTART_COOLDOWN_SECS);
            RestartDecision::CooldownRequired
        } else {
            self.phase = RecoveryPhase::Recovering;
            RestartDecision::RestartAllowed
        }
    }

    pub fn admit_start(&mut self, now_secs: u64) -> RestartDecision {
        self.prune(now_secs);
        if self.phase == RecoveryPhase::Cooldown && now_secs < self.cooldown_until_unix_secs {
            return RestartDecision::CooldownRequired;
        }
        if now_secs >= self.cooldown_until_unix_secs {
            self.cooldown_until_unix_secs = 0;
        }
        self.phase = RecoveryPhase::Recovering;
        RestartDecision::RestartAllowed
    }

    pub fn record_recovered(&mut self, now_secs: u64) {
        self.prune(now_secs);
        self.phase = RecoveryPhase::Healthy;
        self.cooldown_until_unix_secs = 0;
    }

    pub fn snapshot(&mut self, now_secs: u64) -> DaemonRecoverySnapshot {
        self.prune(now_secs);
        let restart_budget_remaining = if self.phase == RecoveryPhase::Cooldown {
            0
        } else {
            MAX_RESTARTS_PER_WINDOW.saturating_sub(self.faults.len())
        };
        DaemonRecoverySnapshot {
            phase: self.phase,
            recent_fault_count: self.faults.len(),
            restart_budget_remaining,
            cooldown_remaining_secs: self.cooldown_until_unix_secs.saturating_sub(now_secs),
            last_fault: self.last_fault,
        }
    }

    fn prune(&mut self, now_secs: u64) {
        self.faults.retain(|fault| {
            now_secs.saturating_sub(fault.occurred_at_unix_secs) <= RESTART_WINDOW_SECS
        });
        if self.phase == RecoveryPhase::Cooldown && now_secs >= self.cooldown_until_unix_secs {
            self.phase = RecoveryPhase::Healthy;
            self.cooldown_until_unix_secs = 0;
            self.faults.clear();
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DaemonRecoveryError {
    #[error("daemon recovery state is busy after {timeout_ms}ms")]
    Busy { timeout_ms: u64 },
    #[error("daemon recovery state I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("daemon recovery state is invalid: {0}")]
    InvalidState(#[from] serde_json::Error),
}

#[derive(Debug, Clone)]
pub struct DaemonRecovery {
    state_path: PathBuf,
    lock_path: PathBuf,
}

/// Process-local deduplication in front of the persistent fault budget.
#[derive(Debug, Clone)]
pub struct DaemonRecoveryFaultReporter {
    recovery: DaemonRecovery,
    reported: Arc<AtomicBool>,
}

impl DaemonRecoveryFaultReporter {
    pub fn new(recovery: DaemonRecovery) -> Self {
        Self {
            recovery,
            reported: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn record_fault_once(&self, fault: RecoveryFault) {
        if self
            .reported
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        match self.recovery.record_fault(fault) {
            Ok(decision) => tracing::error!(
                ?fault,
                ?decision,
                "daemon recovery fault recorded; requesting bounded supervisor restart"
            ),
            Err(error) => tracing::error!(
                ?fault,
                %error,
                "failed to persist daemon recovery fault; requesting shutdown"
            ),
        }
    }
}

impl DaemonRecovery {
    pub fn new(mempal_home: &Path) -> Self {
        Self {
            state_path: mempal_home.join("daemon-recovery.json"),
            lock_path: mempal_home.join("daemon-recovery.lock"),
        }
    }

    pub fn admit_start(&self) -> Result<RestartDecision, DaemonRecoveryError> {
        self.update(|state, now| state.admit_start(now))
    }

    pub fn record_fault(
        &self,
        fault: RecoveryFault,
    ) -> Result<RestartDecision, DaemonRecoveryError> {
        self.update(|state, now| state.record_fault(fault, now))
    }

    pub fn record_recovered(&self) -> Result<(), DaemonRecoveryError> {
        self.update(|state, now| state.record_recovered(now))
    }

    pub fn snapshot(&self) -> Result<DaemonRecoverySnapshot, DaemonRecoveryError> {
        let _lock = RecoveryLock::acquire(&self.lock_path)?;
        let mut state = load_state(&self.state_path)?;
        Ok(state.snapshot(unix_secs()))
    }

    fn update<T>(
        &self,
        operation: impl FnOnce(&mut DaemonRecoveryState, u64) -> T,
    ) -> Result<T, DaemonRecoveryError> {
        if let Some(parent) = self.state_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let _lock = RecoveryLock::acquire(&self.lock_path)?;
        let mut state = load_state(&self.state_path)?;
        let result = operation(&mut state, unix_secs());
        save_state(&self.state_path, &state)?;
        Ok(result)
    }
}

fn load_state(path: &Path) -> Result<DaemonRecoveryState, DaemonRecoveryError> {
    match fs::read(path) {
        Ok(raw) => Ok(serde_json::from_slice(&raw)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(DaemonRecoveryState::default()),
        Err(error) => Err(error.into()),
    }
}

fn save_state(path: &Path, state: &DaemonRecoveryState) -> Result<(), DaemonRecoveryError> {
    let temporary = path.with_extension(format!("json.tmp.{}", std::process::id()));
    let encoded = serde_json::to_vec(state)?;
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(&encoded)?;
    file.sync_all()?;
    fs::rename(&temporary, path)?;
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

struct RecoveryLock(File);

impl RecoveryLock {
    fn acquire(path: &Path) -> Result<Self, DaemonRecoveryError> {
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        let started = Instant::now();
        loop {
            // SAFETY: `file` owns a valid descriptor for the duration of the
            // call; `flock` neither retains the descriptor nor dereferences
            // memory supplied by Rust.
            let result = unsafe {
                libc::flock(
                    std::os::fd::AsRawFd::as_raw_fd(&file),
                    libc::LOCK_EX | libc::LOCK_NB,
                )
            };
            if result == 0 {
                return Ok(Self(file));
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EWOULDBLOCK) {
                return Err(error.into());
            }
            if started.elapsed() >= LOCK_TIMEOUT {
                return Err(DaemonRecoveryError::Busy {
                    timeout_ms: LOCK_TIMEOUT.as_millis() as u64,
                });
            }
            std::thread::sleep(LOCK_RETRY);
        }
    }
}

impl Drop for RecoveryLock {
    fn drop(&mut self) {
        // SAFETY: the descriptor remains valid until this `Drop` completes.
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN);
        }
    }
}

fn unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}
