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
            if let Some(identity) = linux_process_identity(std::process::id()) {
                return identity;
            }
            fallback_process_identity()
        })
        .as_str()
}

/// Verify that a daemon owner still names the same process birth.
pub(crate) fn daemon_owner_matches_process(owner: &str, pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    let identity = linux_process_identity(pid);
    #[cfg(not(target_os = "linux"))]
    let identity = (pid == std::process::id()).then(|| current_process_identity().to_string());

    identity
        .map(|identity| format!("{DAEMON_OWNER_PREFIX}{pid}-{identity}"))
        .as_deref()
        == Some(owner)
}

/// Verify the process birth identity recorded by daemon-owned status.
///
/// Returns `Some(true)` if the identity matches, `Some(false)` if the
/// process is confirmed dead (zombie/exited), and `None` if the process
/// cannot be verified (PID namespace isolation, `hidepid`, etc.).
/// Callers must retain holders when `None` is returned (fail-closed).
#[cfg(target_os = "linux")]
pub(crate) fn process_identity_matches(pid: u32, expected: &str) -> Option<bool> {
    if pid == std::process::id() {
        return Some(current_process_identity() == expected);
    }
    linux_process_identity(pid).map(|identity| identity == expected)
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

#[cfg(target_os = "linux")]
fn linux_process_identity(pid: u32) -> Option<String> {
    if pid == 0 {
        return None;
    }
    let boot_id = boot_id()?;
    let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
    let start_ticks = parse_start_ticks(&stat)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(boot_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(pid.to_string().as_bytes());
    hasher.update(b"\0");
    hasher.update(start_ticks.to_string().as_bytes());
    Some(hasher.finalize().to_hex()[..16].to_string())
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
