use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::db_admission::{
    AdmissionPaths, DbAdmissionConfig, DbAdmissionError, DbAdmissionRequest, DbHolderClass,
    ProfileDbAdmission,
};
use super::db_admission_fault_injection::{self as fault_injection, CrashPoint};
use super::db_admission_lease::fail_next_holder_lease_unlink;

const _: fn() = db_admission_test_process::reference_shared_test_api;

use super::db_admission_test_process::{
    self, DeadlineChild, SpawnSpec, SupervisionError, TestSetupGate,
};

const FIXTURE_CASE_ENV: &str = "MEMPAL_DB_ADMISSION_FIXTURE_CASE";
const FIXTURE_DATABASE_ENV: &str = "MEMPAL_DB_ADMISSION_FIXTURE_DATABASE";
const PRE_CLOSE_PARENT_CRASH_LOCK_ENV: &str = "MEMPAL_PRE_CLOSE_PARENT_CRASH_LOCK";
const FIXTURE_TEST: &str = "core::db_admission_crash_tests::admission_crash_fixture";
const PARENT_CRASH_EXIT_CODE: i32 = 86;

#[test]
fn admission_crash_fixture() {
    if let Some(lock_path) = std::env::var_os(PRE_CLOSE_PARENT_CRASH_LOCK_ENV) {
        run_pre_close_parent_crash_fixture(PathBuf::from(lock_path));
    }
    let Some(case) = std::env::var_os(FIXTURE_CASE_ENV) else {
        return;
    };
    let database =
        PathBuf::from(std::env::var_os(FIXTURE_DATABASE_ENV).expect("fixture database path"));
    let point = crash_point_for_case(case.to_str().expect("UTF-8 fixture case"));
    let _crash_guard = fault_injection::arm(point);
    match point {
        CrashPoint::LeaseCreatedBeforeStatePublish | CrashPoint::StateTempSyncedBeforeRename => {
            let _admission = ProfileDbAdmission::acquire(
                &database,
                DbAdmissionRequest::new(DbHolderClass::Mcp, 1, 1024),
            )
            .expect("fixture admission acquire");
        }
        CrashPoint::ReleaseStateSavedBeforeLeaseUnlink => {
            let admission = ProfileDbAdmission::acquire(
                &database,
                DbAdmissionRequest::new(DbHolderClass::Mcp, 1, 1024),
            )
            .expect("fixture admission acquire before release");
            admission.release().expect("fixture admission release");
        }
        CrashPoint::ReapStateSavedBeforeOrphanSweep => {
            ProfileDbAdmission::snapshot(&database).expect("fixture admission snapshot");
        }
    }
    panic!("configured crash point {point:?} was not reached");
}

#[test]
fn fork_does_not_wait_for_unrelated_admission_state_lock() {
    let temp = tempfile::tempdir().expect("temp dir");
    let lock_path = temp.path().join(".palace.db.admission.lock");
    let state_lock = super::db_admission_state::lock_state(&lock_path)
        .expect("lock admission state before fork");
    let (gate, child_gate) = TestSetupGate::new().expect("create setup gate");

    std::thread::scope(|scope| {
        let worker = scope.spawn(move || {
            let mut spec = SpawnSpec::new("/bin/true").expect("absolute true executable");
            spec.setup_gate(child_gate);
            DeadlineChild::output(spec, Duration::from_secs(2))
        });
        let ready_pid = gate
            .wait_ready(Instant::now() + Duration::from_secs(1))
            .expect("unrelated admission lock must not delay child setup");
        gate.release().expect("release setup gate");
        let output = worker
            .join()
            .expect("launch worker")
            .expect("released child output");
        assert_eq!(output.identity.pid, ready_pid);
        assert!(output.success());
        assert!(!output.timed_out);
        assert!(output.cleanup.kill_fence_sent);
        assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
        let mut status = 0;
        // SAFETY: successful output means the supervisor already reaped this direct child; the
        // WNOHANG probe verifies ownership cleanup without blocking or reaping another process.
        assert_eq!(
            unsafe { libc::waitpid(ready_pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    });
    drop(state_lock);
}

#[test]
fn released_state_lock_does_not_remain_held_by_pre_close_child() {
    let temp = tempfile::tempdir().expect("temp dir");
    let lock_path = temp.path().join(".palace.db.admission.lock");
    let state_lock = super::db_admission_state::lock_state(&lock_path)
        .expect("lock admission state before fork");
    let (gate, child_gate) = TestSetupGate::new().expect("create pre-close gate");

    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let mut spec = SpawnSpec::new("/bin/true").expect("absolute true executable");
            spec.pre_close_gate(child_gate);
            DeadlineChild::output(spec, Duration::from_secs(2))
        });
        gate.wait_ready(Instant::now() + Duration::from_millis(500))
            .expect("child stopped before inherited descriptors close");

        drop(state_lock);
        drop(
            super::db_admission_state::lock_state(&lock_path)
                .expect("released state lock must be acquirable while child remains stopped"),
        );

        gate.release().expect("release pre-close child");
        let output = worker
            .join()
            .expect("launch worker")
            .expect("released child output");
        assert!(output.success());
        assert!(!output.timed_out);
        assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
    });
}

