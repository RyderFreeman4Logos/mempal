use std::path::PathBuf;
use std::time::{Duration, Instant};

pub(crate) const GATE_ENV: &str = "MEMPAL_TEST_WATCH_REGISTRATION_GATE";
pub(crate) const ENTERED_NAME: &str = "entered";
pub(crate) const RELEASE_NAME: &str = "release";
pub(crate) const WATCHED_NAME: &str = "watched";

const RELEASE_WAIT: Duration = Duration::from_secs(5);
const RELEASE_POLL: Duration = Duration::from_millis(5);

/// Hold `watch()` until the test writes `release`. Bounded so a RED path can still reap.
pub(crate) fn wait_before_watch() {
    let Some(dir) = std::env::var_os(GATE_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let _ = std::fs::write(dir.join(ENTERED_NAME), b"");
    let release = dir.join(RELEASE_NAME);
    let deadline = Instant::now() + RELEASE_WAIT;
    while !release.exists() {
        if Instant::now() >= deadline {
            break;
        }
        std::thread::park_timeout(RELEASE_POLL);
    }
    let _ = std::fs::write(dir.join(WATCHED_NAME), b"");
}
