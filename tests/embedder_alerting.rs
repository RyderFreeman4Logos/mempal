use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use mempal::core::config::ConfigHandle;
use mempal::embed::global_embed_status;
use tempfile::TempDir;

fn test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("test mutex poisoned")
}

fn write_config(path: &Path, script_path: &Path, threshold: u64) {
    let tmp_path = path.with_extension("tmp");
    fs::write(
        &tmp_path,
        format!(
            r#"
db_path = "/tmp/mempal-test.db"

[embed]
backend = "openai_compat"

[embed.retry]
interval_secs = 1
search_deadline_secs = 5

[embed.alert]
enabled = true
script_path = "{}"
alert_every_n_failures = {}

[config_hot_reload]
enabled = true
debounce_ms = 100
poll_fallback_secs = 1
"#,
            script_path.display(),
            threshold
        ),
    )
    .expect("write temp config");
    fs::rename(&tmp_path, path).expect("rename config");
}

fn wait_until(timeout: Duration, step: Duration, mut predicate: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if predicate() {
            return;
        }
        std::thread::sleep(step);
    }
    assert!(predicate(), "condition not met before timeout");
}

fn line_count(path: &Path) -> usize {
    fs::read_to_string(path)
        .map(|content| content.lines().count())
        .unwrap_or(0)
}

#[test]
fn test_alert_script_invoked_on_threshold() {
    let _guard = test_guard();
    let tmp = TempDir::new().expect("tempdir");
    let config_path = tmp.path().join("config.toml");
    let alert_log = tmp.path().join("alerts.log");
    let script_path = tmp.path().join("alert.sh");
    fs::write(
        &script_path,
        format!("#!/bin/sh\necho \"$1\" >> \"{}\"\n", alert_log.display()),
    )
    .expect("write script");
    let mut permissions = fs::metadata(&script_path)
        .expect("script metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script_path, permissions).expect("chmod script");

    write_config(&config_path, &script_path, 3);
    ConfigHandle::bootstrap(&config_path).expect("bootstrap config");
    let previous_version = ConfigHandle::version();
    let status = global_embed_status();
    status.reset_for_tests();

    for index in 0..6 {
        status.record_failure(&format!("failure {index}"));
    }
    wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
        line_count(&alert_log) == 2
    });

    write_config(&config_path, &script_path, 1);
    wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
        ConfigHandle::version() != previous_version
    });

    status.record_failure(&"one-more-failure");
    wait_until(Duration::from_secs(3), Duration::from_millis(50), || {
        line_count(&alert_log) == 3
    });

    status.reset_for_tests();
}
