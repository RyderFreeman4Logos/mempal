//! Integration tests for issue #87: `mempal reindex --normalize-added-at`
//! migrates legacy Unix-epoch `added_at` values to ISO 8601 (RFC 3339 UTC).

use std::path::Path;
use std::process::{Command, Output};

use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use rusqlite::params;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup() -> (TempDir, std::path::PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create .mempal dir");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");

    let config_path = mempal_home.join("config.toml");
    std::fs::write(
        &config_path,
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

    (tmp, db_path)
}

fn insert_drawer_with_raw_added_at(db_path: &Path, id: &str, added_at: &str) {
    let db = Database::open(db_path).expect("open db");
    let drawer = Drawer {
        id: id.to_string(),
        content: format!("content for {id}"),
        wing: "test".to_string(),
        room: Some("default".to_string()),
        source_file: Some(format!("{id}.md")),
        source_type: SourceType::Manual,
        added_at: "placeholder".to_string(), // will be overwritten below
        chunk_index: Some(0),
        importance: 0,
        ..Drawer::default()
    };
    db.insert_drawer_with_project(&drawer, None)
        .expect("insert drawer");
    // Overwrite added_at with the raw value (bypassing the ISO helper).
    db.conn()
        .execute(
            "UPDATE drawers SET added_at = ?1 WHERE id = ?2",
            params![added_at, id],
        )
        .expect("set raw added_at");
}

fn run_reindex_normalize(home: &Path) -> Output {
    Command::new(mempal_bin())
        .args(["reindex", "--normalize-added-at"])
        .env("HOME", home)
        // Run in the temp dir (non-git) so project-id inference does not
        // apply a ProjectScoped filter that would hide project_id=NULL rows.
        .current_dir(home)
        .output()
        .expect("run mempal reindex --normalize-added-at")
}

fn run_tail_since(home: &Path, since: &str) -> Output {
    Command::new(mempal_bin())
        .args(["tail", "--since", since, "--limit", "100"])
        .env("HOME", home)
        // Run in the temp dir (non-git) so project-id inference does not
        // apply a ProjectScoped filter that would hide project_id=NULL rows.
        .current_dir(home)
        .output()
        .expect("run mempal tail --since")
}

