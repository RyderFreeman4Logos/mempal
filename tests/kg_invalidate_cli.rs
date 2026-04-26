//! Integration tests for issue #77: `mempal kg invalidate <triple_id>` CLI subcommand.

use std::path::Path;
use std::process::{Command, Output};

use mempal::core::db::Database;
use mempal::core::types::Triple;
use mempal::core::utils::build_triple_id;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db = Database::open(&mempal_home.join("palace.db")).expect("open db");
    (tmp, db)
}

fn insert_active_triple(db: &Database, subject: &str, predicate: &str, object: &str) -> String {
    let id = build_triple_id(subject, predicate, object);
    let triple = Triple {
        id: id.clone(),
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: object.to_string(),
        valid_from: Some("1700000000".to_string()),
        valid_to: None,
        confidence: 1.0,
        source_drawer: None,
    };
    db.insert_triple(&triple).expect("insert triple");
    id
}

fn run_kg_invalidate(home: &Path, triple_id: &str) -> Output {
    Command::new(mempal_bin())
        .args(["kg", "invalidate", triple_id])
        .env("HOME", home)
        .output()
        .expect("run mempal kg invalidate")
}

#[test]
fn test_kg_invalidate_existing_triple() {
    let (tmp, db) = setup();
    let triple_id = insert_active_triple(&db, "Alice", "works_at", "Acme");
    drop(db);

    let output = run_kg_invalidate(tmp.path(), &triple_id);
    assert!(
        output.status.success(),
        "expected success, stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("invalidated triple"),
        "expected 'invalidated triple' in output, got: {stdout}"
    );
    assert!(
        stdout.contains(&triple_id),
        "expected triple_id in output: {stdout}"
    );

    let db = Database::open(&tmp.path().join(".mempal").join("palace.db")).expect("reopen db");
    let triples = db
        .query_triples(Some("Alice"), None, None, false)
        .expect("query");
    assert_eq!(triples.len(), 1);
    assert!(
        triples[0].valid_to.is_some(),
        "valid_to should be set after invalidation"
    );
}

#[test]
fn test_kg_invalidate_missing_triple() {
    let (tmp, _db) = setup();

    let output = run_kg_invalidate(tmp.path(), "nonexistent-triple-id-xyz");
    assert!(
        !output.status.success(),
        "expected non-zero exit for missing triple"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("not found") || stderr.contains("nonexistent"),
        "expected clear error message, got: {stderr}"
    );
}

#[test]
fn test_kg_invalidate_idempotent() {
    let (tmp, db) = setup();
    let triple_id = insert_active_triple(&db, "Bob", "member_of", "Team");
    drop(db);

    let out1 = run_kg_invalidate(tmp.path(), &triple_id);
    assert!(
        out1.status.success(),
        "first invalidation should succeed, stderr={}",
        String::from_utf8_lossy(&out1.stderr)
    );

    let out2 = run_kg_invalidate(tmp.path(), &triple_id);
    assert!(
        out2.status.success(),
        "second invalidation should be no-op success, stderr={}",
        String::from_utf8_lossy(&out2.stderr)
    );
    let stdout2 = String::from_utf8_lossy(&out2.stdout);
    assert!(
        stdout2.contains("already invalidated"),
        "expected 'already invalidated' message: {stdout2}"
    );

    let db = Database::open(&tmp.path().join(".mempal").join("palace.db")).expect("reopen db");
    assert_eq!(
        db.triple_count().expect("triple_count"),
        1,
        "idempotent invalidation must not create duplicate rows"
    );
}

#[test]
fn test_kg_invalidate_writes_same_audit_as_mcp() {
    // The CLI path must write a "kg-invalidate" audit entry to audit.jsonl,
    // matching the same structured format used by delete/purge CLI commands.
    let (tmp, db) = setup();
    let triple_id = insert_active_triple(&db, "Carol", "owns", "Widget");
    drop(db);

    let out = run_kg_invalidate(tmp.path(), &triple_id);
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );

    let audit_path = tmp.path().join(".mempal").join("audit.jsonl");
    assert!(
        audit_path.exists(),
        "audit.jsonl must exist after kg invalidate"
    );

    let content = std::fs::read_to_string(&audit_path).expect("read audit.jsonl");
    let entry: serde_json::Value = content
        .lines()
        .filter_map(|l| serde_json::from_str(l).ok())
        .find(|v: &serde_json::Value| {
            v.get("command").and_then(|c| c.as_str()) == Some("kg-invalidate")
        })
        .expect("kg-invalidate audit entry not found in audit.jsonl");

    assert_eq!(
        entry["details"]["triple_id"].as_str().unwrap_or(""),
        triple_id,
        "audit entry triple_id mismatch"
    );
    assert!(
        entry.get("timestamp").is_some(),
        "audit entry must have timestamp field"
    );
}
