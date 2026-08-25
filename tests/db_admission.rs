#[path = "common/harness/admission_supervisor.rs"]
mod admission_supervisor;
mod common;

use std::fs;
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use admission_supervisor::{
    DeadlineChild, DeadlineOutput, IncompleteCleanup, LeaderResourceState, SpawnSpec, StdioMode,
    SupervisionError,
};

use mempal::core::AsyncDb;
use mempal::core::db::Database;
use mempal::core::db_admission::{
    DbAdmissionConfig, DbAdmissionError, DbAdmissionRequest, DbHolderClass, ProfileDbAdmission,
};

const MIB: u64 = 1024 * 1024;
/// Event-driven collection cap for admission fixtures under suite load.
/// Matches `cli_deadline::CLI_HELPER_DEADLINE`; wait returns on child exit.
const ADMISSION_FIXTURE_OUTPUT_BOUND: Duration = Duration::from_secs(30);
const CLEANUP_RETRY: Duration = Duration::from_secs(5);

fn request(class: DbHolderClass, cache_mib: u64) -> DbAdmissionRequest {
    DbAdmissionRequest::new(class, 1, cache_mib * MIB)
}

/// Wait for a supervised child to complete, then reap it.
///
/// Success returns the captured output. Timeout or incomplete cleanup
/// returns a contextual error instead of a bare empty-stdout / Timeout panic.
fn wait_for_deadline_output(
    spec: SpawnSpec,
    timeout: Duration,
    label: &str,
) -> Result<DeadlineOutput, String> {
    let started = Instant::now();
    let output = match DeadlineChild::output(spec, timeout) {
        Ok(output) => output,
        Err(SupervisionError::CleanupIncomplete(incomplete)) => {
            match incomplete.finish_output(CLEANUP_RETRY) {
                Ok(output) => output,
                Err(still) => return Err(format_incomplete(label, started.elapsed(), &still)),
            }
        }
        Err(error) => {
            return Err(format!(
                "{label} failed after {:?}: {error}",
                started.elapsed()
            ));
        }
    };
    if output.success() && !output.timed_out {
        return Ok(output);
    }
    Err(format_output_failure(label, started.elapsed(), &output))
}

fn format_output_failure(label: &str, elapsed: Duration, output: &DeadlineOutput) -> String {
    let kind = if output.timed_out {
        "timed out"
    } else {
        "failed"
    };
    format!(
        "{label} {kind} after {elapsed:?}: success={} timed_out={} status={:?} stdout={} stderr={} kill_fence={} term_grace_expired={} cleanup_errors={}",
        output.success(),
        output.timed_out,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
        output.cleanup.kill_fence_sent,
        output.cleanup.term_grace_expired,
        output.cleanup.errors.len(),
    )
}