#[test]
fn raw_close_negative_control_retains_lock_until_pre_close_child_exits() {
    let temp = tempfile::tempdir().expect("temp dir");
    let lock_path = temp.path().join(".palace.db.admission.lock");
    let raw_lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lock_path)
        .expect("open raw lock");
    // SAFETY: raw_lock owns this descriptor until it is dropped below.
    assert_eq!(
        unsafe { libc::flock(raw_lock.as_raw_fd(), libc::LOCK_EX) },
        0
    );
    let (gate, child_gate) = TestSetupGate::new().expect("create pre-close gate");

    std::thread::scope(|scope| {
        let worker = scope.spawn(|| {
            let mut spec = SpawnSpec::new("/bin/true").expect("absolute true executable");
            spec.pre_close_gate(child_gate);
            DeadlineChild::output(spec, Duration::from_secs(2))
        });
        gate.wait_ready(Instant::now() + Duration::from_millis(500))
            .expect("child stopped before inherited descriptors close");

        drop(raw_lock);
        assert!(matches!(
            super::db_admission_state::lock_state(&lock_path),
            Err(DbAdmissionError::Busy {
                timeout_ms: 250,
                ..
            })
        ));

        gate.release().expect("release pre-close child");
        let output = worker
            .join()
            .expect("launch worker")
            .expect("released child output");
        assert!(output.success());
        assert!(!output.timed_out);
        assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
    });

    drop(
        super::db_admission_state::lock_state(&lock_path)
            .expect("child exit releases the inherited raw lock descriptor"),
    );
}

#[test]
fn parent_crash_releases_inherited_pre_close_state_lock() {
    let temp = tempfile::tempdir().expect("temp dir");
    let lock_path = temp.path().join(".palace.db.admission.lock");
    let executable = std::env::current_exe().expect("current unit-test executable");
    let mut spec = SpawnSpec::new(executable).expect("absolute unit-test executable");
    spec.args(["--exact", FIXTURE_TEST, "--nocapture", "--test-threads=1"])
        .env(PRE_CLOSE_PARENT_CRASH_LOCK_ENV, lock_path.as_os_str());

    let output = DeadlineChild::output(spec, Duration::from_secs(6))
        .expect("run pre-close parent-crash fixture");
    assert_eq!(output.status.code(), Some(PARENT_CRASH_EXIT_CODE));
    assert!(!output.timed_out);
    assert!(output.cleanup.kill_fence_sent);
    assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
    drop(
        super::db_admission_state::lock_state(&lock_path)
            .expect("owned cleanup releases the crashed parent's inherited lock"),
    );
}

#[test]
fn crash_after_lease_creation_before_state_publish_reclaims_orphan() {
    let _fixture_guard = super::db::db_open_busy_fixture_lock().blocking_lock();
    let temp = tempfile::tempdir().expect("temp dir");
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");

    assert_crashes_at(&database, CrashPoint::LeaseCreatedBeforeStatePublish);
    assert_eq!(state_holder_count(&paths), 0);
    assert_eq!(lease_paths(&paths).len(), 1, "crash must strand one lease");

    let snapshot = ProfileDbAdmission::snapshot(&database).expect("recover orphaned lease");
    assert_eq!(snapshot.active_holders, 0);
    assert_eq!(snapshot.reaped_stale_holders_this_snapshot, 0);
    assert!(lease_paths(&paths).is_empty());
    assert_capacity_reusable(&database);
}

#[test]
fn crash_after_state_temp_sync_before_rename_reclaims_only_the_staged_state() {
    let _fixture_guard = super::db::db_open_busy_fixture_lock().blocking_lock();
    let temp = tempfile::tempdir().expect("temp dir");
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");

    assert_crashes_at(&database, CrashPoint::StateTempSyncedBeforeRename);
    assert!(
        !paths.state_path.exists(),
        "state was not renamed before the crash"
    );
    assert_eq!(
        state_temp_paths(&paths).len(),
        1,
        "crash strands one state temp"
    );

    let snapshot = ProfileDbAdmission::snapshot(&database).expect("recover staged state");
    assert_eq!(snapshot.active_holders, 0);
    assert!(state_temp_paths(&paths).is_empty());
    assert!(
        lease_paths(&paths).is_empty(),
        "unpublished lease is also swept"
    );
    assert_capacity_reusable(&database);
}

