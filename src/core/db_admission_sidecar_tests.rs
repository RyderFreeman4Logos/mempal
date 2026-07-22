use std::fs;

use super::db_admission::{
    AdmissionPaths, DbAdmissionError, DbAdmissionRequest, DbHolderClass, ProfileDbAdmission,
};
use super::db_admission_state::MAX_ADMISSION_STATE_BYTES;

#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};

fn short_tempdir() -> tempfile::TempDir {
    tempfile::TempDir::new_in("/tmp").expect("short tempdir")
}

fn request() -> DbAdmissionRequest {
    DbAdmissionRequest::new(DbHolderClass::Cli, 1, 1024)
}

#[cfg(unix)]
#[test]
fn untrusted_admission_sidecar_parent_is_rejected_before_mutation() {
    let temp = short_tempdir();
    let database = temp.path().join("palace.db");
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o777))
        .expect("make parent group/world writable");

    let error = AdmissionPaths::new(&database).expect_err("untrusted parent must fail closed");

    assert!(matches!(
        error,
        DbAdmissionError::UnsafeSidecarDirectory { .. }
    ));
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700))
        .expect("restore temporary directory permissions");
}

#[cfg(unix)]
#[test]
fn lock_sidecar_symlink_is_rejected_without_following_the_target() {
    let temp = short_tempdir();
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");
    let target = temp.path().join("lock-target");
    fs::write(&target, b"must not become an admission lock").expect("write lock target");
    symlink(&target, &paths.lock_path).expect("link admission lock");

    let error =
        ProfileDbAdmission::snapshot(&database).expect_err("symlinked lock must fail closed");

    assert!(matches!(error, DbAdmissionError::UnsafeSidecar { .. }));
    assert_eq!(
        fs::read(&target).expect("read lock target"),
        b"must not become an admission lock"
    );
}

#[cfg(unix)]
#[test]
fn state_sidecar_symlink_is_rejected_without_following_the_target() {
    let temp = short_tempdir();
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");
    let target = temp.path().join("state-target");
    fs::write(&target, b"{}").expect("write state target");
    symlink(&target, &paths.state_path).expect("link admission state");

    let error =
        ProfileDbAdmission::snapshot(&database).expect_err("symlinked state must fail closed");

    assert!(matches!(error, DbAdmissionError::UnsafeSidecar { .. }));
    assert_eq!(fs::read(&target).expect("read state target"), b"{}");
}

#[cfg(unix)]
#[test]
fn hard_linked_state_sidecar_is_rejected_before_parsing() {
    let temp = tempfile::tempdir().expect("temp dir");
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");
    fs::write(
        &paths.state_path,
        r#"{"schema_version":1,"next_generation":0,"holders":[]}"#,
    )
    .expect("write state");
    fs::hard_link(&paths.state_path, temp.path().join("state-copy.json"))
        .expect("create state hard link");

    let error = ProfileDbAdmission::snapshot(&database).expect_err("hard link must fail closed");
    assert!(matches!(error, DbAdmissionError::UnsafeSidecar { .. }));
}

#[cfg(unix)]
#[test]
fn writable_state_sidecar_is_rejected_before_parsing() {
    use std::os::unix::fs::PermissionsExt;

    let temp = tempfile::tempdir().expect("temp dir");
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");
    fs::write(&paths.state_path, b"{}").expect("write state");
    fs::set_permissions(&paths.state_path, fs::Permissions::from_mode(0o660))
        .expect("make state group writable");

    let error = ProfileDbAdmission::snapshot(&database).expect_err("unsafe state must fail closed");
    assert!(matches!(error, DbAdmissionError::UnsafeSidecar { .. }));
}

#[test]
fn oversized_admission_state_is_rejected_before_json_parsing() {
    let temp = short_tempdir();
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");
    fs::write(&paths.state_path, vec![b'x'; MAX_ADMISSION_STATE_BYTES + 1])
        .expect("write oversized state");

    let error =
        ProfileDbAdmission::snapshot(&database).expect_err("oversized state must fail closed");

    assert!(matches!(error, DbAdmissionError::StateTooLarge { .. }));
}

#[test]
fn unknown_admission_state_schema_is_rejected_before_mutation() {
    let temp = short_tempdir();
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");
    let unsupported = br#"{"schema_version":99,"next_generation":0,"holders":[]}"#;
    fs::write(&paths.state_path, unsupported).expect("write unsupported state");

    let error = ProfileDbAdmission::acquire(&database, request())
        .expect_err("unknown state schema must fail closed");

    assert!(matches!(
        error,
        DbAdmissionError::UnsupportedStateVersion { version: 99, .. }
    ));
    assert_eq!(
        fs::read(&paths.state_path).expect("state remains unmodified"),
        unsupported
    );
}

#[cfg(unix)]
#[test]
fn only_valid_unreferenced_state_temps_are_reclaimed() {
    let temp = short_tempdir();
    let database = temp.path().join("palace.db");
    let paths = AdmissionPaths::new(&database).expect("admission paths");
    let valid = paths.state_temp_path("0123456789abcdef0123456789abcdef");
    let invalid = paths.state_path.with_file_name(format!(
        "{}.tmp.not-a-valid-admission-temp",
        paths
            .state_path
            .file_name()
            .expect("state path file name")
            .to_string_lossy()
    ));
    let protected_target = temp.path().join("protected-target");
    let symlink_temp = paths.state_temp_path("fedcba9876543210fedcba9876543210");
    fs::write(&valid, b"staged state").expect("write valid staged state");
    fs::write(&invalid, b"not our temp grammar").expect("write invalid staged state");
    fs::write(&protected_target, b"do not unlink through symlink").expect("write protected target");
    symlink(&protected_target, &symlink_temp).expect("link staged-state candidate");

    let snapshot = ProfileDbAdmission::snapshot(&database).expect("recover valid staged state");

    assert_eq!(snapshot.active_holders, 0);
    assert!(
        !valid.exists(),
        "valid unreferenced staged state is reclaimed"
    );
    assert!(invalid.exists(), "invalid temp grammar is never reclaimed");
    assert!(
        symlink_temp.is_symlink(),
        "symlink temp remains fail closed"
    );
    assert_eq!(
        fs::read(&protected_target).expect("read protected target"),
        b"do not unlink through symlink"
    );
}