fn format_incomplete(label: &str, elapsed: Duration, incomplete: &IncompleteCleanup) -> String {
    format!(
        "{label} timed out after {elapsed:?}: cleanup incomplete resources={:?} kill_fence={} term_grace_expired={} cleanup_errors={}",
        incomplete.resources,
        incomplete.report.kill_fence_sent,
        incomplete.report.term_grace_expired,
        incomplete.report.errors.len(),
    )
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_output_wait_returns_completed_child_output() {
    let mut command = SpawnSpec::new("/bin/sh").expect("absolute shell");
    command.args(["-c", "printf ready"]);
    let output = wait_for_deadline_output(command, Duration::from_secs(2), "ready fixture")
        .expect("ready fixture should complete");
    assert_eq!(output.stdout, b"ready");
    assert!(output.success());
    assert!(!output.timed_out);
    assert!(output.cleanup.errors.is_empty());
    assert!(output.cleanup.kill_fence_sent);
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_output_wait_returns_contextual_timeout_and_reaps() {
    let mut command = SpawnSpec::new("/bin/sh").expect("absolute shell");
    command.args([
        "-c",
        "trap '' TERM; printf started; while :; do sleep 60; done",
    ]);
    let err = wait_for_deadline_output(command, Duration::from_millis(150), "hanging fixture")
        .expect_err("hanging fixture must time out");
    assert!(
        err.contains("hanging fixture"),
        "timeout must name the wait: {err}"
    );
    assert!(
        err.contains("timed out"),
        "timeout must be a contextual failure, not a bare Timeout panic: {err}"
    );
    assert!(
        err.contains("kill_fence") || err.contains("cleanup"),
        "timeout must report cleanup/reap context: {err}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_output_wait_rejects_term_trap_exit_zero_after_deadline() {
    let mut command = SpawnSpec::new("/bin/sh").expect("absolute shell");
    command.args(["-c", "trap 'exit 0' TERM; while :; do sleep 60; done"]);
    let err = wait_for_deadline_output(command, Duration::from_millis(150), "term-trap fixture")
        .expect_err("term-trap exit 0 after deadline must be Err");
    assert!(
        err.contains("term-trap fixture"),
        "timeout must name the wait: {err}"
    );
    assert!(
        err.contains("timed out"),
        "timeout provenance must be preserved: {err}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_output_wait_reports_immediate_nonzero_as_failed() {
    let command = SpawnSpec::new("/bin/false").expect("absolute false");
    let err = wait_for_deadline_output(command, Duration::from_secs(2), "false fixture")
        .expect_err("immediate /bin/false must fail");
    assert!(
        err.contains("false fixture"),
        "failure must name the wait: {err}"
    );
    assert!(
        err.contains("failed"),
        "non-timeout failure must say failed: {err}"
    );
    assert!(
        !err.contains("timed out"),
        "non-timeout failure must not say timed out: {err}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn bounded_output_wait_reports_missing_executable_as_failed() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let command = SpawnSpec::new(tmp.path().join("missing-bin"))
        .expect("missing absolute path is accepted by SpawnSpec");
    let err = wait_for_deadline_output(command, Duration::from_secs(2), "missing fixture")
        .expect_err("missing executable must fail");
    assert!(
        err.contains("missing fixture"),
        "failure must name the wait: {err}"
    );
    assert!(
        err.contains("failed"),
        "non-timeout spawn/setup failure must say failed: {err}"
    );
    assert!(
        !err.contains("timed out"),
        "non-timeout spawn/setup failure must not say timed out: {err}"
    );
}

#[test]
fn exact_profile_budget_is_admitted_and_excess_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    // Disable reserved seats for this exact-fill contract.
    let config = DbAdmissionConfig::new(2, 64 * MIB).with_reserved_service_holders(0);

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
            reaped_stale_holders: 0,
            reason: mempal::core::db_admission::BudgetExceededReason::HolderLimit,
            ..
        }
    ));

    let snapshot =
        ProfileDbAdmission::snapshot_with_config(&db_path, config).expect("admission snapshot");
    assert_eq!(snapshot.active_holders, 2);
    assert_eq!(snapshot.configured_cache_bytes, 64 * MIB);
    assert_eq!(snapshot.service_holders, 2);
    assert_eq!(snapshot.holders[0].generation, first.generation());
    assert_eq!(snapshot.holders[1].generation, second.generation());
}

#[test]
fn dropping_holder_returns_profile_capacity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config = DbAdmissionConfig::new(1, 16 * MIB).with_reserved_service_holders(0);

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
fn reserved_service_slots_admit_mcp_after_transient_fill() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    // max=3, reserve=1: two transient holders fill the non-reserved seats; MCP
    // can still open because the reserved service seat remains available.
    let config = DbAdmissionConfig::new(3, 64 * MIB).with_reserved_service_holders(1);
    let first =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Cli, 1), config)
            .expect("first transient");
    let second =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Hook, 1), config)
            .expect("second transient");
    let refused =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Api, 1), config)
            .expect_err("transient must not consume reserved service seat");
    assert!(matches!(
        refused,
        DbAdmissionError::BudgetExceeded {
            reason: mempal::core::db_admission::BudgetExceededReason::ReservedServiceSlots,
            reaped_stale_holders: 0,
            reserved_service_holders: 1,
            ..
        }
    ));
    let mcp =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Mcp, 1), config)
            .expect("MCP must open reserved service seat");
    assert_eq!(mcp.generation(), 3);
    drop((first, second, mcp));
}

#[test]
fn stale_holders_are_reaped_before_budget_check_and_service_opens() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config = DbAdmissionConfig::new(2, 32 * MIB).with_reserved_service_holders(0);

    // Fill the budget with live leases, then drop them without release by
    // overwriting state to dead leases via the same path used by crash tests:
    // acquire two holders, drop lease files while keeping state entries via
    // force-writing dead tokens is covered in unit tests. Here we prove the
    // end-to-end acquire path reaps then admits MCP after live capacity was full.
    let holders = (0..2)
        .map(|_| {
            ProfileDbAdmission::acquire_with_config(
                &db_path,
                request(DbHolderClass::Cli, 1),
                config,
            )
            .expect("fill holder budget")
        })
        .collect::<Vec<_>>();
    // Live holders still block.
    let live_block =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Mcp, 1), config)
            .expect_err("live holders must refuse");
    assert!(matches!(
        live_block,
        DbAdmissionError::BudgetExceeded {
            active_holders: 2,
            reason: mempal::core::db_admission::BudgetExceededReason::HolderLimit,
            reaped_stale_holders: 0,
            ..
        }
    ));
    drop(holders);

    let mcp =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Mcp, 1), config)
            .expect("released capacity must reopen for MCP");
    let snapshot =
        ProfileDbAdmission::snapshot_with_config(&db_path, config).expect("post-open snapshot");
    assert_eq!(snapshot.active_holders, 1);
    assert_eq!(snapshot.service_holders, 1);
    assert_eq!(snapshot.reserved_service_holders, 0);
    drop(mcp);
}