#[test]
fn crash_after_release_state_save_before_lease_unlink_reclaims_orphan() {
    let _fixture_guard = super::db::db_open_busy_fixture_lock().blocking_lock();
    let temp = tempfile::tempdir().expect("temp dir");
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");

    assert_crashes_at(&database, CrashPoint::ReleaseStateSavedBeforeLeaseUnlink);
    assert_eq!(
        state_holder_count(&paths),
        0,
        "release must durably remove the holder row before unlink"
    );
    assert_eq!(
        lease_paths(&paths).len(),
        1,
        "release crash must strand the now-unreferenced lease"
    );

    let first = ProfileDbAdmission::snapshot(&database).expect("sweep release orphan");
    let second = ProfileDbAdmission::snapshot(&database).expect("verify idempotent recovery");
    assert_eq!(first.active_holders, 0);
    assert_eq!(first.reaped_stale_holders_this_snapshot, 0);
    assert_eq!(second.reaped_stale_holders_this_snapshot, 0);
    assert!(lease_paths(&paths).is_empty());
    assert_capacity_reusable(&database);
}

#[test]
fn crash_after_reap_state_save_before_orphan_sweep_reclaims_lease_next_pass() {
    let _fixture_guard = super::db::db_open_busy_fixture_lock().blocking_lock();
    let temp = tempfile::tempdir().expect("temp dir");
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");
    seed_dead_holder(&paths, "reap-before-sweep");

    assert_crashes_at(&database, CrashPoint::ReapStateSavedBeforeOrphanSweep);
    assert_eq!(
        state_holder_count(&paths),
        0,
        "reaped state must be durable"
    );
    assert_eq!(lease_paths(&paths).len(), 1, "crash must strand the lease");

    let first = ProfileDbAdmission::snapshot(&database).expect("sweep stranded lease");
    let second = ProfileDbAdmission::snapshot(&database).expect("verify idempotent sweep");
    assert_eq!(first.active_holders, 0);
    assert_eq!(first.reaped_stale_holders_this_snapshot, 0);
    assert_eq!(second.active_holders, 0);
    assert!(lease_paths(&paths).is_empty());
    assert_capacity_reusable(&database);
}

#[test]
fn release_surfaces_one_time_unlink_failure_then_a_real_retry_removes_orphan() {
    let temp = tempfile::tempdir().expect("temp dir");
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");
    let admission = ProfileDbAdmission::acquire(
        &database,
        DbAdmissionRequest::new(DbHolderClass::Mcp, 1, 1024),
    )
    .expect("acquire holder");
    let leases = lease_paths(&paths);
    assert_eq!(leases.len(), 1);

    let unlink_fault = fail_next_holder_lease_unlink(&leases[0]);
    let error = admission
        .release()
        .expect_err("first release must surface the injected unlink failure");
    match error {
        DbAdmissionError::Io { path, source } => {
            assert_eq!(path, leases[0]);
            assert_eq!(
                source.to_string(),
                "injected one-time holder lease unlink failure"
            );
        }
        other => panic!("unexpected release error: {other:?}"),
    }

    assert_eq!(
        state_holder_count(&paths),
        0,
        "release must persist row removal"
    );
    assert_eq!(
        lease_paths(&paths),
        leases,
        "failed unlink must leave an orphan for a later real operation"
    );

    drop(unlink_fault);
    assert!(
        !admission
            .release()
            .expect("retry release after injected error"),
        "the holder row was already removed by the first release"
    );
    assert!(lease_paths(&paths).is_empty());
    drop(admission);
    assert_capacity_reusable(&database);
}

fn run_pre_close_parent_crash_fixture(lock_path: PathBuf) -> ! {
    let _state_lock = super::db_admission_state::lock_state(&lock_path)
        .expect("lock admission state before parent crash");
    let (gate, child_gate) = TestSetupGate::new().expect("create pre-close gate");
    let _worker = std::thread::spawn(move || {
        let mut spec = SpawnSpec::new("/bin/true").expect("absolute true executable");
        spec.pre_close_gate(child_gate);
        DeadlineChild::output(spec, Duration::from_secs(6))
    });
    gate.wait_ready(Instant::now() + Duration::from_millis(500))
        .expect("child stopped before parent crash");

    // SAFETY: the fixture intentionally models an uncatchable owner crash without running
    // StateLock::drop; the outer DeadlineChild owns bounded cleanup of this process group.
    unsafe { libc::_exit(PARENT_CRASH_EXIT_CODE) }
}

