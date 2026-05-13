//! Integration tests for the automatic ingest fact-check gate.

use std::fs;
use std::path::Path;

use mempal::core::config::{AutoFactCheckConfig, IngestGatingConfig};
use mempal::core::db::Database;
use mempal::core::types::{SourceType, Triple};
use mempal::core::utils::build_triple_id;
use mempal::embed::{Embedder, Result as EmbedResult};
use mempal::ingest::{IngestOptions, ingest_file_with_options};
use tempfile::TempDir;

#[derive(Default)]
struct StubEmbedder;

#[async_trait::async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> EmbedResult<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
    }

    fn dimensions(&self) -> usize {
        3
    }

    fn name(&self) -> &str {
        "stub"
    }
}

fn new_test_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    (tmp, db)
}

fn write_input(dir: &Path, content: &str) -> std::path::PathBuf {
    let path = dir.join("input.md");
    fs::write(&path, content).expect("write input");
    path
}

fn insert_triple(db: &Database, subject: &str, predicate: &str, object: &str) {
    insert_triple_with_confidence(db, subject, predicate, object, 1.0);
}

fn insert_triple_with_confidence(
    db: &Database,
    subject: &str,
    predicate: &str,
    object: &str,
    confidence: f64,
) {
    let triple = Triple {
        id: build_triple_id(subject, predicate, object),
        subject: subject.to_string(),
        predicate: predicate.to_string(),
        object: object.to_string(),
        valid_from: Some("1700000000".to_string()),
        valid_to: None,
        confidence,
        source_drawer: None,
    };
    db.insert_triple(&triple).expect("insert triple");
}

fn gating(fact_check: AutoFactCheckConfig) -> IngestGatingConfig {
    IngestGatingConfig {
        fact_check,
        ..IngestGatingConfig::default()
    }
}

fn enabled_fact_check(reject_on_contradiction: bool) -> AutoFactCheckConfig {
    AutoFactCheckConfig {
        enabled: true,
        reject_on_contradiction,
        reject_on_stale: false,
        reject_on_similar_name: false,
    }
}

async fn ingest_with_gating(
    db: &Database,
    path: &Path,
    wing: &str,
    gating: &IngestGatingConfig,
) -> mempal::ingest::IngestStats {
    ingest_file_with_options(
        db,
        &StubEmbedder,
        path,
        wing,
        IngestOptions {
            room: Some("decision"),
            gating: Some(gating),
            project_id: Some("test-project"),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest")
}

fn fact_audit_rows(db: &Database) -> Vec<(String, Option<String>, Option<String>)> {
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT decision, label, reason FROM gating_audit WHERE label LIKE 'fact_check.%' ORDER BY created_at, id",
        )
        .expect("prepare audit query");
    stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query audit")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect audit")
}

#[tokio::test]
async fn test_fact_check_contradiction_rejects_when_enabled() {
    let (tmp, db) = new_test_db();
    insert_triple(&db, "Bob", "husband_of", "Alice");
    let path = write_input(tmp.path(), "Bob is Alice's brother.");
    let gating = gating(enabled_fact_check(true));

    let stats = ingest_with_gating(&db, &path, "mempal", &gating).await;

    assert_eq!(stats.dropped_by_gate, 1);
    assert!(stats.drawer_ids.is_empty());
    assert!(
        stats
            .fact_check_warnings
            .iter()
            .any(|w| w.contains("fact_check.relation_contradiction"))
    );
    assert_eq!(db.drawer_count().expect("drawer count"), 0);
    let rows = fact_audit_rows(&db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "skip");
    assert_eq!(
        rows[0].1.as_deref(),
        Some("fact_check.relation_contradiction")
    );
}

