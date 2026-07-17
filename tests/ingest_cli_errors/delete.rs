use super::*;

fn run_delete(home: &Path, drawer_id: &str) -> Output {
    Command::new(mempal_bin())
        .args(["delete", drawer_id])
        .env("HOME", home)
        // Keep fixture identity independent of the developer's repo cwd.
        .env("MEMPAL_PROJECT_ID", "cli-delete-fixture")
        .output()
        .expect("run mempal delete")
}

fn insert_drawer(home: &Path, drawer_id: &str) {
    let db =
        mempal::core::db::Database::open(&home.join(".mempal").join("palace.db")).expect("open db");
    let drawer = mempal::core::types::Drawer::new_bootstrap_evidence(
        mempal::core::types::BootstrapEvidenceArgs {
            id: drawer_id.to_string(),
            content: "cli delete lease fixture".to_string(),
            wing: "lease-wing".to_string(),
            room: Some("lease-room".to_string()),
            source_file: Some("/tmp/cli-delete.md".to_string()),
            source_type: mempal::core::types::SourceType::AgentInference,
            added_at: "1713000000".to_string(),
            chunk_index: Some(0),
            importance: 2,
        },
    );
    db.insert_drawer_with_project(&drawer, Some("cli-delete-fixture"))
        .expect("insert drawer");
}

#[test]
fn test_cli_delete_succeeds_under_existing_writer_lease() {
    let tmp = setup_home();
    insert_drawer(tmp.path(), "cli-delete-lease-target");
    let _lease = hold_daemon_writer_lease(tmp.path());

    let output = run_delete(tmp.path(), "cli-delete-lease-target");

    assert!(
        output.status.success(),
        "delete should succeed under daemon writer lease: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("cli delete lease fixture"),
        "delete stdout must not expose raw drawer content"
    );
    let db = mempal::core::db::Database::open(&tmp.path().join(".mempal").join("palace.db"))
        .expect("open db");
    assert!(
        !db.drawer_exists("cli-delete-lease-target")
            .expect("drawer exists")
    );
}

#[test]
fn test_cli_delete_succeeds_without_writer_lease_conflict() {
    let tmp = setup_home();
    insert_drawer(tmp.path(), "cli-delete-success-target");

    let output = run_delete(tmp.path(), "cli-delete-success-target");

    assert!(
        output.status.success(),
        "delete should succeed without writer lease conflict: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let db = mempal::core::db::Database::open(&tmp.path().join(".mempal").join("palace.db"))
        .expect("open db");
    assert!(
        !db.drawer_exists("cli-delete-success-target")
            .expect("drawer exists")
    );
}
