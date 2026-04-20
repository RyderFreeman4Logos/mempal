//! Integration tests for P9-A fact checker (mempal_fact_check MCP tool + CLI).

use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use mempal::core::db::Database;
use mempal::core::types::Triple;
use mempal::factcheck::{FactIssue, check};
use tempfile::TempDir;

/// Unix seconds "now" baseline for tests — fixed so valid_to comparisons
/// are deterministic.
const NOW_SECS: u64 = 1_800_000_000; // ~2027-01-15
const NOW_RFC3339: &str = "2027-01-15T08:00:00Z";

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn new_test_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db)
}

fn insert_triple_active(db: &Database, subject: &str, predicate: &str, object: &str) {
    let id = mempal::core::utils::build_triple_id(subject, predicate, object);
    let triple = Triple {
        id,
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: object.to_string(),
        valid_from: Some(NOW_SECS.saturating_sub(86_400).to_string()),
        valid_to: None,
        confidence: 1.0,
        source_drawer: None,
    };
    db.insert_triple(&triple).expect("insert triple");
}

fn insert_triple_expired(db: &Database, subject: &str, predicate: &str, object: &str) {
    let id = mempal::core::utils::build_triple_id(subject, predicate, object);
    let triple = Triple {
        id,
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: object.to_string(),
        valid_from: Some((NOW_SECS.saturating_sub(86_400 * 30)).to_string()),
        valid_to: Some((NOW_SECS.saturating_sub(86_400)).to_string()),
        confidence: 1.0,
        source_drawer: None,
    };
    db.insert_triple(&triple).expect("insert triple");
}

fn setup_cli_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db = Database::open(&mempal_home.join("palace.db")).expect("open db");
    (tmp, db)
}

#[test]
fn test_similar_name_conflict_detected() {
    let (_tmp, db) = new_test_db();
    insert_triple_active(&db, "Bob", "husband_of", "Alice");

    let text = "Bobby is Alice's husband";
    let report = check(text, &db, NOW_SECS, None).expect("check");

    let has_conflict = report.issues.iter().any(|i| {
        matches!(
            i,
            FactIssue::SimilarNameConflict {
                mentioned, known_entity, ..
            } if mentioned == "Bobby" && known_entity == "Bob"
        )
    });
    assert!(
        has_conflict,
        "expected SimilarNameConflict, got issues: {:?}",
        report.issues
    );
}

#[test]
fn test_relation_contradiction_detected() {
    let (_tmp, db) = new_test_db();
    insert_triple_active(&db, "Bob", "husband_of", "Alice");

    let text = "Bob is Alice's brother";
    let report = check(text, &db, NOW_SECS, None).expect("check");

    let has_rel = report.issues.iter().any(|i| {
        matches!(
            i,
            FactIssue::RelationContradiction {
                subject,
                text_claim,
                kg_fact,
                ..
            } if subject == "Bob" && text_claim == "brother_of" && kg_fact == "husband_of"
        )
    });
    assert!(
        has_rel,
        "expected RelationContradiction, got: {:?}",
        report.issues
    );
}

#[test]
fn test_stale_fact_detected() {
    let (_tmp, db) = new_test_db();
    insert_triple_expired(&db, "Alice", "works_at", "Acme");

    let text = "Alice works at Acme";
    let report = check(text, &db, NOW_SECS, None).expect("check");

    let has_stale = report.issues.iter().any(|i| {
        matches!(
            i,
            FactIssue::StaleFact {
                subject,
                predicate,
                object,
                ..
            } if subject == "Alice" && predicate == "works_at" && object == "Acme"
        )
    });
    assert!(has_stale, "expected StaleFact, got: {:?}", report.issues);
}

#[test]
fn test_consistent_text_no_issues() {
    let (_tmp, db) = new_test_db();
    insert_triple_active(&db, "Bob", "husband_of", "Alice");

    // Narrative text that doesn't match any extractable triple pattern
    // and doesn't contain a typo relative to known entities.
    let text = "Bob and Alice went hiking together today.";
    let report = check(text, &db, NOW_SECS, None).expect("check");

    assert!(
        report.issues.is_empty(),
        "expected no issues, got: {:?}",
        report.issues
    );
    // Known entities "Bob" and "Alice" appear exactly as stored → no conflict
    assert!(report.checked_entities.iter().any(|e| e == "Bob"));
    assert!(report.checked_entities.iter().any(|e| e == "Alice"));
}

#[test]
fn test_fact_check_has_no_db_side_effects() {
    let (_tmp, db) = new_test_db();
    insert_triple_active(&db, "Bob", "husband_of", "Alice");

    let schema_before = db.schema_version().unwrap();
    let drawer_before = db.drawer_count().unwrap();
    let triple_before = db.triple_count().unwrap();

    // Run several fact checks with different texts.
    let _ = check("Bob is Alice's brother", &db, NOW_SECS, None);
    let _ = check("Bobby is Alice's husband", &db, NOW_SECS, None);
    let _ = check("Alice works at Acme", &db, NOW_SECS, None);
    let _ = check(
        "Random narrative with no pattern match.",
        &db,
        NOW_SECS,
        None,
    );

    assert_eq!(db.schema_version().unwrap(), schema_before);
    assert_eq!(db.drawer_count().unwrap(), drawer_before);
    assert_eq!(db.triple_count().unwrap(), triple_before);
}

