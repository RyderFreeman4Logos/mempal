#[path = "common/harness/admission_supervisor.rs"]
mod admission_supervisor;
mod common;

use std::fs;
use std::process::{Command, Stdio};
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use admission_supervisor::DeadlineChild;

use mempal::core::AsyncDb;
use mempal::core::db::Database;
use mempal::core::db_admission::{
    DbAdmissionConfig, DbAdmissionError, DbAdmissionRequest, DbHolderClass, ProfileDbAdmission,
};

const MIB: u64 = 1024 * 1024;
fn request(class: DbHolderClass, cache_mib: u64) -> DbAdmissionRequest {
    DbAdmissionRequest::new(class, 1, cache_mib * MIB)
}

#[test]
fn exact_profile_budget_is_admitted_and_excess_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config = DbAdmissionConfig::new(2, 64 * MIB);

    let first = ProfileDbAdmission::acquire_with_config(
        &db_path,
        request(DbHolderClass::Daemon, 32),
        config,
    )
    .expect("first holder");
    let second =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Mcp, 32), config)
            .expect("exact holder/cache budget");

    let error =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Cli, 1), config)
            .expect_err("holder over budget must fail");
    assert!(matches!(
        error,
        DbAdmissionError::BudgetExceeded {
            active_holders: 2,
            requested_cache_bytes: MIB,
            ..
        }
    ));

    let snapshot =
        ProfileDbAdmission::snapshot_with_config(&db_path, config).expect("admission snapshot");
    assert_eq!(snapshot.active_holders, 2);
    assert_eq!(snapshot.configured_cache_bytes, 64 * MIB);
    assert_eq!(snapshot.holders[0].generation, first.generation());
    assert_eq!(snapshot.holders[1].generation, second.generation());
}

#[test]
fn dropping_holder_returns_profile_capacity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config = DbAdmissionConfig::new(1, 16 * MIB);

    let first =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Api, 16), config)
            .expect("first holder");
    let first_generation = first.generation();
    drop(first);

    let replacement =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Hook, 16), config)
            .expect("capacity after release");
    assert!(replacement.generation() > first_generation);
}

#[test]
fn concurrent_registration_never_oversubscribes_profile_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = Arc::new(tmp.path().join("palace.db"));
    let start = Arc::new(Barrier::new(5));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let config = DbAdmissionConfig::new(2, 32 * MIB);
    let mut workers = Vec::new();

    for _ in 0..4 {
        let db_path = Arc::clone(&db_path);
        let start = Arc::clone(&start);
        let release = Arc::clone(&release);
        let admitted_tx = admitted_tx.clone();
        workers.push(std::thread::spawn(move || {
            start.wait();
            let admission = ProfileDbAdmission::acquire_with_config(
                &db_path,
                request(DbHolderClass::Cli, 16),
                config,
            );
            admitted_tx
                .send(admission.is_ok())
                .expect("report admission");
            if admission.is_ok() {
                let (released, signal) = &*release;
                let mut released = released.lock().expect("release lock");
                while !*released {
                    released = signal.wait(released).expect("release signal");
                }
            }
            admission.is_ok()
        }));
    }
    drop(admitted_tx);

    start.wait();
    let reported = (0..4)
        .map(|_| {
            admitted_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("worker admission result")
        })
        .filter(|admitted| *admitted)
        .count();
    let (released, signal) = &*release;
    *released.lock().expect("release lock") = true;
    signal.notify_all();
    let admitted = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker"))
        .filter(|admitted| *admitted)
        .count();
    assert_eq!(reported, 2);
    assert_eq!(admitted, 2);
}

#[test]
fn async_pool_holds_admission_for_its_full_lifetime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let pool = AsyncDb::open_for(&db_path, 2, DbHolderClass::Mcp).expect("async pool");

    let active = ProfileDbAdmission::snapshot(&db_path).expect("active snapshot");
    assert_eq!(
        active.active_holders,
        1,
        "pool={:?}",
        pool.resource_snapshot()
    );
    assert_eq!(active.holders[0].holder_class, DbHolderClass::Mcp);

    drop(pool);
    assert_eq!(
        ProfileDbAdmission::snapshot(&db_path)
            .expect("released snapshot")
            .active_holders,
        0
    );
}

#[cfg(target_os = "linux")]
#[test]
fn deadline_child_bounds_non_exiting_fixture_with_inherited_pipes() {
    let started = Instant::now();
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "trap '' TERM; printf ready; while :; do sleep 60; done",
    ]);
    let output = DeadlineChild::output(command, Duration::from_secs(1))
        .expect("run non-exiting inherited-pipe fixture");

    assert_eq!(output.stdout, b"ready");
    assert!(output.timed_out, "non-exiting fixture must reach deadline");
    assert!(
        output.cleanup.errors.is_empty(),
        "{:#?}",
        output.cleanup.errors
    );
    assert!(!output.cleanup.transferred_to_background_reaper);
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "subprocess cleanup exceeded its deadline"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn deadline_child_retains_tail_diagnostics_after_capture_limit() {
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "head -c 1100000 /dev/zero | tr '\\0' x; printf '\\nTAIL_MARKER\\n'",
    ]);
    let output = DeadlineChild::output(command, Duration::from_secs(5))
        .expect("capture bounded diagnostic output");

    assert!(output.success(), "fixture must complete successfully");
    assert!(
        output.stdout_truncated,
        "capture metadata must report truncation"
    );
    assert!(
        output.stdout_total_bytes > output.stdout.len(),
        "capture metadata must retain the full observed byte count"
    );
    assert!(!output.stderr_truncated);
    assert_eq!(output.stderr_total_bytes, 0);
    assert!(
        output.cleanup.errors.is_empty(),
        "{:#?}",
        output.cleanup.errors
    );
    assert!(!output.cleanup.transferred_to_background_reaper);
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("TAIL_MARKER"),
        "bounded capture must retain a tail diagnostic marker"
    );
}

