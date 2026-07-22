#[cfg(target_os = "linux")]
use super::{
    AdmissionPaths, DbAdmissionError, DbAdmissionHolder, DbAdmissionRequest, DbHolderClass,
    ProfileDbAdmission, holder_is_live, imp,
};
use super::{ProcessLiveness, retain_holder_for_liveness};
#[cfg(target_os = "linux")]
use std::fs::{File, OpenOptions};
#[cfg(target_os = "linux")]
use std::path::Path;

fn short_tempdir() -> tempfile::TempDir {
    tempfile::TempDir::new_in("/tmp").expect("short tempdir")
}

#[cfg(target_os = "linux")]
fn holder(pid: u32, process_identity: &str, pid_namespace: Option<String>) -> DbAdmissionHolder {
    DbAdmissionHolder {
        holder_class: DbHolderClass::Mcp,
        owner_identity: "test-holder".to_string(),
        pid,
        generation: 1,
        acquired_at_unix_secs: 1,
        connection_count: 1,
        configured_cache_bytes: 1024,
        token: "test-token".to_string(),
        process_identity: process_identity.to_string(),
        pid_namespace,
        lease_version: 0,
    }
}

#[cfg(target_os = "linux")]
fn raw_leased_holder(token: &str, pid: u32, pid_namespace: &str) -> serde_json::Value {
    serde_json::json!({
        "holder_class": "mcp",
        "owner_identity": format!("test-holder-{token}"),
        "pid": pid,
        "generation": 1,
        "acquired_at_unix_secs": 1,
        "connection_count": 1,
        "configured_cache_bytes": 1024,
        "token": token,
        "process_identity": "foreign-process-birth",
        "pid_namespace": pid_namespace,
        "lease_version": 1
    })
}

#[cfg(target_os = "linux")]
fn write_raw_state(path: &Path, holders: Vec<serde_json::Value>) {
    std::fs::write(
        path,
        serde_json::to_vec(&serde_json::json!({
            "next_generation": holders.len(),
            "holders": holders
        }))
        .expect("serialize admission state"),
    )
    .expect("write admission state");
}

#[cfg(target_os = "linux")]
fn create_lease(paths: &AdmissionPaths, token: &str, lock: bool) -> File {
    let lease_path = paths.holder_lease_path(token);
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lease_path)
        .expect("create holder lease");
    if lock {
        assert!(
            imp::try_lock_exclusive(&file).expect("lock holder lease"),
            "new holder lease must be lockable"
        );
    }
    file
}

#[cfg(target_os = "linux")]
#[test]
fn new_holder_record_stores_pid_namespace_identity() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let _admission = ProfileDbAdmission::acquire(
        &db_path,
        DbAdmissionRequest::new(DbHolderClass::Mcp, 1, 1024),
    )
    .expect("acquire holder");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(paths.state_path).expect("read admission state"))
            .expect("parse admission state");
    let expected = std::fs::read_link("/proc/self/ns/pid")
        .expect("read current PID namespace")
        .to_string_lossy()
        .into_owned();

    assert_eq!(state["holders"][0]["pid_namespace"], expected);
}

#[cfg(target_os = "linux")]
#[test]
fn cross_pid_namespace_holders_are_retained_when_pid_conflicts_or_is_missing() {
    for pid in [std::process::id(), u32::MAX] {
        let foreign_holder = holder(
            pid,
            "foreign-process-birth",
            Some("pid:[foreign]".to_string()),
        );

        assert!(
            holder_is_live(&foreign_holder),
            "foreign namespace holder with PID {pid} must be retained"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn crashed_holder_in_nested_pid_namespace_is_reaped_from_lease() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let token = "nested-crashed";
    drop(create_lease(&paths, token, false));
    write_raw_state(
        &paths.state_path,
        vec![raw_leased_holder(token, 7, "pid:[nested-child]")],
    );

    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("reap crashed holder");

    assert_eq!(snapshot.active_holders, 0);
    assert_eq!(snapshot.reaped_stale_holders_this_snapshot, 1);
    assert_eq!(snapshot.unknown_holders, 0);
}

#[cfg(target_os = "linux")]
#[test]
fn unrelated_host_pid_alias_does_not_keep_dead_namespaced_holder() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let token = "host-pid-alias";
    drop(create_lease(&paths, token, false));
    write_raw_state(
        &paths.state_path,
        vec![raw_leased_holder(
            token,
            std::process::id(),
            "pid:[unrelated-namespace]",
        )],
    );

    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("reap aliased holder");

    assert_eq!(snapshot.active_holders, 0);
    assert_eq!(snapshot.reaped_stale_holders_this_snapshot, 1);
}

#[cfg(target_os = "linux")]
#[test]
fn live_foreign_namespace_owner_keeps_multiple_registrations() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let first_token = "live-registration-one";
    let second_token = "live-registration-two";
    let _first_lease = create_lease(&paths, first_token, true);
    let _second_lease = create_lease(&paths, second_token, true);
    write_raw_state(
        &paths.state_path,
        vec![
            raw_leased_holder(first_token, 19, "pid:[live-foreign]"),
            raw_leased_holder(second_token, 19, "pid:[live-foreign]"),
        ],
    );

    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("retain live holder leases");

    assert_eq!(snapshot.active_holders, 2);
    assert_eq!(snapshot.holders.len(), 2);
    assert_eq!(snapshot.unknown_holders, 0);
}

