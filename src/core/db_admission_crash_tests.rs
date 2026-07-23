use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::db_admission::{
    AdmissionPaths, DbAdmissionConfig, DbAdmissionError, DbAdmissionRequest, DbHolderClass,
    ProfileDbAdmission,
};
use super::db_admission_fault_injection::{self as fault_injection, CrashPoint};
use super::db_admission_lease::fail_next_holder_lease_unlink;

// Test-only supervisor is shared with integration harnesses via path include (Rust 011).
#[path = "db_admission_test_process.rs"]
mod db_admission_test_process;

const _: fn() = db_admission_test_process::reference_shared_test_api;

use db_admission_test_process::{DeadlineChild, SpawnSpec};

const FIXTURE_CASE_ENV: &str = "MEMPAL_DB_ADMISSION_FIXTURE_CASE";
const FIXTURE_DATABASE_ENV: &str = "MEMPAL_DB_ADMISSION_FIXTURE_DATABASE";
const FIXTURE_TEST: &str = "core::db_admission_crash_tests::admission_crash_fixture";

#[test]
fn admission_crash_fixture() {
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
fn crash_after_lease_creation_before_state_publish_reclaims_orphan() {
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

fn assert_crashes_at(database: &Path, point: CrashPoint) {
    let executable = std::env::current_exe().expect("current unit-test executable");
    let mut spec = SpawnSpec::new(executable).expect("absolute unit-test executable");
    spec.args(["--exact", FIXTURE_TEST, "--nocapture", "--test-threads=1"])
        .env(FIXTURE_DATABASE_ENV, database.as_os_str())
        .env(FIXTURE_CASE_ENV, fixture_case(point));

    let output =
        DeadlineChild::output(spec, Duration::from_secs(3)).expect("run admission crash fixture");
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
