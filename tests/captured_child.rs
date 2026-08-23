#[path = "common/harness/captured_child.rs"]
mod captured_child_harness;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use captured_child_harness::CapturedChild;
use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn captured_child_waits_for_delayed_stderr_event() {
    let _ = (
        CapturedChild::id,
        CapturedChild::kill,
        CapturedChild::signal,
        CapturedChild::signal_or_panic,
        captured_child_harness::hold_sqlite_lock_for,
        captured_child_harness::write_daemon_home_diagnostics,
    );
    let diagnostics = TempDir::new_in("/tmp").expect("short diagnostics dir");
    let mut command = Command::new("sh");
    command
        .args(["-c", "sleep 0.2; printf 'captured child ready\n' >&2"])
        .stdin(Stdio::null());
    let mut child = CapturedChild::spawn(&mut command, diagnostics.path(), "stderr-event", None)
        .expect("spawn delayed stderr child");
    let started = Instant::now();

    child.wait_for_stderr_event("captured child ready", Duration::from_secs(2));

    assert!(started.elapsed() >= Duration::from_millis(100));
    assert!(
        child
            .wait_or_panic("wait for delayed stderr child")
            .success(),
        "delayed stderr child must exit cleanly"
    );
}