#[tokio::test]
async fn test_fact_check_contradiction_allows_higher_confidence_new_fact() {
    let (tmp, db) = new_test_db();
    insert_triple_with_confidence(&db, "Bob", "husband_of", "Alice", 0.3);
    let path = write_input(tmp.path(), "Bob is Alice's brother.");
    let gating = gating(enabled_fact_check(true));

    let stats = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &path,
        "mempal",
        IngestOptions {
            room: Some("decision"),
            gating: Some(&gating),
            project_id: Some("test-project"),
            source_type: Some(SourceType::UserExplicit),
            confidence: Some(0.9),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest");

    assert_eq!(stats.dropped_by_gate, 0);
    assert_eq!(stats.drawer_ids.len(), 1);
    assert!(
        stats
            .fact_check_warnings
            .iter()
            .any(|w| w.contains("new_confidence=0.900")
                && w.contains("existing_confidence=0.300")
                && w.contains("confidence_gap=0.600"))
    );
    let rows = fact_audit_rows(&db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "keep");
    assert_eq!(
        rows[0].1.as_deref(),
        Some("fact_check.relation_contradiction")
    );
}

#[tokio::test]
async fn test_fact_check_contradiction_rejects_equal_confidence_new_fact() {
    let (tmp, db) = new_test_db();
    insert_triple_with_confidence(&db, "Bob", "husband_of", "Alice", 0.5);
    let path = write_input(tmp.path(), "Bob is Alice's brother.");
    let gating = gating(enabled_fact_check(true));

    let stats = ingest_with_gating(&db, &path, "mempal", &gating).await;

    assert_eq!(stats.dropped_by_gate, 1);
    assert!(stats.drawer_ids.is_empty());
}

#[tokio::test]
async fn test_fact_check_contradiction_warns_when_rejection_disabled() {
    let (tmp, db) = new_test_db();
    insert_triple(&db, "Bob", "husband_of", "Alice");
    let path = write_input(tmp.path(), "Bob is Alice's brother.");
    let gating = gating(enabled_fact_check(false));

    let stats = ingest_with_gating(&db, &path, "mempal", &gating).await;

    assert_eq!(stats.dropped_by_gate, 0);
    assert_eq!(stats.drawer_ids.len(), 1);
    assert!(
        stats
            .fact_check_warnings
            .iter()
            .any(|w| w.contains("fact_check.relation_contradiction"))
    );
    let rows = fact_audit_rows(&db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "keep");
    assert_eq!(
        rows[0].1.as_deref(),
        Some("fact_check.relation_contradiction")
    );
}

#[tokio::test]
async fn test_fact_check_clean_passes_without_warnings() {
    let (tmp, db) = new_test_db();
    insert_triple(&db, "Bob", "husband_of", "Alice");
    let path = write_input(tmp.path(), "Bob is Alice's husband.");
    let gating = gating(enabled_fact_check(true));

    let stats = ingest_with_gating(&db, &path, "mempal", &gating).await;

    assert_eq!(stats.dropped_by_gate, 0);
    assert_eq!(stats.drawer_ids.len(), 1);
    assert!(stats.fact_check_warnings.is_empty());
    let rows = fact_audit_rows(&db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "keep");
    assert_eq!(rows[0].1.as_deref(), Some("fact_check.clean"));
}

#[tokio::test]
async fn test_hooks_raw_bypasses_fact_check_gate() {
    let (tmp, db) = new_test_db();
    insert_triple(&db, "Bob", "husband_of", "Alice");
    let path = write_input(tmp.path(), "Bob is Alice's brother.");
    let gating = gating(enabled_fact_check(true));

    let stats = ingest_with_gating(&db, &path, "hooks-raw", &gating).await;

    assert_eq!(stats.dropped_by_gate, 0);
    assert_eq!(stats.drawer_ids.len(), 1);
    assert!(stats.fact_check_warnings.is_empty());
    assert!(fact_audit_rows(&db).is_empty());
}

#[tokio::test]
async fn test_fact_check_engine_error_fails_open_with_warning() {
    let (tmp, db) = new_test_db();
    db.conn()
        .execute_batch("DROP TABLE triples;")
        .expect("drop triples");
    let path = write_input(tmp.path(), "Bob is Alice's brother.");
    let gating = gating(enabled_fact_check(true));

    let stats = ingest_with_gating(&db, &path, "mempal", &gating).await;

    assert_eq!(stats.dropped_by_gate, 0);
    assert_eq!(stats.drawer_ids.len(), 1);
    assert!(
        stats
            .fact_check_warnings
            .iter()
            .any(|w| w.contains("fact_check.error"))
    );
    let rows = fact_audit_rows(&db);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, "keep");
    assert_eq!(rows[0].1.as_deref(), Some("fact_check.error"));
}

