//! Process birth identities used to defend daemon state from PID reuse.

use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

const DAEMON_OWNER_PREFIX: &str = "mempal-daemon-";
static CURRENT_PROCESS_IDENTITY: OnceLock<String> = OnceLock::new();

/// Return the stable owner identity for the current daemon process.
pub(crate) fn current_daemon_owner() -> String {
    format!(
        "{DAEMON_OWNER_PREFIX}{}-{}",
        std::process::id(),
        current_process_identity()
    )
}

/// Return a process-lifetime identity that changes when a PID is reused.
pub(crate) fn current_process_identity() -> &'static str {
    CURRENT_PROCESS_IDENTITY
        .get_or_init(|| {
            #[cfg(target_os = "linux")]
            if let LinuxProcessResult::Alive(identity) = linux_process_identity(std::process::id())
            {
                return identity;
            }
            fallback_process_identity()
        })
        .as_str()
}

/// Verify that a daemon owner still names the same process birth.
pub(crate) fn daemon_owner_matches_process(owner: &str, pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    let identity = match linux_process_identity(pid) {
        LinuxProcessResult::Alive(id) => Some(id),
        _ => None,
    };
    #[cfg(not(target_os = "linux"))]
    let identity = (pid == std::process::id()).then(|| current_process_identity().to_string());

    identity
        .map(|identity| format!("{DAEMON_OWNER_PREFIX}{pid}-{identity}"))
        .as_deref()
        == Some(owner)
}

/// Process liveness classification for admission holder retention.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProcessLiveness {
    /// Identity matches — holder is alive.
    Live,
    /// Process is confirmed dead (no `/proc` entry, zombie, or identity mismatch).
    Dead,
    /// Cannot determine (permission denied, PID namespace isolation).
    /// Callers must retain the holder (fail-closed).
    Unverifiable,
}

/// Verify the process birth identity recorded by daemon-owned status.
///
/// Returns [`ProcessLiveness::Live`] if the identity matches,
/// [`ProcessLiveness::Dead`] if the process is confirmed dead or the
/// identity doesn't match, and [`ProcessLiveness::Unverifiable`] if the
/// process cannot be checked (PID namespace isolation, `hidepid`, etc.).
#[cfg(target_os = "linux")]
pub(crate) fn process_identity_liveness(pid: u32, expected: &str) -> ProcessLiveness {
    if pid == std::process::id() {
        return if current_process_identity() == expected {
            ProcessLiveness::Live
        } else {
            ProcessLiveness::Dead
        };
    }
    match linux_process_identity(pid) {
        LinuxProcessResult::Alive(identity) => {
            if identity == expected {
                ProcessLiveness::Live
            } else {
                ProcessLiveness::Dead
            }
        }
        LinuxProcessResult::Dead => ProcessLiveness::Dead,
        LinuxProcessResult::Unverifiable => ProcessLiveness::Unverifiable,
    }
}

/// Backwards-compatible bool wrapper for callers that only need match/no-match.
/// Returns `Some(true)` on match, `Some(false)` on confirmed dead/mismatch,
/// `None` on unverifiable.
#[cfg(target_os = "linux")]
pub(crate) fn process_identity_matches(pid: u32, expected: &str) -> Option<bool> {
    match process_identity_liveness(pid, expected) {
        ProcessLiveness::Live => Some(true),
        ProcessLiveness::Dead => Some(false),
        ProcessLiveness::Unverifiable => None,
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn boot_id() -> Option<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn boot_id() -> Option<String> {
    None
}

/// Result of probing a Linux process identity.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinuxProcessResult {
    /// Process exists and identity was computed.
    Alive(String),
    /// Process is confirmed dead (no `/proc` entry, zombie, or exited).
    Dead,
    /// Cannot determine (permission denied, namespace isolation).
    Unverifiable,
}

#[cfg(target_os = "linux")]
fn linux_process_identity(pid: u32) -> LinuxProcessResult {
    if pid == 0 {
        return LinuxProcessResult::Dead;
    }
    let Some(boot_id) = boot_id() else {
        return LinuxProcessResult::Unverifiable;
    };
    match std::fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => match parse_start_ticks(&stat) {
            Some(start_ticks) => {
                let mut hasher = blake3::Hasher::new();
                hasher.update(boot_id.as_bytes());
                hasher.update(b"\0");
                hasher.update(pid.to_string().as_bytes());
                hasher.update(b"\0");
                hasher.update(start_ticks.to_string().as_bytes());
                LinuxProcessResult::Alive(hasher.finalize().to_hex()[..16].to_string())
            }
            // parse_start_ticks returns None for zombie/dead states — process is dead.
            None => LinuxProcessResult::Dead,
        },
        Err(error) => match error.kind() {
            // Entry doesn't exist — process has exited.
            std::io::ErrorKind::NotFound => LinuxProcessResult::Dead,
            // Permission denied or other access error — unverifiable.
            std::io::ErrorKind::PermissionDenied => LinuxProcessResult::Unverifiable,
            _ => LinuxProcessResult::Unverifiable,
        },
    }
}

fn fallback_process_identity() -> String {
    let started_at_nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let mut hasher = blake3::Hasher::new();
    hasher.update(std::process::id().to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(started_at_nanos.to_string().as_bytes());
    hasher.finalize().to_hex()[..16].to_string()
}

#[cfg(target_os = "linux")]
pub(crate) fn parse_start_ticks(stat: &[u8]) -> Option<u64> {
    let close = stat.windows(2).rposition(|window| window == b") ")?;
    let mut fields = stat[close + 2..]
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty());
    let state = fields.next()?;
    if state.len() != 1 || matches!(state[0], b'Z' | b'X') {
        return None;
    }
    fields.nth(18)?.iter().try_fold(0_u64, |value, digit| {
        digit
            .checked_sub(b'0')
            .filter(|digit| *digit <= 9)
            .and_then(|digit| {
                value
                    .checked_mul(10)
                    .and_then(|value| value.checked_add(u64::from(digit)))
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_start_ticks_handles_spaces_and_closing_parentheses_in_name() {
        let stat = b"123 (mempal ) worker) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 98765";

        assert_eq!(parse_start_ticks(stat), Some(98765));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_start_ticks_rejects_zombie_and_dead_processes() {
        let zombie = b"123 (mempal) Z 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 98765";
        let dead = b"123 (mempal) X 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 98765";

        assert_eq!(parse_start_ticks(zombie), None);
        assert_eq!(parse_start_ticks(dead), None);
    }

    #[test]
    fn current_daemon_owner_includes_pid_and_birth_identity() {
        let owner = current_daemon_owner();

        assert!(owner.starts_with(&format!("{DAEMON_OWNER_PREFIX}{}-", std::process::id())));
        assert!(owner.len() > format!("{DAEMON_OWNER_PREFIX}{}-", std::process::id()).len());
        assert!(daemon_owner_matches_process(&owner, std::process::id()));
        assert!(!daemon_owner_matches_process(
            &format!("{DAEMON_OWNER_PREFIX}{}-previous-start", std::process::id()),
            std::process::id()
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn current_process_identity_matches_inside_pid_namespaces() {
        assert!(
            process_identity_matches(std::process::id(), current_process_identity())
                .unwrap_or(false)
        );
    }
}
