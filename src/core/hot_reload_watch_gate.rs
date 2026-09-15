use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(crate) const GATE_ENV: &str = "MEMPAL_TEST_WATCH_REGISTRATION_GATE";
pub(crate) const ENTERED_NAME: &str = "entered";
pub(crate) const RELEASE_NAME: &str = "release";
pub(crate) const WATCHED_NAME: &str = "watched";
pub(crate) const POLL_GATE_ENV: &str = "MEMPAL_TEST_POLL_BASELINE_GATE";
pub(crate) const POLL_ENTERED_NAME: &str = "poll-entered";
pub(crate) const POLL_RELEASE_NAME: &str = "poll-release";

const RELEASE_WAIT: Duration = Duration::from_secs(5);
const RELEASE_POLL: Duration = Duration::from_millis(5);

/// Hold `watch()` until the test writes `release`. Bounded so a RED path can still reap.
pub(crate) fn wait_before_watch() {
    if let Some(dir) = wait_for_release(GATE_ENV, ENTERED_NAME, RELEASE_NAME) {
        let _ = std::fs::write(dir.join(WATCHED_NAME), b"");
    }
}

/// Hold the fallback poller's initial snapshot until the test writes `poll-release`.
pub(crate) fn wait_before_poll_baseline() {
    let _ = wait_for_release(POLL_GATE_ENV, POLL_ENTERED_NAME, POLL_RELEASE_NAME);
}

fn wait_for_release(env: &str, entered: &str, release: &str) -> Option<PathBuf> {
    let dir = PathBuf::from(std::env::var_os(env)?);
    let _ = std::fs::write(dir.join(entered), b"");
    let release = dir.join(release);
    let deadline = Instant::now() + RELEASE_WAIT;
    while !release.exists() {
        if Instant::now() >= deadline {
            break;
        }
        std::thread::park_timeout(RELEASE_POLL);
    }
    Some(dir)
}
