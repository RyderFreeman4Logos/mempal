//! Bounded Linux daemon readiness checks for lifecycle automation.
//!
//! Readiness is intentionally stricter than process liveness: exactly one
//! daemon must be registered for the configured database, its PID file and
//! executable must identify that process, and the write IPC service must
//! answer a read-only probe whose kernel-authenticated peer PID is that process.

use std::fmt;
use std::path::Path;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use thiserror::Error;

const READINESS_POLL_INTERVAL: Duration = Duration::from_millis(50);
const WRITE_TRANSPORT_PROBE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DaemonReadinessState {
    ProbePending,
    SingletonNotRegistered,
    MultipleDaemons,
    RegistrationPending,
    ExecutableUnavailable,
    ExecutableNotCurrent,
    WriteTransportUnavailable,
    Ready(i32),
}

impl fmt::Display for DaemonReadinessState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ProbePending => write!(f, "daemon readiness probe still running"),
            Self::SingletonNotRegistered => write!(f, "singleton daemon not registered"),
            Self::MultipleDaemons => write!(f, "multiple daemon processes detected"),
            Self::RegistrationPending => write!(f, "singleton daemon registration pending"),
            Self::ExecutableUnavailable => write!(f, "daemon executable identity unavailable"),
            Self::ExecutableNotCurrent => write!(f, "daemon executable is not current"),
            Self::WriteTransportUnavailable => write!(f, "daemon write transport unavailable"),
            Self::Ready(_) => write!(f, "ready"),
        }
    }
}

/// A bounded readiness failure whose message contains no paths or payloads.
#[derive(Debug, Error)]
pub enum DaemonReadinessError {
    #[error("daemon readiness timed out after {timeout_secs}s (last state: {last_state})")]
    Timeout {
        timeout_secs: u64,
        last_state: String,
    },
    #[error("daemon readiness is supported only on Linux")]
    UnsupportedPlatform,
    #[error("daemon readiness probe could not start")]
    ProbeStart,
}

/// Wait until the singleton daemon and its write transport are ready.
///
/// Success means one daemon owns the configured database, `daemon.pid`
/// identifies it, `/proc/<pid>/exe` matches this CLI executable, and that same
/// PID answered the read-only hook IPC readiness probe, as authenticated by
/// the Unix socket peer credentials.
pub fn wait(db_path: &Path, timeout: Duration) -> Result<i32, DaemonReadinessError> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (db_path, timeout);
        return Err(DaemonReadinessError::UnsupportedPlatform);
    }

    #[cfg(target_os = "linux")]
    {
        let db_path = db_path.to_path_buf();
        wait_with_probe(timeout, move |transport_timeout| {
            probe(&db_path, transport_timeout)
        })
    }
}