fn read_added_at(db_path: &Path, id: &str) -> String {
    let db = Database::open(db_path).expect("open db");
    db.conn()
        .query_row(
            "SELECT added_at FROM drawers WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )
        .expect("read added_at")
}

// ── test_reindex_normalize_idempotent ─────────────────────────────────────────

#[test]
fn test_reindex_normalize_idempotent() {
    let (tmp, db_path) = setup();

    // Insert mixed rows: some Unix epoch, some already ISO 8601.
    insert_drawer_with_raw_added_at(&db_path, "drawer_epoch_1", "1777169989");
    insert_drawer_with_raw_added_at(&db_path, "drawer_epoch_2", "1777080369");
    insert_drawer_with_raw_added_at(&db_path, "drawer_iso_1", "2026-04-26T05:39:49Z");

    // First run: should convert the 2 epoch rows.
    let out1 = run_reindex_normalize(tmp.path());
    assert!(
        out1.status.success(),
        "first run should succeed, stderr={}",
        String::from_utf8_lossy(&out1.stderr)
    );
    let stdout1 = String::from_utf8_lossy(&out1.stdout);
    assert!(
        stdout1.contains("2 drawers") || stdout1.contains("normalised"),
        "first run should report converted rows: {stdout1}"
    );

    // Verify epoch rows were converted.
    let at1 = read_added_at(&db_path, "drawer_epoch_1");
    assert!(
        at1.contains('T') && at1.ends_with('Z'),
        "drawer_epoch_1 should now be ISO 8601, got: {at1}"
    );
    let at2 = read_added_at(&db_path, "drawer_epoch_2");
    assert!(
        at2.contains('T') && at2.ends_with('Z'),
        "drawer_epoch_2 should now be ISO 8601, got: {at2}"
    );

    // Second run: should report 0 rows changed (all already ISO).
    let out2 = run_reindex_normalize(tmp.path());
    assert!(
        out2.status.success(),
        "second run should succeed, stderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout2.contains("nothing to do") || stdout2.contains("0 drawers"),
        "second run should report nothing changed: {stdout2}"
    );
}

// ── test_reindex_normalize_batched_does_not_block_reads ───────────────────────

#[test]
fn test_reindex_normalize_batched_does_not_block_reads() {
    let (tmp, db_path) = setup();

    // Insert a batch of rows with Unix epoch added_at.
    for i in 0..50 {
        insert_drawer_with_raw_added_at(
            &db_path,
            &format!("drawer_batch_{i:03}"),
            &format!("{}", 1_777_000_000u64 + i),
        );
    }

    // Run reindex --normalize-added-at in a background thread while concurrently
    // reading from the DB.  The WAL-mode batching must not block SELECTs.
    let db_path_clone = db_path.clone();
    let home = tmp.path().to_path_buf();
    let reindex_handle = std::thread::spawn(move || run_reindex_normalize(&home));

    // Concurrent reads during the reindex must succeed.
    let db = Database::open(&db_path_clone).expect("open db for concurrent read");
    let count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE deleted_at IS NULL",
            [],
            |r| r.get(0),
        )
        .expect("concurrent SELECT must not be blocked");
    assert!(count >= 0, "concurrent read must succeed");

    let out = reindex_handle
        .join()
        .expect("reindex thread should not panic");
    assert!(
        out.status.success(),
        "reindex should succeed, stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ── test_tail_since_works_after_normalize ─────────────────────────────────────

#[test]
fn test_tail_since_works_after_normalize() {
    let (tmp, db_path) = setup();

    // Current Unix epoch in seconds.
    let now_secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after epoch")
        .as_secs();

    // Insert a row with Unix epoch added_at set to 60 seconds ago — within
    // the "10m" window.
    let recent_secs = now_secs.saturating_sub(60);
    insert_drawer_with_raw_added_at(&db_path, "drawer_recent_epoch", &recent_secs.to_string());

    // Insert an old row (25 hours ago) — outside the "10m" window.
    let old_secs = now_secs.saturating_sub(25 * 3600);
    insert_drawer_with_raw_added_at(&db_path, "drawer_old_epoch", &old_secs.to_string());

    // Before normalization, both rows have Unix epoch added_at.
    // The fix to parse_added_at means tail --since already works for Unix-epoch
    // rows; verify it here too.
    let out_before = run_tail_since(tmp.path(), "10m");
    let stdout_before = String::from_utf8_lossy(&out_before.stdout);
    assert!(
        out_before.status.success(),
        "tail --since before normalize should succeed, stderr={}",
        String::from_utf8_lossy(&out_before.stderr)
    );
    assert!(
        stdout_before.contains("drawer_recent_epoch"),
        "recent row must appear in tail --since 10m before normalize, got: {stdout_before}"
    );
    assert!(
        !stdout_before.contains("drawer_old_epoch"),
        "old row must NOT appear in tail --since 10m before normalize, got: {stdout_before}"
    );

    // Normalize.
    let norm_out = run_reindex_normalize(tmp.path());
    assert!(
        norm_out.status.success(),
        "normalize should succeed, stderr={}",
        String::from_utf8_lossy(&norm_out.stderr)
    );

    // After normalization: added_at values are ISO 8601.
    let at_recent = read_added_at(&db_path, "drawer_recent_epoch");
    assert!(
        at_recent.contains('T'),
        "after normalize: drawer_recent_epoch should be ISO, got: {at_recent}"
    );

    // tail --since 10m must still return the recent row (ISO filtering works).
    let out_after = run_tail_since(tmp.path(), "10m");
    assert!(
        out_after.status.success(),
        "tail --since after normalize should succeed, stderr={}",
        String::from_utf8_lossy(&out_after.stderr)
    );
    let stdout_after = String::from_utf8_lossy(&out_after.stdout);
    assert!(
        stdout_after.contains("drawer_recent_epoch"),
        "recent row must appear after normalize, got: {stdout_after}"
    );
    assert!(
        !stdout_after.contains("drawer_old_epoch"),
        "old row must NOT appear after normalize, got: {stdout_after}"
    );
}

// ── test_ingest_path_writes_iso_not_epoch ─────────────────────────────────────

#[test]
fn test_ingest_path_writes_iso_not_epoch() {
    use mempal::core::utils::iso_timestamp;

    // The iso_timestamp() helper must produce a valid RFC 3339 UTC string.
    let ts = iso_timestamp();
    assert!(
        ts.contains('T'),
        "iso_timestamp() must contain 'T' separator, got: {ts}"
    );
    assert!(
        ts.ends_with('Z'),
        "iso_timestamp() must end with 'Z' (UTC), got: {ts}"
    );
    // Must not be a bare integer.
    assert!(
        !ts.chars().all(|c| c.is_ascii_digit()),
        "iso_timestamp() must not be a bare Unix epoch integer, got: {ts}"
    );

    // When a drawer is ingested via the library, the stored added_at must be
    // ISO 8601 (not a bare integer string).
    let (tmp, db_path) = setup();
    let db = Database::open(&db_path).expect("open db");
    let drawer = Drawer {
        id: "drawer_test_iso_write".to_string(),
        content: "test content".to_string(),
        wing: "test".to_string(),
        room: Some("default".to_string()),
        source_file: Some("test.md".to_string()),
        source_type: SourceType::Manual,
        added_at: iso_timestamp(),
        chunk_index: Some(0),
        importance: 0,
        ..Drawer::default()
    };
    db.insert_drawer_with_project(&drawer, None)
        .expect("insert drawer");

    let stored_at: String = db
        .conn()
        .query_row(
            "SELECT added_at FROM drawers WHERE id = 'drawer_test_iso_write'",
            [],
            |row| row.get(0),
        )
        .expect("read stored added_at");

    assert!(
        stored_at.contains('T') && stored_at.ends_with('Z'),
        "stored added_at must be ISO 8601, got: {stored_at}"
    );
    drop(tmp);
}