#[test]
fn concurrent_registration_never_oversubscribes_profile_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = Arc::new(tmp.path().join("palace.db"));
    let start = Arc::new(Barrier::new(5));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let config = DbAdmissionConfig::new(2, 32 * MIB).with_reserved_service_holders(0);
    let mut workers = Vec::new();

    for _ in 0..4 {
        let db_path = Arc::clone(&db_path);
        let start = Arc::clone(&start);
        let release = Arc::clone(&release);
        let admitted_tx = admitted_tx.clone();
        workers.push(std::thread::spawn(move || {
            start.wait();
            // Under suite load the 250ms admission flock wait can expire as Busy
            // before a budget decision is reached. Busy is lock contention, not
            // a budget rejection; retry until Ok/BudgetExceeded within a bound.
            let deadline = Instant::now() + Duration::from_secs(2);
            let admission = loop {
                match ProfileDbAdmission::acquire_with_config(
                    &db_path,
                    request(DbHolderClass::Cli, 16),
                    config,
                ) {
                    Ok(holder) => break Ok(holder),
                    Err(DbAdmissionError::Busy { .. }) if Instant::now() < deadline => {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => break Err(error),
                }
            };
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
                .recv_timeout(std::time::Duration::from_secs(5))
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
    let mut command = SpawnSpec::new("/bin/sh").expect("absolute shell");
    command.args([
        "-c",
        "trap '' TERM; printf ready; while :; do sleep 60; done",
    ]);
    let output = DeadlineChild::output(command, Duration::from_secs(5))
        .expect("run non-exiting inherited-pipe fixture");

    assert_eq!(output.stdout, b"ready");
    assert!(output.timed_out, "non-exiting fixture must reach deadline");
    assert!(
        output.cleanup.errors.is_empty(),
        "{:#?}",
        output.cleanup.errors
    );
    assert!(output.cleanup.kill_fence_sent);
    assert!(
        started.elapsed() < Duration::from_secs(15),
        "subprocess cleanup exceeded its deadline"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn deadline_child_retains_tail_diagnostics_after_capture_limit() {
    let mut command = SpawnSpec::new("/bin/sh").expect("absolute shell");
    command.args([
        "-c",
        "head -c 1100000 /dev/zero | tr '\\0' x; printf '\\nTAIL_MARKER\\n'",
    ]);
    let output = wait_for_deadline_output(
        command,
        ADMISSION_FIXTURE_OUTPUT_BOUND,
        "capture bounded diagnostic output",
    )
    .unwrap_or_else(|error| panic!("{error}"));

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
    assert!(output.cleanup.kill_fence_sent);
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
    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("snapshot at holder cap");
    assert_eq!(snapshot.active_holders, 16);

    let mut command =
        SpawnSpec::new(env!("CARGO_BIN_EXE_mempal")).expect("absolute mempal executable");
    command.arg("status").env("HOME", &home);
    command.current_dir(&home).expect("absolute home directory");
    let output = wait_for_deadline_output(
        command,
        ADMISSION_FIXTURE_OUTPUT_BOUND,
        "status at holder cap",
    )
    .unwrap_or_else(|error| panic!("{error}"));

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
            && stdout.contains("unknown_holders: 0")
            && stdout.contains("reserved_service_holders="),
        "status must expose stale, reserved, and unknown holder diagnostics: {stdout}"
    );

    drop(holders);
}

#[cfg(target_os = "linux")]
#[test]
fn pid_namespace_mcp_holder_is_reaped_after_forced_exit_when_supported() {
    let Ok(mut support_command) = SpawnSpec::resolve("unshare") else {
        eprintln!("skipping PID namespace integration probe: unshare is unavailable");
        return;
    };
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

    let mut command = SpawnSpec::resolve("unshare").expect("resolved unshare executable");
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
        .stdio(StdioMode::PipedInput);
    command.current_dir(&home).expect("absolute home directory");
    let mut child = DeadlineChild::spawn(command, Duration::from_secs(5))
        .expect("spawn namespaced MCP fixture");
    child
        .write_stdin(
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"pid-namespace-test","version":"0.0.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mempal_status","arguments":{}}}
"#,
        Duration::from_secs(5),
    )
    .expect("write namespaced child stdin");
    let live_deadline = Instant::now() + Duration::from_secs(30);
    let live_snapshot = loop {
        let diagnostic = child.exit_diagnostic().expect("inspect namespaced fixture");
        if child.resources().leader != LeaderResourceState::Running {
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
        // Bounded sleep instead of yield_now: under high host load a busy-spin
        // starves the fixture process we are waiting for. Matches the readiness
        // probe pattern from #825 (tests/write_wait_cli/ipc.rs).
        let remaining = live_deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(Duration::from_millis(50).min(remaining));
    };
    assert_eq!(live_snapshot.reaped_stale_holders_this_snapshot, 0);
    assert_eq!(live_snapshot.unknown_holders, 0);

    let cleanup = child
        .force_kill()
        .expect_complete("kill namespaced MCP fixture");
    assert!(cleanup.errors.is_empty(), "cleanup errors: {cleanup:?}");
    assert!(cleanup.kill_fence_sent);

    let reap_deadline = Instant::now() + Duration::from_secs(30);
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
        let remaining = reap_deadline.saturating_duration_since(Instant::now());
        std::thread::sleep(Duration::from_millis(50).min(remaining));
    }
    assert!(
        reaped_total > 0,
        "forced child exit must reap its holder lease"
    );
}