fn assert_crashes_at(database: &Path, point: CrashPoint) {
    let executable = std::env::current_exe().expect("current unit-test executable");
    let mut spec = SpawnSpec::new(executable).expect("absolute unit-test executable");
    spec.args(["--exact", FIXTURE_TEST, "--nocapture", "--test-threads=1"])
        .env(FIXTURE_DATABASE_ENV, database.as_os_str())
        .env(FIXTURE_CASE_ENV, fixture_case(point));

    // Match tests/db_admission.rs: collection bound returns on child exit;
    // CleanupIncomplete gets a separate reap retry, not a larger product timeout.
    let output = match DeadlineChild::output(spec, Duration::from_secs(30)) {
        Ok(output) => output,
        Err(SupervisionError::CleanupIncomplete(incomplete)) => incomplete
            .finish_output(Duration::from_secs(5))
            .expect("run admission crash fixture"),
        Err(error) => panic!("run admission crash fixture: {error:?}"),
    };
    assert_eq!(
        output.status.code(),
        Some(point.exit_code()),
        "fixture did not reach crash point: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.timed_out);
    assert!(output.cleanup.kill_fence_sent);
    assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
}

fn fixture_case(point: CrashPoint) -> &'static str {
    match point {
        CrashPoint::LeaseCreatedBeforeStatePublish => "lease-created-before-state-publish",
        CrashPoint::StateTempSyncedBeforeRename => "state-temp-synced-before-rename",
        CrashPoint::ReleaseStateSavedBeforeLeaseUnlink => "release-state-saved-before-lease-unlink",
        CrashPoint::ReapStateSavedBeforeOrphanSweep => "reap-state-saved-before-orphan-sweep",
    }
}

fn crash_point_for_case(case: &str) -> CrashPoint {
    match case {
        "lease-created-before-state-publish" => CrashPoint::LeaseCreatedBeforeStatePublish,
        "state-temp-synced-before-rename" => CrashPoint::StateTempSyncedBeforeRename,
        "release-state-saved-before-lease-unlink" => CrashPoint::ReleaseStateSavedBeforeLeaseUnlink,
        "reap-state-saved-before-orphan-sweep" => CrashPoint::ReapStateSavedBeforeOrphanSweep,
        other => panic!("unknown admission fixture case {other}"),
    }
}

fn seed_dead_holder(paths: &AdmissionPaths, token: &str) {
    let lease = paths.holder_lease_path(token);
    drop(
        OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&lease)
            .expect("create unlocked stale lease"),
    );
    let state = serde_json::json!({
        "next_generation": 1,
        "holders": [{
            "holder_class": "mcp",
            "owner_identity": "crashed-test-holder",
            "pid": u32::MAX,
            "generation": 1,
            "acquired_at_unix_secs": 1,
            "connection_count": 1,
            "configured_cache_bytes": 1024,
            "token": token,
            "process_identity": "crashed-test-process",
            "pid_namespace": "pid:[crashed-test]",
            "lease_version": 1
        }]
    });
    std::fs::write(
        &paths.state_path,
        serde_json::to_vec(&state).expect("serialize seeded state"),
    )
    .expect("write seeded state");
}

fn state_holder_count(paths: &AdmissionPaths) -> usize {
    match std::fs::read(&paths.state_path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes)
            .expect("parse admission state")["holders"]
            .as_array()
            .expect("holder array")
            .len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
        Err(error) => panic!("read admission state: {error}"),
    }
}

fn lease_paths(paths: &AdmissionPaths) -> Vec<PathBuf> {
    let mut leases = std::fs::read_dir(paths.state_parent())
        .expect("read admission sidecars")
        .map(|entry| entry.expect("read sidecar entry").path())
        .filter(|path| paths.is_current_lease_path(path))
        .collect::<Vec<_>>();
    leases.sort();
    leases
}

fn state_temp_paths(paths: &AdmissionPaths) -> Vec<PathBuf> {
    let mut staged = std::fs::read_dir(paths.state_parent())
        .expect("read admission sidecars")
        .map(|entry| entry.expect("read sidecar entry").path())
        .filter(|path| paths.is_current_state_temp_path(path))
        .collect::<Vec<_>>();
    staged.sort();
    staged
}

fn assert_capacity_reusable(database: &Path) {
    let admission = ProfileDbAdmission::acquire_with_config(
        database,
        DbAdmissionRequest::new(DbHolderClass::Mcp, 1, 1024),
        DbAdmissionConfig::new(1, 1024),
    )
    .expect("recovered capacity must be reusable");
    drop(admission);
    assert_eq!(
        ProfileDbAdmission::snapshot(database)
            .expect("snapshot reused capacity")
            .active_holders,
        0
    );
}
