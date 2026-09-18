use std::fs;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use tempfile::TempDir;

use super::config::ConfigHandle;
use super::hot_reload_watch_gate::{
    ENTERED_NAME, GATE_ENV, NON_COOPERATIVE_ENV, POLL_ENTERED_NAME, POLL_GATE_ENV,
    POLL_RELEASE_NAME, RELEASE_NAME, WATCHED_NAME,
};

struct FileGate {
    dir: TempDir,
    env: &'static str,
    entered: &'static str,
    release: &'static str,
    non_cooperative: bool,
    worker: Option<thread::JoinHandle<()>>,
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
            non_cooperative: false,
            worker: None,
        }
    }

    fn arm_non_cooperative() -> Self {
        let mut gate = Self::arm(GATE_ENV, ENTERED_NAME, RELEASE_NAME);
        // SAFETY: callers hold global_config_test_lock for the singleton watcher.
        unsafe {
            std::env::set_var(NON_COOPERATIVE_ENV, "1");
        }
        gate.non_cooperative = true;
        gate
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
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        ConfigHandle::harness_reset();
        // SAFETY: same lock scope as arm(); always clear so later tests are not gated.
        unsafe {
            std::env::remove_var(self.env);
            if self.non_cooperative {
                std::env::remove_var(NON_COOPERATIVE_ENV);
            }
        }
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
    assert_blocked_bootstrap(FileGate::arm_non_cooperative());
}

#[tokio::test]
async fn blocked_initial_baseline_returns_bounded_error_without_runtime() {
    let lock = super::config::global_config_test_lock();
    let _lock = lock.lock().await;
    assert_blocked_bootstrap(FileGate::arm(
        POLL_GATE_ENV,
        POLL_ENTERED_NAME,
        POLL_RELEASE_NAME,
    ));
}

fn assert_blocked_bootstrap(mut gate: FileGate) {
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
    .expect("write config");

    let (done_tx, done_rx) = mpsc::channel();
    let bootstrap_path = config_path.clone();
    let reloads = ConfigHandle::harness_reload_counter();
    let previous_reloads = reloads.load(Ordering::SeqCst);
    let started = Instant::now();
    gate.worker = Some(thread::spawn(move || {
        let result = ConfigHandle::bootstrap(&bootstrap_path);
        let _ = done_tx.send(result.is_err());
    }));
    assert!(
        wait_until(Duration::from_secs(2), || gate.entered()),
        "coordinator never reached the IO gate"
    );
    let bounded_result = done_rx.recv_timeout(Duration::from_millis(1500));
    let elapsed = started.elapsed();
    if bounded_result == Ok(true) {
        assert!(!ConfigHandle::harness_runtime_active());
        // A blocked registration must retain ownership and refuse further workers.
        for _ in 0..3 {
            let retry = Instant::now();
            let error = ConfigHandle::bootstrap(&config_path).expect_err("cleanup pending");
            assert!(error.to_string().contains("cleanup still pending"));
            assert!(retry.elapsed() < Duration::from_millis(250));
        }
    }
    let config = fs::read_to_string(&config_path).expect("fixture config");
    fs::write(
        &config_path,
        config.replace("isolation = false", "isolation = true"),
    )
    .expect("change config before releasing abandoned IO");
    gate.release();
    gate.worker
        .take()
        .expect("bootstrap worker")
        .join()
        .expect("bootstrap thread");
    // Independent release precedes both joins, including the deferred reaper.
    ConfigHandle::harness_reset();
    assert_eq!(
        reloads.load(Ordering::SeqCst),
        previous_reloads,
        "abandoned IO published a reload"
    );
    assert_eq!(bounded_result, Ok(true), "bootstrap did not fail boundedly");
    assert!(elapsed < Duration::from_secs(2));
    assert!(!ConfigHandle::harness_runtime_active());
    if gate.env == GATE_ENV {
        assert!(
            !gate.exists(WATCHED_NAME),
            "timed-out worker registered a watch"
        );
    }
    ConfigHandle::bootstrap(&config_path).expect("retry after cleanup");
    assert!(ConfigHandle::harness_runtime_active());
}