#[test]
fn test_unknown_entity_no_false_positive() {
    let (_tmp, db) = new_test_db();
    // Empty KG — no triples, no drawers.

    let text = "Alice and Bob went to the store.";
    let report = check(text, &db, NOW_SECS, None).expect("check");

    assert!(
        report.issues.is_empty(),
        "empty KG should not produce conflicts; got: {:?}",
        report.issues
    );
}

#[test]
fn test_empty_text_no_issues() {
    let (_tmp, db) = new_test_db();
    insert_triple_active(&db, "Alice", "works_at", "Acme");

    let report = check("", &db, NOW_SECS, None).expect("check");
    assert!(report.issues.is_empty());
    assert!(report.checked_entities.is_empty());
}

#[test]
fn test_scope_filters_known_entities() {
    let (_tmp, db) = new_test_db();
    // KG triple whose subject "Kevyn" is close in edit distance to "Kevin".
    // With no drawer seeding, the only source is the KG.
    insert_triple_active(&db, "Kevyn", "works_at", "Acme");

    let text = "Kevin is on the team.";
    let report = check(text, &db, NOW_SECS, Some(("mempal", None))).expect("check");

    // Kevyn (KG) vs Kevin (text) → distance 1. Scope filter applies to
    // drawer-derived entities but KG path is unscoped in the current impl
    // (known entities merge KG ∪ scoped drawers). This test pins that
    // behavior so future refactors are intentional.
    let has_conflict = report
        .issues
        .iter()
        .any(|i| matches!(i, FactIssue::SimilarNameConflict { .. }));
    assert!(has_conflict);
}

#[test]
fn test_validate_scope_rejects_room_without_wing() {
    let err = mempal::factcheck::validate_scope(None, Some("design")).expect_err("invalid scope");
    assert!(matches!(
        err,
        mempal::factcheck::FactCheckError::InvalidScope(_)
    ));
}

#[test]
fn test_custom_now_overrides_current_time() {
    let (_tmp, db) = new_test_db();

    // Triple valid_to at NOW_SECS (right at the cutoff). When now ==
    // valid_to, it's NOT expired (strict < comparison). When now > valid_to,
    // it IS expired.
    let id = mempal::core::utils::build_triple_id("Alice", "works_at", "Acme");
    let triple = Triple {
        id,
        subject: "Alice".to_string(),
        predicate: "works_at".to_string(),
        object: "Acme".to_string(),
        valid_from: Some((NOW_SECS - 1000).to_string()),
        valid_to: Some(NOW_SECS.to_string()),
        confidence: 1.0,
        source_drawer: None,
    };
    db.insert_triple(&triple).expect("insert");

    // now == valid_to → not stale
    let report_equal = check("Alice works at Acme", &db, NOW_SECS, None).expect("check");
    assert!(
        !report_equal
            .issues
            .iter()
            .any(|i| matches!(i, FactIssue::StaleFact { .. })),
        "triple with valid_to == now should not be stale"
    );

    // now > valid_to → stale
    let report_after = check("Alice works at Acme", &db, NOW_SECS + 1, None).expect("check");
    assert!(
        report_after
            .issues
            .iter()
            .any(|i| matches!(i, FactIssue::StaleFact { .. })),
        "triple with valid_to < now must be stale"
    );
}

#[test]
fn test_report_serializes_to_json_round_trip() {
    let (_tmp, db) = new_test_db();
    insert_triple_active(&db, "Bob", "husband_of", "Alice");

    let text = "Bob is Alice's brother";
    let report = check(text, &db, NOW_SECS, None).expect("check");
    assert!(!report.issues.is_empty());

    let json = serde_json::to_string(&report).expect("serialize");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("parse");
    assert!(parsed.get("issues").is_some());
    assert!(parsed.get("checked_entities").is_some());
    assert!(parsed.get("kg_triples_scanned").is_some());

    // Round trip through FactCheckReport.
    let back: mempal::factcheck::FactCheckReport =
        serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back.issues.len(), report.issues.len());
}

#[test]
fn test_current_time_default_path_works() {
    // Smoke test that using SystemTime::now() in the default path doesn't
    // panic or misclassify an active triple.
    let (_tmp, db) = new_test_db();
    insert_triple_active(&db, "Alice", "works_at", "Acme");

    let wall_now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();

    let report = check("Alice works at Acme", &db, wall_now, None).expect("check");
    let stale_count = report
        .issues
        .iter()
        .filter(|i| matches!(i, FactIssue::StaleFact { .. }))
        .count();
    assert_eq!(stale_count, 0, "active triple should not be stale");
}

#[test]
fn test_cli_fact_check_from_stdin() {
    let (tmp, db) = setup_cli_db();
    insert_triple_active(&db, "Bob", "husband_of", "Alice");

    let mut child = Command::new(mempal_bin())
        .arg("fact-check")
        .arg("-")
        .arg("--now")
        .arg(NOW_RFC3339)
        .env("HOME", tmp.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn mempal fact-check");

    use std::io::Write;
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(b"Bob is Alice's brother")
        .expect("write stdin");

    let output = child.wait_with_output().expect("wait output");
    assert!(output.status.success(), "stdout={output:?}");

    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(stdout.contains("\"relation_contradiction\""), "{stdout}");
    assert!(stdout.contains("\"husband_of\""), "{stdout}");
}
