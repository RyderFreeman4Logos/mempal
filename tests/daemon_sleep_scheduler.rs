mod common;

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::harness::CapturedChild;
use mempal::core::db::Database;

fn spawn_daemon(home: &Path, log_path: &Path) -> CapturedChild {
    spawn_daemon_with_cycle_block(home, log_path, None)
}

fn spawn_daemon_with_cycle_block(
    home: &Path,
    log_path: &Path,
    cycle_block_path: Option<&Path>,
) -> CapturedChild {
    let mut command = Command::new(env!("CARGO_BIN_EXE_mempal"));
    command
        .args(["daemon", "--foreground"])
        .env("HOME", home)
        .env(
            mempal::daemon_singleton::MEMPAL_RUNTIME_DIR_ENV,
            home.join(".mempal/runtime"),
        )
        .stdin(Stdio::null());
    if let Some(cycle_block_path) = cycle_block_path {
        command.env("MEMPAL_TEST_SLEEP_CYCLE_BLOCK_FILE", cycle_block_path);
    }
    CapturedChild::spawn(
        &mut command,
        &home.join("diagnostics"),
        "embedded-sleep-scheduler",
        Some(log_path.to_path_buf()),
    )
    .expect("spawn foreground daemon")
}

fn wait_for_scheduled_phase(child: &mut CapturedChild, db_path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll daemon") {
            panic!(
                "daemon exited before its scheduled sleep cycle: {status}\n{}",
                child.diagnostics()
            );
        }
        let phase = Database::open(db_path).ok().and_then(|db| {
            db.conn()
                .query_row(
                    "SELECT phase FROM sleep_log ORDER BY created_at DESC, id DESC LIMIT 1",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .ok()
        });
        if let Some(phase) = phase {
            return phase;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "daemon did not run a scheduled sleep cycle\n{}",
        child.diagnostics()
    );
}

fn wait_for_diagnostic(child: &mut CapturedChild, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll daemon") {
            panic!(
                "daemon exited before diagnostic marker `{marker}`: {status}\n{}",
                child.diagnostics()
            );
        }
        if child.diagnostics().contains(marker) {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "daemon did not emit diagnostic marker `{marker}`\n{}",
        child.diagnostics()
    );
}

fn wait_for_daemon_exit(
    child: &mut CapturedChild,
    timeout: Duration,
) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if let Some(status) = child.try_wait().expect("poll daemon shutdown") {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    None
}

fn stop_daemon(child: &mut CapturedChild) {
    child.signal_or_panic(libc::SIGTERM, "signal daemon");
    let status = wait_for_daemon_exit(child, Duration::from_secs(10))
        .unwrap_or_else(|| panic!("daemon did not stop after SIGTERM\n{}", child.diagnostics()));
    assert!(status.success(), "daemon shutdown failed: {status}");
}

#[cfg(unix)]
#[test]
fn daemon_runs_configured_sleep_cycle_when_hooks_are_disabled() {
    let home = tempfile::TempDir::new_in("/tmp").expect("short temporary home");
    let mempal_home = home.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let log_path = mempal_home.join("daemon.log");
    Database::open(&db_path).expect("initialize database");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"db_path = "{}"

[api]
addr = "127.0.0.1:0"

[embed]
backend = "stub"

[hooks]
enabled = false

[daemon]
log_path = "{}"

[sleep]
auto_interval_secs = 1
phases = ["salience"]
"#,
            db_path.display(),
            log_path.display()
        ),
    )
    .expect("write daemon config");

    let mut daemon = spawn_daemon(home.path(), &log_path);
    let phase = wait_for_scheduled_phase(&mut daemon, &db_path);
    assert_eq!(phase, "salience");

    stop_daemon(&mut daemon);
}

#[cfg(unix)]
#[test]
fn daemon_sigterm_is_bounded_during_blocked_sleep_cycle() {
    let home = tempfile::TempDir::new_in("/tmp").expect("short temporary home");
    let mempal_home = home.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let log_path = mempal_home.join("daemon.log");
    let cycle_block_path = home.path().join("block-sleep-cycle");
    Database::open(&db_path).expect("initialize database");
    fs::write(&cycle_block_path, "blocked").expect("create sleep cycle block file");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"db_path = "{}"

[api]
addr = "127.0.0.1:0"

[embed]
backend = "stub"

[hooks]
enabled = false

[daemon]
log_path = "{}"

[sleep]
auto_interval_secs = 1
phases = ["salience"]
"#,
            db_path.display(),
            log_path.display()
        ),
    )
    .expect("write daemon config");

    let mut daemon = spawn_daemon_with_cycle_block(home.path(), &log_path, Some(&cycle_block_path));
    wait_for_diagnostic(&mut daemon, "daemon embedded sleep cycle started");

    daemon.signal_or_panic(libc::SIGTERM, "signal daemon during sleep cycle");
    let status = wait_for_daemon_exit(&mut daemon, Duration::from_secs(3));
    let diagnostics = daemon.diagnostics();
    fs::remove_file(&cycle_block_path).expect("release blocked sleep cycle");

    let status = status.unwrap_or_else(|| {
        panic!("daemon did not bound the active sleep cycle after SIGTERM\n{diagnostics}")
    });
    assert!(status.success(), "daemon shutdown failed: {status}");
}

#[cfg(unix)]
#[test]
fn daemon_runs_configured_sleep_cycle_under_its_writer_lease() {
    let home = tempfile::TempDir::new_in("/tmp").expect("short temporary home");
    let mempal_home = home.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let log_path = mempal_home.join("daemon.log");
    Database::open(&db_path).expect("initialize database");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"db_path = "{}"

[api]
addr = "127.0.0.1:0"

[embed]
backend = "stub"

[hooks]
enabled = true
daemon_poll_interval_ms = 50

[daemon]
log_path = "{}"

[sleep]
auto_interval_secs = 1
phases = ["salience"]
"#,
            db_path.display(),
            log_path.display()
        ),
    )
    .expect("write daemon config");

    let mut daemon = spawn_daemon(home.path(), &log_path);
    let phase = wait_for_scheduled_phase(&mut daemon, &db_path);
    assert_eq!(phase, "salience");

    let db = Database::open(&db_path).expect("reopen database");
    let leases = db
        .runtime_writer_lease_status(Some("sqlite-writer"))
        .expect("read daemon writer lease");
    assert_eq!(
        leases.len(),
        1,
        "scheduled cycle must reuse the daemon lease"
    );
    assert_eq!(leases[0].mode, "daemon");

    stop_daemon(&mut daemon);
}
