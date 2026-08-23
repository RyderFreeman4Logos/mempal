mod common;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::harness::CapturedChild;
use tempfile::TempDir;

#[cfg(unix)]
#[test]
fn captured_child_waits_for_delayed_stderr_event() {
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
