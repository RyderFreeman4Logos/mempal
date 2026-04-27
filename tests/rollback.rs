use std::fs;
use std::path::PathBuf;
use std::process::Command;

use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

struct TestEnv {
    _tmp: TempDir,
    home: PathBuf,
    db_path: PathBuf,
}

impl TestEnv {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().join("home");
        let mempal_home = home.join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open database");
        fs::write(
            mempal_home.join("config.toml"),
            format!(
                r#"
db_path = "{}"

[embed]
backend = "model2vec"
"#,
                db_path.display()
            ),
        )
        .expect("write config");
        Self {
            _tmp: tmp,
            home,
            db_path,
        }
    }

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open database")
    }
}

fn insert_drawer(
    db: &Database,
    id: &str,
    added_at: &str,
    wing: &str,
    room: Option<&str>,
    project_id: Option<&str>,
) {
    db.insert_drawer_with_project(
        &Drawer {
            id: id.to_string(),
            content: format!("content for {id}"),
            wing: wing.to_string(),
            room: room.map(str::to_string),
            source_file: Some(format!("{id}.md")),
            source_type: SourceType::Manual,
            added_at: added_at.to_string(),
            chunk_index: Some(0),
            importance: 0,
        },
        project_id,
    )
    .expect("insert drawer");
}

fn is_deleted(db: &Database, id: &str) -> bool {
    db.conn()
        .query_row(
            "SELECT deleted_at IS NOT NULL FROM drawers WHERE id = ?1",
            [id],
            |row| row.get::<_, bool>(0),
        )
        .expect("read deleted_at")
}

fn deleted_ids(mut ids: Vec<String>) -> Vec<String> {
    ids.sort();
    ids
}

#[test]
fn test_rollback_deletes_only_after_since() {
    let env = TestEnv::new();
    let db = env.db();
    insert_drawer(&db, "before", "2026-01-01T00:00:00Z", "default", None, None);
    insert_drawer(&db, "middle", "2026-02-01T00:00:00Z", "default", None, None);
    insert_drawer(&db, "after", "2026-03-01T00:00:00Z", "default", None, None);

    let ids = db
        .soft_delete_drawers_since("2026-02-01T00:00:00Z", None, None, None)
        .expect("rollback");

    assert_eq!(deleted_ids(ids), vec!["after"]);
    assert!(!is_deleted(&db, "before"));
    assert!(!is_deleted(&db, "middle"));
    assert!(is_deleted(&db, "after"));
}

#[test]
fn test_rollback_wing_filter() {
    let env = TestEnv::new();
    let db = env.db();
    insert_drawer(
        &db,
        "target",
        "2026-03-01T00:00:00Z",
        "target-wing",
        None,
        None,
    );
    insert_drawer(
        &db,
        "other",
        "2026-03-01T00:00:00Z",
        "other-wing",
        None,
        None,
    );

    let ids = db
        .soft_delete_drawers_since("2026-02-01T00:00:00Z", Some("target-wing"), None, None)
        .expect("rollback");

    assert_eq!(deleted_ids(ids), vec!["target"]);
    assert!(is_deleted(&db, "target"));
    assert!(!is_deleted(&db, "other"));
}

#[test]
fn test_rollback_dry_run_no_mutation() {
    let env = TestEnv::new();
    let db = env.db();
    insert_drawer(&db, "before", "2026-01-01T00:00:00Z", "default", None, None);
    insert_drawer(&db, "after", "2026-03-01T00:00:00Z", "default", None, None);

    let count = db
        .count_drawers_since("2026-02-01T00:00:00Z", None, None, None)
        .expect("count rollback");

    assert_eq!(count, 1);
    assert!(!is_deleted(&db, "before"));
    assert!(!is_deleted(&db, "after"));
}

#[test]
fn test_rollback_already_deleted_not_re_deleted() {
    let env = TestEnv::new();
    let db = env.db();
    insert_drawer(
        &db,
        "already",
        "2026-03-01T00:00:00Z",
        "default",
        None,
        None,
    );
    assert!(db.soft_delete_drawer("already").expect("soft delete"));

    let ids = db
        .soft_delete_drawers_since("2026-02-01T00:00:00Z", None, None, None)
        .expect("rollback");

    assert!(ids.is_empty());
    assert!(is_deleted(&db, "already"));
}

#[test]
fn test_rollback_empty_result() {
    let env = TestEnv::new();
    let db = env.db();
    insert_drawer(&db, "old", "2026-03-01T00:00:00Z", "default", None, None);

    let ids = db
        .soft_delete_drawers_since("2027-01-01T00:00:00Z", None, None, None)
        .expect("rollback");

    assert!(ids.is_empty());
    assert!(!is_deleted(&db, "old"));
}

#[test]
fn test_rollback_cli_dry_run_no_mutation() {
    let env = TestEnv::new();
    let db = env.db();
    insert_drawer(
        &db,
        "old",
        "2026-01-01T00:00:00Z",
        "default",
        None,
        Some("home"),
    );
    insert_drawer(
        &db,
        "new",
        "2026-03-01T00:00:00Z",
        "default",
        None,
        Some("home"),
    );

    let output = Command::new(mempal_bin())
        .env("HOME", &env.home)
        .env_remove("MEMPAL_PROJECT_ID")
        .current_dir(&env.home)
        .args(["rollback", "--since", "2026-02-01T00:00:00Z", "--dry-run"])
        .output()
        .expect("run rollback dry-run");

    assert!(
        output.status.success(),
        "rollback dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("would delete 1 drawers since 2026-02-01T00:00:00Z"),
        "unexpected stdout: {stdout}"
    );
    let db = env.db();
    assert!(!is_deleted(&db, "old"));
    assert!(!is_deleted(&db, "new"));
}
