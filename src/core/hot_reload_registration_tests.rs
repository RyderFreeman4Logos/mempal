use std::fs;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::config::ConfigHandle;
use super::hot_reload_watch_gate::{
    ENTERED_NAME, GATE_ENV, POLL_ENTERED_NAME, POLL_GATE_ENV, POLL_RELEASE_NAME, RELEASE_NAME,
    WATCHED_NAME,
};

struct FileGate {
    dir: TempDir,
    env: &'static str,
    entered: &'static str,
    release: &'static str,
}

impl FileGate {
    fn arm(env: &'static str, entered: &'static str, release: &'static str) -> Self {
        let dir = TempDir::new().expect("gate tempdir");
        // SAFETY: callers hold global_config_test_lock for the singleton watcher.
        unsafe {
            std::env::set_var(env, dir.path());
        }
        Self {
            dir,
            env,
            entered,
            release,
        }
    }

    fn entered(&self) -> bool {
        self.dir.path().join(self.entered).exists()
    }

    fn exists(&self, name: &str) -> bool {
        self.dir.path().join(name).exists()
    }

    fn release(&self) {
        fs::write(self.dir.path().join(self.release), b"").expect("write gate release");
    }
}

impl Drop for FileGate {
    fn drop(&mut self) {
        self.release();
        // SAFETY: same lock scope as arm(); always clear so later tests are not gated.
        unsafe {
            std::env::remove_var(self.env);
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
    let gate = FileGate::arm(GATE_ENV, ENTERED_NAME, RELEASE_NAME);
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
    if done_rx.recv_timeout(Duration::from_millis(150)).is_ok() {
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
        gate.exists(WATCHED_NAME),
        "bootstrap returned before parent-directory watch() was attempted"
    );
    assert!(ConfigHandle::harness_runtime_active());
}

#[tokio::test]
async fn bootstrap_applies_change_from_watcher_registration_window() {
    let lock = super::config::global_config_test_lock();
    let _lock = lock.lock().await;
    let gate = FileGate::arm(GATE_ENV, ENTERED_NAME, RELEASE_NAME);
    let tmp = TempDir::new().expect("config tempdir");
    let config_path = tmp.path().join("config.toml");
    fs::write(
        &config_path,
        r#"
[config_hot_reload]
enabled = true
debounce_ms = 10
poll_fallback_secs = 1

[search]
strict_project_isolation = false
"#,
    )
    .expect("write initial config");

    let bootstrap_path = config_path.clone();
    let worker = thread::spawn(move || ConfigHandle::bootstrap(&bootstrap_path));
    assert!(
        wait_until(Duration::from_secs(2), || gate.entered()),
        "coordinator never reached the watch gate"
    );
    fs::write(
        &config_path,
        r#"
[config_hot_reload]
enabled = true
debounce_ms = 10
poll_fallback_secs = 1

[search]
strict_project_isolation = true
"#,
    )
    .expect("write config during registration");
    gate.release();
    worker
        .join()
        .expect("bootstrap thread")
        .expect("bootstrap config");

    assert!(
        ConfigHandle::current().search.strict_project_isolation,
        "registration-window update was not applied before bootstrap returned"
    );
}

#[tokio::test]
async fn blocked_watcher_registration_returns_bounded_error_without_runtime() {
    let lock = super::config::global_config_test_lock();
    let _lock = lock.lock().await;
    let gate = FileGate::arm(GATE_ENV, ENTERED_NAME, RELEASE_NAME);
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
    let started = Instant::now();
    let worker = thread::spawn(move || {
        let result = ConfigHandle::bootstrap(&bootstrap_path);
        let _ = done_tx.send(result.is_err());
        result
    });
    assert!(
        wait_until(Duration::from_secs(2), || gate.entered()),
        "coordinator never reached the watch gate"
    );
    let bounded_result = done_rx.recv_timeout(Duration::from_millis(1500));
    gate.release();
    let result = worker.join().expect("bootstrap thread");

    assert_eq!(bounded_result, Ok(true), "bootstrap did not fail boundedly");
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(result.is_err());
    assert!(!ConfigHandle::harness_runtime_active());
}

#[tokio::test]
async fn bootstrap_waits_until_poll_fallback_baseline_completes() {
    let lock = super::config::global_config_test_lock();
    let _lock = lock.lock().await;
    let gate = FileGate::arm(POLL_GATE_ENV, POLL_ENTERED_NAME, POLL_RELEASE_NAME);
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
        "poller never reached the baseline gate"
    );
    if done_rx.recv_timeout(Duration::from_millis(150)).is_ok() {
        panic!("bootstrap returned before poll fallback baseline completed");
    }

    gate.release();
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("bootstrap must return after poll fallback baseline");
    worker
        .join()
        .expect("bootstrap thread")
        .expect("bootstrap config");
    assert!(ConfigHandle::harness_runtime_active());
}