#[tokio::test]
async fn failed_registration_preserves_previous_runtime_projection() {
    let lock = super::config::global_config_test_lock();
    let _lock = lock.lock().await;
    ConfigHandle::harness_reset();
    let tmp = TempDir::new().expect("config tempdir");
    let previous_dir = tmp.path().join("previous");
    let candidate_dir = tmp.path().join("candidate");
    fs::create_dir_all(&previous_dir).expect("previous config directory");
    fs::create_dir_all(&candidate_dir).expect("candidate config directory");
    let previous_path = previous_dir.join("config.toml");
    let candidate_path = candidate_dir.join("config.toml");
    fs::write(
        &previous_path,
        r#"
[config_hot_reload]
enabled = true
debounce_ms = 10
poll_fallback_secs = 1
[search]
strict_project_isolation = false
[ingest_gating.embedding_classifier]
prototypes = ["previous"]
"#,
    )
    .expect("write previous config");
    fs::write(
        &candidate_path,
        r#"
[config_hot_reload]
enabled = true
debounce_ms = 10
poll_fallback_secs = 1
[search]
strict_project_isolation = true
[ingest_gating.embedding_classifier]
prototypes = ["candidate"]
"#,
    )
    .expect("write candidate config");
    ConfigHandle::bootstrap(&previous_path).expect("bootstrap previous runtime");
    let previous_meta = ConfigHandle::snapshot_meta();
    let previous_events = ConfigHandle::recent_events();
    let previous_event_path = ConfigHandle::harness_event_log_path();
    let previous_prototypes = ConfigHandle::runtime_prototypes();
    assert!(ConfigHandle::harness_runtime_active());

    let mut gate = FileGate::arm_non_cooperative();
    let (done_tx, done_rx) = mpsc::channel();
    gate.worker = Some(thread::spawn(move || {
        let result = ConfigHandle::bootstrap(&candidate_path).map_err(|error| error.to_string());
        let _ = done_tx.send(result);
    }));
    assert!(
        wait_until(Duration::from_secs(2), || gate.entered()),
        "candidate watcher never reached the registration gate"
    );
    let error = done_rx
        .recv_timeout(Duration::from_millis(1500))
        .expect("candidate bootstrap must fail boundedly")
        .expect_err("blocked candidate bootstrap must fail");
    assert!(error.contains("watcher registration timed out"));

    assert_eq!(ConfigHandle::snapshot_meta(), previous_meta);
    assert!(!ConfigHandle::current().search.strict_project_isolation);
    assert_eq!(ConfigHandle::runtime_prototypes(), previous_prototypes);
    assert_eq!(ConfigHandle::recent_events(), previous_events);
    assert_eq!(ConfigHandle::harness_event_log_path(), previous_event_path);
    assert!(
        ConfigHandle::harness_runtime_active(),
        "failed candidate registration stopped the previous runtime"
    );
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

#[tokio::test]
async fn fallback_detects_return_to_original_file_after_notify_reload() {
    let lock = super::config::global_config_test_lock();
    let _lock = lock.lock().await;
    let gate = FileGate::arm(GATE_ENV, ENTERED_NAME, RELEASE_NAME);
    gate.release();
    let tmp = TempDir::new().expect("config tempdir");
    let path = tmp.path().join("config.toml");
    let original = tmp.path().join("original.toml");
    let replacement = tmp.path().join("replacement.toml");
    let config = "[config_hot_reload]\nenabled = true\ndebounce_ms = 10\npoll_fallback_secs = 1\n[search]\nstrict_project_isolation = false\n";
    fs::write(&path, config).expect("initial config");
    // Preserve the exact original signature, including inode and mtime.
    fs::hard_link(&path, &original).expect("retain original inode");
    ConfigHandle::bootstrap(&path).expect("bootstrap config");
    fs::write(
        &replacement,
        config.replace("isolation = false", "isolation = true"),
    )
    .expect("replacement config");
    fs::rename(&replacement, &path).expect("replace config");
    assert!(wait_until(Duration::from_secs(2), || {
        ConfigHandle::current().search.strict_project_isolation
    }));
    ConfigHandle::simulate_notify_failure();
    assert!(wait_until(Duration::from_secs(2), || {
        ConfigHandle::recent_events()
            .iter()
            .any(|event| event.contains("notify watcher crashed, falling back to poll"))
    }));
    fs::rename(&original, &path).expect("restore original inode");
    assert!(
        wait_until(Duration::from_secs(2), || {
            !ConfigHandle::current().search.strict_project_isolation
        }),
        "poll fallback missed the original file after a notify reload"
    );
}