fn wait_with_probe(
    timeout: Duration,
    mut probe: impl FnMut(Duration) -> DaemonReadinessState + Send + 'static,
) -> Result<i32, DaemonReadinessError> {
    let started = Instant::now();
    let deadline = started.checked_add(timeout).unwrap_or(started);
    let (state_sender, state_receiver) = mpsc::sync_channel(1);
    std::thread::Builder::new()
        .name("mempal-daemon-readiness".to_string())
        .spawn(move || {
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let state = probe(WRITE_TRANSPORT_PROBE_TIMEOUT.min(remaining));
                let ready = matches!(state, DaemonReadinessState::Ready(_));
                if state_sender.send(state).is_err() || ready {
                    break;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                std::thread::sleep(READINESS_POLL_INTERVAL.min(remaining));
            }
        })
        .map_err(|_| DaemonReadinessError::ProbeStart)?;

    let mut last_state = DaemonReadinessState::ProbePending;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(DaemonReadinessError::Timeout {
                timeout_secs: timeout.as_secs(),
                last_state: last_state.to_string(),
            });
        }
        match state_receiver.recv_timeout(remaining) {
            Ok(state) => {
                last_state = state;
                if Instant::now() >= deadline {
                    continue;
                }
                if let DaemonReadinessState::Ready(pid) = state {
                    return Ok(pid);
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout | mpsc::RecvTimeoutError::Disconnected) => {
                return Err(DaemonReadinessError::Timeout {
                    timeout_secs: timeout.as_secs(),
                    last_state: last_state.to_string(),
                });
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn probe(db_path: &Path, transport_timeout: Duration) -> DaemonReadinessState {
    let Some(binary_name) = crate::daemon_singleton::current_binary_name() else {
        return DaemonReadinessState::ExecutableUnavailable;
    };
    let daemons = crate::daemon_singleton::enumerate_daemon_processes(&binary_name, db_path);
    let daemon = match daemons.as_slice() {
        [] => return DaemonReadinessState::SingletonNotRegistered,
        [daemon] => daemon,
        _ => return DaemonReadinessState::MultipleDaemons,
    };
    let daemon_pid = daemon.pid;
    if read_registered_pid(db_path) != Some(daemon_pid) {
        return DaemonReadinessState::RegistrationPending;
    }
    match daemon_executable_is_current(daemon_pid) {
        None => return DaemonReadinessState::ExecutableUnavailable,
        Some(false) => return DaemonReadinessState::ExecutableNotCurrent,
        Some(true) => {}
    }

    let mempal_home = db_path.parent().unwrap_or(db_path);
    match crate::hook_ipc::probe_readiness(mempal_home, transport_timeout) {
        Some(response)
            if readiness_response_matches(
                daemon,
                &response,
                daemon.matches_process_identity(&response.process_identity),
            ) =>
        {
            DaemonReadinessState::Ready(daemon_pid)
        }
        _ => DaemonReadinessState::WriteTransportUnavailable,
    }
}

#[cfg(target_os = "linux")]
fn readiness_response_matches(
    daemon: &crate::daemon_singleton::DaemonProcess,
    response: &crate::hook_ipc::HookIpcReadiness,
    process_birth_matches: bool,
) -> bool {
    i32::try_from(response.pid).ok() == Some(daemon.pid)
        && response.peer_pid == response.pid
        && !response.process_identity.is_empty()
        && process_birth_matches
}

fn read_registered_pid(db_path: &Path) -> Option<i32> {
    let mempal_home = db_path.parent().unwrap_or(db_path);
    std::fs::read_to_string(mempal_home.join("daemon.pid"))
        .ok()
        .and_then(|content| content.trim().parse::<i32>().ok())
        .filter(|pid| *pid > 0)
}

#[cfg(target_os = "linux")]
fn daemon_executable_is_current(pid: i32) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;

    let daemon_exe = format!("/proc/{pid}/exe");
    let daemon_target = std::fs::read_link(&daemon_exe).ok()?;
    if daemon_target.to_string_lossy().ends_with(" (deleted)") {
        return Some(false);
    }
    let current = std::fs::metadata("/proc/self/exe").ok()?;
    let daemon = std::fs::metadata(daemon_exe).ok()?;
    Some(current.dev() == daemon.dev() && current.ino() == daemon.ino())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_readiness_states_are_explicit_and_redacted() {
        let states = [
            DaemonReadinessState::ProbePending,
            DaemonReadinessState::SingletonNotRegistered,
            DaemonReadinessState::MultipleDaemons,
            DaemonReadinessState::RegistrationPending,
            DaemonReadinessState::ExecutableUnavailable,
            DaemonReadinessState::ExecutableNotCurrent,
            DaemonReadinessState::WriteTransportUnavailable,
        ];
        for state in states {
            let rendered = state.to_string();
            assert!(!rendered.is_empty());
            assert!(!rendered.contains('/'));
            assert!(!rendered.contains("palace.db"));
        }
    }

    #[test]
    fn test_timeout_reports_last_readiness_boundary_without_path() {
        let error = DaemonReadinessError::Timeout {
            timeout_secs: 3,
            last_state: DaemonReadinessState::WriteTransportUnavailable.to_string(),
        };
        assert_eq!(
            error.to_string(),
            "daemon readiness timed out after 3s (last state: daemon write transport unavailable)"
        );
    }

    #[test]
    fn test_slow_probe_cannot_return_ready_after_deadline() {
        let (release_sender, release_receiver) = mpsc::channel();
        let error = wait_with_probe(Duration::from_millis(10), move |_| {
            release_receiver
                .recv()
                .expect("release deliberately slow readiness probe");
            DaemonReadinessState::Ready(42)
        })
        .expect_err("slow probe must time out before reporting ready");

        assert!(matches!(error, DaemonReadinessError::Timeout { .. }));
        release_sender.send(()).expect("release probe worker");
    }

    #[test]
    fn test_unsupported_platform_error_is_immediate_and_redacted() {
        assert_eq!(
            DaemonReadinessError::UnsupportedPlatform.to_string(),
            "daemon readiness is supported only on Linux"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_readiness_response_rejects_same_pid_after_process_birth_changes() {
        let daemon = crate::daemon_singleton::DaemonProcess {
            pid: 42,
            start_time_ticks: 7,
        };
        let response = crate::hook_ipc::HookIpcReadiness {
            pid: 42,
            peer_pid: 42,
            process_identity: "previous-process-birth".to_string(),
        };

        assert!(!readiness_response_matches(&daemon, &response, false));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn test_readiness_response_rejects_unverified_socket_peer() {
        let daemon = crate::daemon_singleton::DaemonProcess {
            pid: 42,
            start_time_ticks: 7,
        };
        let response = crate::hook_ipc::HookIpcReadiness {
            pid: 42,
            peer_pid: 99,
            process_identity: "claimed-process-birth".to_string(),
        };

        assert!(!readiness_response_matches(&daemon, &response, true));
    }
}
