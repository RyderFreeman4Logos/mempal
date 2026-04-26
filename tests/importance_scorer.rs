use std::fs;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_home(tmp: &TempDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create .mempal dir");
    let db_path = mempal_home.join("palace.db");
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
    Database::open(&db_path).expect("initialize db");
    (home, db_path)
}

fn insert_drawer_at(db_path: &Path, id: &str, wing: &str, content: &str, importance: i32) {
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer(&Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: wing.to_string(),
        room: None,
        source_file: Some(format!("{id}.md")),
        source_type: SourceType::Manual,
        added_at: "1713000000".to_string(),
        chunk_index: Some(0),
        importance,
    })
    .expect("insert drawer");
}

fn run_recompute(home: &Path, only_zero: bool) -> std::process::Output {
    let mut cmd = Command::new(mempal_bin());
    cmd.env("HOME", home)
        .arg("reindex")
        .arg("--recompute-importance");
    if only_zero {
        cmd.arg("--only-zero");
    }
    cmd.output().expect("run recompute-importance command")
}

fn read_importance(db_path: &Path, id: &str) -> i32 {
    let db = Database::open(db_path).expect("open db");
    db.conn()
        .query_row(
            "SELECT COALESCE(importance, 0) FROM drawers WHERE id = ?1",
            [id],
            |row| row.get::<_, i32>(0),
        )
        .expect("read importance")
}

// --- CLI integration tests ---

#[test]
fn test_reindex_recompute_importance_updates_zero_drawers() {
    let tmp = TempDir::new().expect("tempdir");
    let (home, db_path) = setup_home(&tmp);

    // Seed a decision drawer and a chatter drawer, both at importance 0
    let decision_content = "# Decision: use SQLite\n\n\
        ## Why\nZero external dependencies.\n\n\
        ## Decision\nChose SQLite.\n\n\
        This is a long enough drawer to exceed 1000 bytes. "
        .to_string()
        + &"x".repeat(1000);
    insert_drawer_at(
        &db_path,
        "drawer-decision",
        "decision",
        &decision_content,
        0,
    );
    insert_drawer_at(&db_path, "drawer-chatter", "default", "ok sounds good", 0);

    let output = run_recompute(&home, false);
    assert!(
        output.status.success(),
        "recompute-importance failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Decision drawer should have received a score > 0
    let decision_score = read_importance(&db_path, "drawer-decision");
    assert!(
        decision_score >= 2,
        "decision drawer should score >= 2, got {decision_score}"
    );

    // Chatter drawer should have a very low score
    let chatter_score = read_importance(&db_path, "drawer-chatter");
    assert!(
        chatter_score <= 1,
        "chatter drawer should score <= 1, got {chatter_score}"
    );
}

#[test]
fn test_reindex_idempotent() {
    let tmp = TempDir::new().expect("tempdir");
    let (home, db_path) = setup_home(&tmp);

    let content = "# Decision: idempotency test\n## Why\nreason\n## Decision\ndecision text\n";
    insert_drawer_at(&db_path, "drawer-idem", "decision", content, 0);

    let first = run_recompute(&home, false);
    assert!(first.status.success(), "first run failed");
    let score_after_first = read_importance(&db_path, "drawer-idem");

    let second = run_recompute(&home, false);
    assert!(second.status.success(), "second run failed");
    let score_after_second = read_importance(&db_path, "drawer-idem");

    assert_eq!(
        score_after_first, score_after_second,
        "importance score must be stable across two runs"
    );
}

#[test]
fn test_reindex_only_zero_skips_nonzero() {
    let tmp = TempDir::new().expect("tempdir");
    let (home, db_path) = setup_home(&tmp);

    // Drawer A already has importance=3 (set explicitly)
    insert_drawer_at(&db_path, "drawer-nonzero", "default", "already set", 3);
    // Drawer B has importance=0 and should be scored
    let content = "# Decision: from zero\n## Why\nreason\n## Decision\ndecision\n";
    insert_drawer_at(&db_path, "drawer-zero", "decision", content, 0);

    let output = run_recompute(&home, true); // --only-zero
    assert!(output.status.success(), "recompute failed");

    // Drawer A should remain at 3 (was not zero, skipped)
    assert_eq!(
        read_importance(&db_path, "drawer-nonzero"),
        3,
        "non-zero drawer should not be touched by --only-zero"
    );
    // Drawer B should have been scored
    let scored = read_importance(&db_path, "drawer-zero");
    assert!(
        scored >= 2,
        "zero drawer should have been scored, got {scored}"
    );
}

#[test]
fn test_reindex_batched_does_not_block_concurrent_reads() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("init db");

    // Seed 1500 drawers (> batch size of 1000) so we exercise multiple batches
    let db_seed = Database::open(&db_path).expect("open for seeding");
    for i in 0..1500 {
        db_seed
            .insert_drawer(&Drawer {
                id: format!("batch-drawer-{i:04}"),
                content: format!("content for drawer {i}"),
                wing: "test".to_string(),
                room: None,
                source_file: None,
                source_type: SourceType::Manual,
                added_at: "1713000000".to_string(),
                chunk_index: Some(i as i64),
                importance: 0,
            })
            .expect("insert drawer");
    }
    drop(db_seed);

    let updates: Vec<(String, i32)> = (0..1500)
        .map(|i| (format!("batch-drawer-{i:04}"), 1))
        .collect();

    let db_path_writer = db_path.clone();
    let updates_for_writer = updates.clone();
    let writer_done = Arc::new(AtomicBool::new(false));
    let writer_done_clone = Arc::clone(&writer_done);

    // Writer: bulk_update_importance in a separate thread using its own DB connection
    let writer = thread::spawn(move || {
        let db_w = Database::open(&db_path_writer).expect("open writer db");
        db_w.bulk_update_importance(&updates_for_writer)
            .expect("bulk update importance");
        writer_done_clone.store(true, Ordering::SeqCst);
    });

    // Reader: concurrent SELECTs from the main thread using a different DB connection.
    // WAL mode guarantees readers are never blocked by an active writer.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut read_succeeded = false;
    while Instant::now() < deadline {
        let db_r = Database::open(&db_path).expect("open reader db");
        let count: i64 = db_r
            .conn()
            .query_row("SELECT COUNT(*) FROM drawers", [], |row| row.get(0))
            .expect("concurrent select should never fail in WAL mode");
        assert_eq!(count, 1500, "reader should always see all 1500 drawers");
        read_succeeded = true;
        if writer_done.load(Ordering::SeqCst) {
            break;
        }
        thread::sleep(Duration::from_millis(5));
    }
    assert!(read_succeeded, "reader loop never executed");

    writer.join().expect("writer thread panicked");

    // After writer completes, all rows should have importance = 1
    let db_final = Database::open(&db_path).expect("open final db");
    let still_zero: i64 = db_final
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM drawers WHERE importance = 0",
            [],
            |row| row.get(0),
        )
        .expect("final count");
    assert_eq!(
        still_zero, 0,
        "all drawers should have importance = 1 after bulk update"
    );
}
