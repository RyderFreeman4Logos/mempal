use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::config::ConfigHandle;
use super::hot_reload_watch_gate::{ENTERED_NAME, GATE_ENV, RELEASE_NAME, WATCHED_NAME};

struct WatchGate {
    dir: TempDir,
}

impl WatchGate {
    fn arm() -> Self {
        let dir = TempDir::new().expect("watch gate tempdir");
        // SAFETY: callers hold global_config_test_lock for the singleton watcher.
        unsafe {
            std::env::set_var(GATE_ENV, dir.path());
        }
        Self { dir }
    }

    fn entered(&self) -> bool {
        self.dir.path().join(ENTERED_NAME).exists()
    }

    fn watched(&self) -> bool {
        self.dir.path().join(WATCHED_NAME).exists()
    }

    fn release(&self) {
        fs::write(self.dir.path().join(RELEASE_NAME), b"").expect("write watch release");
    }
}

impl Drop for WatchGate {
    fn drop(&mut self) {
        self.release();
        // SAFETY: same lock scope as arm(); always clear so later tests are not gated.
        unsafe {
            std::env::remove_var(GATE_ENV);
        }
        ConfigHandle::harness_reset();
    }
}

fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        thread::park_timeout(Duration::from_millis(5));
    }
    predicate()
}

#[tokio::test]
async fn bootstrap_waits_until_watcher_registration_completes() {
    let lock = super::config::global_config_test_lock();
    let _lock = lock.lock().await;
    let gate = WatchGate::arm();
    let tmp = TempDir::new().expect("config tempdir");
    let config_path = tmp.path().join("config.toml");
    fs::write(
        &config_path,
        r#"
[config_hot_reload]
enabled = true
debounce_ms = 10
poll_fallback_secs = 1
"#,
    )
    .expect("write config");

    let (done_tx, done_rx) = mpsc::channel();
    let bootstrap_path = config_path.clone();
    let worker = thread::spawn(move || {
        let result = ConfigHandle::bootstrap(&bootstrap_path);
        let _ = done_tx.send(());
        result
    });

    assert!(
        wait_until(Duration::from_secs(2), || gate.entered()),
        "coordinator never reached the watch gate"
    );
    if done_rx.recv_timeout(Duration::from_millis(1500)).is_ok() {
        panic!("bootstrap returned before watcher registration completed");
    }

    gate.release();
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("bootstrap must return after watch registration");
    worker
        .join()
        .expect("bootstrap thread")
        .expect("bootstrap config");
    assert!(
        gate.watched(),
        "bootstrap returned before parent-directory watch() was attempted"
    );
    assert!(ConfigHandle::harness_runtime_active());
}