#[tokio::test]
async fn test_rejected_replacement_does_not_supersede_old_drawer() {
    let (tmp, db) = new_test_db();
    let old_path = write_input(tmp.path(), "old fact remains active");
    let old = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &old_path,
        "mempal",
        IngestOptions {
            room: Some("decision"),
            project_id: Some("test-project"),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest old drawer");
    let old_id = old.drawer_ids.first().expect("old drawer id").clone();

    insert_triple(&db, "Bob", "husband_of", "Alice");
    let replacement_path = write_input(tmp.path(), "Bob is Alice's brother.");
    let gating = gating(enabled_fact_check(true));

    let stats = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &replacement_path,
        "mempal",
        IngestOptions {
            room: Some("decision"),
            gating: Some(&gating),
            project_id: Some("test-project"),
            supersedes: Some(&old_id),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest rejected replacement");

    assert_eq!(stats.dropped_by_gate, 1);
    assert!(stats.drawer_ids.is_empty());
    assert!(stats.superseded_drawer_id.is_none());
    assert!(
        db.get_drawer(&old_id).expect("old lookup").is_some(),
        "old drawer must remain active when every replacement chunk is rejected"
    );
}

#[tokio::test]
async fn test_duplicate_replacement_supersedes_old_drawer() {
    let (tmp, db) = new_test_db();
    let old_path = write_input(tmp.path(), "stale duplicate replacement source");
    let old = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &old_path,
        "mempal",
        IngestOptions {
            room: Some("decision"),
            project_id: Some("test-project"),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest old drawer");
    let old_id = old.drawer_ids.first().expect("old drawer id").clone();

    let canonical_path = write_input(tmp.path(), "canonical replacement fact");
    let canonical = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &canonical_path,
        "mempal",
        IngestOptions {
            room: Some("decision"),
            project_id: Some("test-project"),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest canonical drawer");
    let canonical_id = canonical
        .drawer_ids
        .first()
        .expect("canonical drawer id")
        .clone();

    let replacement_path = write_input(tmp.path(), "canonical replacement fact");
    let stats = ingest_file_with_options(
        &db,
        &StubEmbedder,
        &replacement_path,
        "mempal",
        IngestOptions {
            room: Some("decision"),
            project_id: Some("test-project"),
            supersedes: Some(&old_id),
            ..IngestOptions::default()
        },
    )
    .await
    .expect("ingest duplicate replacement");

    assert_eq!(stats.drawer_ids, vec![canonical_id.clone()]);
    assert_eq!(stats.superseded_drawer_id.as_deref(), Some(old_id.as_str()));
    assert!(db.get_drawer(&old_id).expect("old lookup").is_none());
    assert!(
        db.get_drawer(&canonical_id)
            .expect("canonical lookup")
            .is_some()
    );
}

#[tokio::test]
async fn test_fact_check_config_disabled_skips_gate_entirely() {
    let (tmp, db) = new_test_db();
    insert_triple(&db, "Bob", "husband_of", "Alice");
    let path = write_input(tmp.path(), "Bob is Alice's brother.");
    let gating = gating(AutoFactCheckConfig::default());

    let stats = ingest_with_gating(&db, &path, "mempal", &gating).await;

    assert_eq!(stats.dropped_by_gate, 0);
    assert_eq!(stats.drawer_ids.len(), 1);
    assert!(stats.fact_check_warnings.is_empty());
    assert!(fact_audit_rows(&db).is_empty());
}