#[cfg(target_os = "linux")]
#[test]
fn repeated_smoke_style_crash_reap_cycles_do_not_grow_holder_state() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");

    for cycle in 0..32 {
        let token = format!("smoke-crash-{cycle}");
        drop(create_lease(&paths, &token, false));
        write_raw_state(
            &paths.state_path,
            vec![raw_leased_holder(
                &token,
                100 + cycle,
                &format!("pid:[smoke-{cycle}]"),
            )],
        );

        let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("reap smoke holder");
        assert_eq!(
            snapshot.active_holders, 0,
            "stale holder survived cycle {cycle}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn unreferenced_current_database_lease_is_reclaimed_on_next_snapshot() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let token = "orphaned-before-state-publication";
    let lease_path = paths.holder_lease_path(token);
    drop(create_lease(&paths, token, false));
    write_raw_state(&paths.state_path, Vec::new());

    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("recover orphaned lease");

    assert_eq!(snapshot.active_holders, 0);
    assert!(
        !lease_path.exists(),
        "an unreferenced current-format lease must be reclaimed before admission writes"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn old_writer_stripped_lease_version_still_uses_current_lease_as_liveness_anchor() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let token = "old-writer-stripped-version";
    drop(create_lease(&paths, token, false));
    let mut holder = raw_leased_holder(token, 7, "pid:[foreign-old-writer]");
    holder
        .as_object_mut()
        .expect("holder object")
        .remove("lease_version");
    write_raw_state(&paths.state_path, vec![holder]);

    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("recover old-writer holder");

    assert_eq!(
        snapshot.active_holders, 0,
        "an unlocked deterministic lease proves this legacy-shaped holder is dead"
    );
    assert_eq!(snapshot.unknown_holders, 0);
}

#[cfg(target_os = "linux")]
#[test]
fn orphan_sweep_retries_after_a_later_verifiable_pass() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let token = "retry-after-unlink-verification";
    let lease_path = paths.holder_lease_path(token);
    let alias_path = temp.path().join("lease-hard-link");
    drop(create_lease(&paths, token, false));
    std::fs::hard_link(&lease_path, &alias_path).expect("make lease temporarily unverifiable");
    write_raw_state(&paths.state_path, Vec::new());

    let first = ProfileDbAdmission::snapshot(&db_path).expect("first recovery pass");
    assert_eq!(first.active_holders, 0);
    assert!(
        lease_path.exists(),
        "a multi-link lease must be retained fail-closed on the first pass"
    );

    std::fs::remove_file(&alias_path).expect("restore single-link lease");
    let second = ProfileDbAdmission::snapshot(&db_path).expect("second recovery pass");
    assert_eq!(second.active_holders, 0);
    assert!(
        !lease_path.exists(),
        "a later verifiable pass must recover the previously retained orphan"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn repeated_orphan_recovery_does_not_grow_current_lease_entries() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    write_raw_state(&paths.state_path, Vec::new());

    for cycle in 0..32 {
        let token = format!("orphan-cycle-{cycle}");
        let lease_path = paths.holder_lease_path(&token);
        drop(create_lease(&paths, &token, false));
        let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("recover orphaned lease");
        assert_eq!(snapshot.active_holders, 0);
        assert!(
            !lease_path.exists(),
            "orphan lease survived recovery cycle {cycle}"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn unknown_version_holder_protects_its_deterministic_lease_from_orphan_sweep() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let token = "unknown-version-protected";
    let lease_path = paths.holder_lease_path(token);
    drop(create_lease(&paths, token, false));
    let mut holder = raw_leased_holder(token, 7, "pid:[unknown-version]");
    holder["lease_version"] = serde_json::json!(u8::MAX);
    write_raw_state(&paths.state_path, vec![holder]);

    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("inspect unknown holder");

    assert!(
        lease_path.exists(),
        "unknown-version holder must retain its lease"
    );
    assert_eq!(snapshot.unknown_holders, 1);
    assert_eq!(
        snapshot.unknown_holder_diagnostics,
        vec![super::UnknownHolderDiagnostic {
            generation: 1,
            reason: super::UnknownHolderReason::UnknownLeaseVersion,
        }]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn stale_reap_count_is_limited_to_the_snapshot_that_performed_recovery() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let token = "snapshot-reap-count";
    drop(create_lease(&paths, token, false));
    write_raw_state(
        &paths.state_path,
        vec![raw_leased_holder(token, 7, "pid:[snapshot-reap]")],
    );

    let first = ProfileDbAdmission::snapshot(&db_path).expect("first snapshot");
    let second = ProfileDbAdmission::snapshot(&db_path).expect("second snapshot");

    assert_eq!(first.reaped_stale_holders_this_snapshot, 1);
    assert_eq!(second.reaped_stale_holders_this_snapshot, 0);
}

#[cfg(target_os = "linux")]
#[test]
fn missing_process_in_current_pid_namespace_is_reclaimed() {
    let pid_namespace = std::fs::read_link("/proc/self/ns/pid")
        .expect("read current PID namespace")
        .to_string_lossy()
        .into_owned();
    let missing_holder = holder(u32::MAX, "missing-process-birth", Some(pid_namespace));

    assert!(!holder_is_live(&missing_holder));
}

#[cfg(target_os = "linux")]
#[test]
fn legacy_holder_without_pid_namespace_is_retained_fail_closed() {
    let legacy_holder: DbAdmissionHolder = serde_json::from_value(serde_json::json!({
        "holder_class": "mcp",
        "owner_identity": "legacy-holder",
        "pid": u32::MAX,
        "generation": 1,
        "acquired_at_unix_secs": 1,
        "connection_count": 1,
        "configured_cache_bytes": 1024,
        "token": "legacy-token",
        "process_identity": "missing-process-birth"
    }))
    .expect("deserialize legacy holder");

    assert_eq!(legacy_holder.pid_namespace, None);
    assert!(holder_is_live(&legacy_holder));
}

#[cfg(target_os = "linux")]
#[test]
fn snapshot_reports_unverifiable_legacy_holder() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let mut legacy = raw_leased_holder("legacy-unknown", 23, "pid:[foreign-legacy]");
    legacy
        .as_object_mut()
        .expect("holder object")
        .remove("lease_version");
    write_raw_state(&paths.state_path, vec![legacy]);

    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("snapshot legacy holder");

    assert_eq!(snapshot.active_holders, 1);
    assert_eq!(snapshot.unknown_holders, 1);
    assert_eq!(snapshot.unknown_holder_generations, vec![1]);
    assert_eq!(
        snapshot.unknown_holder_diagnostics,
        vec![super::UnknownHolderDiagnostic {
            generation: 1,
            reason: super::UnknownHolderReason::LegacyProcessIdentityUnverifiable,
        }]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn nofollow_lease_open_failure_is_reported_as_a_typed_unknown_reason() {
    use std::os::unix::fs::symlink;

    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&db_path).expect("admission paths");
    let token = "nofollow-open-failure";
    let lease_path = paths.holder_lease_path(token);
    let target = temp.path().join("not-a-lease");
    std::fs::write(&target, b"not a lease").expect("write symlink target");
    symlink(&target, &lease_path).expect("create lease-path symlink");
    write_raw_state(
        &paths.state_path,
        vec![raw_leased_holder(token, 7, "pid:[nofollow-open-failure]")],
    );

    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("inspect protected lease");

    assert_eq!(snapshot.active_holders, 1);
    assert_eq!(
        snapshot.unknown_holder_diagnostics,
        vec![super::UnknownHolderDiagnostic {
            generation: 1,
            reason: super::UnknownHolderReason::LeaseOpenUnavailable,
        }]
    );
    assert!(
        lease_path.is_symlink(),
        "unknown holder lease must remain protected"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn admission_paths_allow_directory_for_sqlite_diagnostics() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    std::fs::create_dir(&db_path).expect("create directory at database path");

    let paths = AdmissionPaths::new(&db_path).expect("admission must defer non-regular paths");

    assert_eq!(
        paths.database_path,
        std::fs::canonicalize(db_path).expect("canonical database directory")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn admission_paths_reject_regular_file_with_multiple_hard_links() {
    let temp = short_tempdir();
    let db_path = temp.path().join("palace.db");
    let alias_path = temp.path().join("palace-alias.db");
    std::fs::write(&db_path, b"sqlite fixture").expect("create database file");
    std::fs::hard_link(&db_path, alias_path).expect("create database hard link");

    let error = match AdmissionPaths::new(&db_path) {
        Ok(_) => panic!("hard-linked regular database must be rejected"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        DbAdmissionError::InvalidRequest(
            "database file has multiple hard links; admission identity cannot be established safely"
        )
    ));
}

#[test]
fn unverifiable_foreign_process_is_retained_fail_closed() {
    assert!(retain_holder_for_liveness(ProcessLiveness::Unverifiable));
}

#[test]
fn confirmed_dead_foreign_process_is_reclaimable() {
    assert!(!retain_holder_for_liveness(ProcessLiveness::Dead));
}