#[test]
fn status_remains_available_when_holder_budget_is_exhausted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    drop(Database::open(&db_path).expect("initialize database"));
    let holders = (0..16)
        .map(|_| {
            ProfileDbAdmission::acquire(&db_path, DbAdmissionRequest::new(DbHolderClass::Mcp, 1, 1))
                .expect("fill holder budget")
        })
        .collect::<Vec<_>>();

    let mut command = Command::new(env!("CARGO_BIN_EXE_mempal"));
    command.arg("status").env("HOME", &home).current_dir(&home);
    let output =
        DeadlineChild::output(command, Duration::from_secs(5)).expect("run status at holder cap");

    assert!(
        output.success(),
        "status must remain diagnostic at holder cap: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("status stdout UTF-8");
    assert!(
        stdout.contains("holders: 16/16"),
        "status must report the exhausted admission budget: {stdout}"
    );
    assert!(
        stdout.contains("reaped_stale_holders_this_snapshot: 0")
            && stdout.contains("unknown_holders: 0"),
        "status must expose stale and unknown holder diagnostics: {stdout}"
    );

    drop(holders);
}

#[cfg(target_os = "linux")]
#[test]
fn pid_namespace_mcp_holder_is_reaped_after_forced_exit_when_supported() {
    let mut support_command = Command::new("unshare");
    support_command.args([
        "--user",
        "--map-root-user",
        "--pid",
        "--fork",
        "--kill-child=SIGKILL",
        "--mount-proc",
        "true",
    ]);
    let support = DeadlineChild::output(support_command, Duration::from_secs(5));
    match support {
        Ok(output) if output.success() => {}
        Ok(output)
            if output
                .stderr
                .windows(b"Operation not permitted".len())
                .any(|window| window == b"Operation not permitted")
                || output
                    .stderr
                    .windows(b"not permitted".len())
                    .any(|window| window == b"not permitted") =>
        {
            eprintln!("skipping PID namespace integration probe: user namespaces are unavailable");
            return;
        }
        Ok(output) => panic!(
            "unshare capability probe failed without a recognized unsupported result: status={:?} stdout={} stderr={} cleanup={:?}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
            output.cleanup
        ),
        Err(error) => panic!("unshare capability probe failed: {error}"),
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    drop(Database::open(&db_path).expect("initialize database"));

    let mut command = Command::new("unshare");
    command
        .args([
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--kill-child=SIGKILL",
            "--mount-proc",
        ])
        .arg(env!("CARGO_BIN_EXE_mempal"))
        .args(["serve", "--mcp"])
        .env("HOME", &home)
        .current_dir(&home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = DeadlineChild::spawn(command, Duration::from_secs(5))
        .expect("spawn namespaced MCP fixture");
    child
        .write_stdin(
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"pid-namespace-test","version":"0.0.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mempal_status","arguments":{}}}
"#,
        Instant::now() + Duration::from_secs(5),
    )
    .expect("write namespaced child stdin");
    let live_deadline = Instant::now() + Duration::from_secs(10);
    let live_snapshot = loop {
        if let Some(diagnostic) = child.exit_diagnostic() {
            panic!("namespaced MCP fixture exited before registration: {diagnostic}");
        }
        if let Ok(snapshot) = ProfileDbAdmission::snapshot(&db_path)
            && snapshot.active_holders > 0
        {
            break snapshot;
        }
        assert!(
            Instant::now() < live_deadline,
            "namespaced MCP fixture did not register before deadline"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(live_snapshot.reaped_stale_holders_this_snapshot, 0);
    assert_eq!(live_snapshot.unknown_holders, 0);

    let cleanup = child.force_kill();
    assert!(cleanup.errors.is_empty(), "cleanup errors: {cleanup:?}");
    assert!(
        !cleanup.transferred_to_background_reaper,
        "fixture cleanup must reap the direct child synchronously"
    );

    let reap_deadline = Instant::now() + Duration::from_secs(10);
    let mut reaped_total = 0usize;
    loop {
        if let Ok(snapshot) = ProfileDbAdmission::snapshot(&db_path) {
            reaped_total = reaped_total.saturating_add(snapshot.reaped_stale_holders_this_snapshot);
            if snapshot.active_holders == 0 {
                break;
            }
        }
        assert!(
            Instant::now() < reap_deadline,
            "namespaced MCP holder was not reaped before deadline"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        reaped_total > 0,
        "forced child exit must reap its holder lease"
    );
}
