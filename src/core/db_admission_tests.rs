#[cfg(target_os = "linux")]
use super::{
    AdmissionPaths, DbAdmissionError, DbAdmissionHolder, DbAdmissionRequest, DbHolderClass,
    ProfileDbAdmission, holder_is_live,
};
use super::{ProcessLiveness, retain_holder_for_liveness};

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
    }
}

#[cfg(target_os = "linux")]
#[test]
fn new_holder_record_stores_pid_namespace_identity() {
    let temp = tempfile::tempdir().expect("temp dir");
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
fn admission_paths_allow_directory_for_sqlite_diagnostics() {
    let temp = tempfile::tempdir().expect("temp dir");
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
    let temp = tempfile::tempdir().expect("temp dir");
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
