//! Integration tests for P12 stage-1 mind-model bootstrap schema/core work.

use std::sync::Arc;
use std::{fs, path::Path};

use async_trait::async_trait;
use mempal::core::types::{
    AnchorKind, Drawer, KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind, Provenance,
    SourceType, TriggerHints,
};
use mempal::core::{anchor, db::Database};
use mempal::embed::{Embedder, EmbedderFactory};
use mempal::mcp::MempalMcpServer;
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

fn create_v4_db(path: &std::path::Path) {
    let conn = Connection::open(path).expect("open v4 db");
    conn.execute_batch(
        r#"
        PRAGMA foreign_keys = ON;

        CREATE TABLE drawers (
            id TEXT PRIMARY KEY,
            content TEXT NOT NULL,
            wing TEXT NOT NULL,
            room TEXT,
            source_file TEXT,
            source_type TEXT NOT NULL CHECK(source_type IN ('project', 'conversation', 'manual')),
            added_at TEXT NOT NULL,
            chunk_index INTEGER,
            deleted_at TEXT,
            importance INTEGER DEFAULT 0
        );

        CREATE TABLE triples (
            id TEXT PRIMARY KEY,
            subject TEXT NOT NULL,
            predicate TEXT NOT NULL,
            object TEXT NOT NULL,
            valid_from TEXT,
            valid_to TEXT,
            confidence REAL DEFAULT 1.0,
            source_drawer TEXT REFERENCES drawers(id)
        );

        CREATE TABLE taxonomy (
            wing TEXT NOT NULL,
            room TEXT NOT NULL DEFAULT '',
            display_name TEXT,
            keywords TEXT,
            PRIMARY KEY (wing, room)
        );

        CREATE INDEX idx_drawers_wing ON drawers(wing);
        CREATE INDEX idx_drawers_wing_room ON drawers(wing, room);
        CREATE INDEX idx_drawers_deleted_at ON drawers(deleted_at);
        CREATE INDEX idx_triples_subject ON triples(subject);
        CREATE INDEX idx_triples_object ON triples(object);

        CREATE VIRTUAL TABLE drawers_fts USING fts5(
            content,
            content='drawers',
            content_rowid='rowid'
        );

        CREATE TRIGGER drawers_ai AFTER INSERT ON drawers BEGIN
            INSERT INTO drawers_fts(rowid, content) VALUES (new.rowid, new.content);
        END;

        CREATE TRIGGER drawers_au_softdelete AFTER UPDATE OF deleted_at ON drawers
            WHEN new.deleted_at IS NOT NULL AND old.deleted_at IS NULL BEGIN
            INSERT INTO drawers_fts(drawers_fts, rowid, content)
            VALUES ('delete', old.rowid, old.content);
        END;

        PRAGMA user_version = 4;
        "#,
    )
    .expect("apply v4 schema");
}

fn new_db() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db)
}

struct StubEmbedderFactory {
    vector: Vec<f32>,
}

struct StubEmbedder {
    vector: Vec<f32>,
}

#[async_trait]
impl EmbedderFactory for StubEmbedderFactory {
    async fn build(&self) -> mempal::embed::Result<Box<dyn Embedder>> {
        Ok(Box::new(StubEmbedder {
            vector: self.vector.clone(),
        }))
    }
}

#[async_trait]
impl Embedder for StubEmbedder {
    async fn embed(&self, texts: &[&str]) -> mempal::embed::Result<Vec<Vec<f32>>> {
        Ok(texts.iter().map(|_| self.vector.clone()).collect())
    }

    fn dimensions(&self) -> usize {
        self.vector.len()
    }

    fn name(&self) -> &str {
        "stub"
    }
}

fn setup_mcp_server() -> (TempDir, Database, MempalMcpServer) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    let server = MempalMcpServer::new_with_factory(
        db_path,
        Arc::new(StubEmbedderFactory {
            vector: vec![0.1, 0.2, 0.3],
        }),
    );
    (tmp, db, server)
}

fn init_git_repo(path: &Path) {
    std::process::Command::new("git")
        .arg("init")
        .current_dir(path)
        .output()
        .expect("git init should run");
}

#[test]
fn test_migration_backfills_legacy_drawers_with_bootstrap_defaults() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    create_v4_db(&db_path);

    {
        let conn = Connection::open(&db_path).expect("reopen v4 db");
        conn.execute(
            r#"
            INSERT INTO drawers (
                id,
                content,
                wing,
                room,
                source_file,
                source_type,
                added_at,
                chunk_index,
                importance
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            "#,
            (
                "drawer_legacy_001",
                "Legacy evidence body",
                "mempal",
                Some("bootstrap"),
                Some("docs/specs/legacy.md"),
                "project",
                "1710000000",
                Some(0_i64),
                4_i32,
            ),
        )
        .expect("insert legacy drawer");
    }

    let db = Database::open(&db_path).expect("migrate db to latest");
    assert_eq!(db.schema_version().expect("schema version"), 5);

    let drawer = db
        .get_drawer("drawer_legacy_001")
        .expect("load drawer")
        .expect("drawer exists");

    assert_eq!(drawer.memory_kind, MemoryKind::Evidence);
    assert_eq!(drawer.domain, MemoryDomain::Project);
    assert_eq!(drawer.field, "general");
    assert_eq!(drawer.anchor_kind, AnchorKind::Repo);
    assert_eq!(drawer.anchor_id, "repo://legacy");
    assert_eq!(drawer.parent_anchor_id, None);
    assert_eq!(drawer.provenance, Some(Provenance::Research));
    assert_eq!(drawer.statement, None);
    assert_eq!(drawer.tier, None);
    assert_eq!(drawer.status, None);
    assert!(drawer.supporting_refs.is_empty());
    assert!(drawer.counterexample_refs.is_empty());
    assert!(drawer.teaching_refs.is_empty());
    assert!(drawer.verification_refs.is_empty());
    assert_eq!(drawer.scope_constraints, None);
    assert_eq!(drawer.trigger_hints, None);
}

#[test]
fn test_global_anchor_rejected_for_non_global_domain() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");

    let drawer = Drawer {
        id: "drawer_invalid_anchor".to_string(),
        content: "repo-local note".to_string(),
        wing: "mempal".to_string(),
        room: Some("bootstrap".to_string()),
        source_file: Some("tests://mind-model".to_string()),
        source_type: SourceType::Manual,
        added_at: "1710001234".to_string(),
        chunk_index: None,
        importance: 0,
        memory_kind: MemoryKind::Evidence,
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        anchor_kind: AnchorKind::Global,
        anchor_id: "global://all".to_string(),
        parent_anchor_id: None,
        provenance: Some(Provenance::Human),
        statement: None,
        tier: None,
        status: None,
        supporting_refs: Vec::new(),
        counterexample_refs: Vec::new(),
        teaching_refs: Vec::new(),
        verification_refs: Vec::new(),
        scope_constraints: None,
        trigger_hints: None,
    };

    let error = db
        .insert_drawer(&drawer)
        .expect_err("global anchor should reject non-global domain");
    let message = error.to_string();
    assert!(
        message.contains("global") && message.contains("domain"),
        "unexpected error: {message}"
    );
}

#[test]
fn test_insert_load_roundtrip_preserves_json_metadata_and_read_paths() {
    let (_tmp, db) = new_db();
    let drawer = Drawer {
        id: "drawer_knowledge_roundtrip".to_string(),
        content: "Detailed rationale body".to_string(),
        wing: "mempal".to_string(),
        room: Some("bootstrap".to_string()),
        source_file: Some("knowledge://project/bootstrap/typed-drawer".to_string()),
        source_type: SourceType::Manual,
        added_at: "1710002000".to_string(),
        chunk_index: Some(0),
        importance: 3,
        memory_kind: MemoryKind::Knowledge,
        domain: MemoryDomain::Project,
        field: anchor::DEFAULT_FIELD.to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: anchor::LEGACY_REPO_ANCHOR_ID.to_string(),
        parent_anchor_id: None,
        provenance: Some(Provenance::Human),
        statement: Some("Typed drawers persist structured metadata.".to_string()),
        tier: Some(KnowledgeTier::Shu),
        status: Some(KnowledgeStatus::Promoted),
        supporting_refs: vec!["drawer_ev_001".to_string(), "drawer_ev_002".to_string()],
        counterexample_refs: vec!["drawer_cex_001".to_string()],
        teaching_refs: Vec::new(),
        verification_refs: vec!["drawer_verify_001".to_string()],
        scope_constraints: Some("Task 1 only".to_string()),
        trigger_hints: Some(TriggerHints {
            intent_tags: vec!["schema".to_string(), "bootstrap".to_string()],
            workflow_bias: vec!["tdd".to_string()],
            tool_needs: vec!["cargo-check".to_string()],
        }),
    };

    db.insert_drawer(&drawer).expect("insert drawer");

    let loaded = db
        .get_drawer(&drawer.id)
        .expect("get drawer")
        .expect("drawer exists");
    assert_eq!(loaded.supporting_refs, drawer.supporting_refs);
    assert_eq!(loaded.counterexample_refs, drawer.counterexample_refs);
    assert_eq!(loaded.trigger_hints, drawer.trigger_hints);

    let top = db.top_drawers(5).expect("top drawers");
    let top_loaded = top
        .into_iter()
        .find(|candidate| candidate.id == drawer.id)
        .expect("drawer present in top_drawers");
    assert_eq!(top_loaded.supporting_refs, drawer.supporting_refs);
    assert_eq!(top_loaded.counterexample_refs, drawer.counterexample_refs);
    assert_eq!(top_loaded.trigger_hints, drawer.trigger_hints);
}

#[test]
fn test_read_path_rejects_non_array_or_non_string_list_payloads() {
    let (_tmp, db) = new_db();
    db.conn()
        .execute(
            r#"
            INSERT INTO drawers (
                id, content, wing, room, source_file, source_type, added_at, chunk_index, importance,
                memory_kind, domain, field, anchor_kind, anchor_id, parent_anchor_id, provenance,
                statement, tier, status, supporting_refs, counterexample_refs, teaching_refs,
                verification_refs, scope_constraints, trigger_hints
            )
            VALUES (?1, ?2, ?3, NULL, NULL, ?4, ?5, NULL, 0, ?6, ?7, ?8, ?9, ?10, NULL, ?11,
                    NULL, NULL, NULL, ?12, '[]', '[]', '[]', NULL, NULL)
            "#,
            (
                "drawer_bad_json",
                "bad payload",
                "mempal",
                "manual",
                "1710003000",
                "evidence",
                "project",
                anchor::DEFAULT_FIELD,
                "repo",
                anchor::LEGACY_REPO_ANCHOR_ID,
                "human",
                r#"["ok", 42]"#,
            ),
        )
        .expect("insert malformed drawer");

    let error = db
        .get_drawer("drawer_bad_json")
        .expect_err("malformed list payload should fail");
    let message = error.to_string();
    assert!(
        message.contains("JSON") || message.contains("list"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn test_mcp_ingest_defaults_to_evidence_drawer_bootstrap_metadata() {
    let (_tmp, db, server) = setup_mcp_server();
    let response = server
        .ingest_json_for_test(json!({
            "content": "Bootstrap evidence body",
            "wing": "mempal",
            "room": "bootstrap",
            "source": "notes://bootstrap",
            "importance": 2
        }))
        .await
        .expect("ingest should succeed");

    let drawer = db
        .get_drawer(&response.drawer_id)
        .expect("load drawer")
        .expect("drawer exists");

    assert_eq!(drawer.memory_kind, MemoryKind::Evidence);
    assert_eq!(drawer.domain, MemoryDomain::Project);
    assert_eq!(drawer.field, "general");
    assert_eq!(drawer.provenance, Some(Provenance::Human));
    assert_eq!(drawer.statement, None);
    assert_eq!(drawer.tier, None);
    assert_eq!(drawer.status, None);
}

#[tokio::test]
async fn test_evidence_drawer_rejects_knowledge_only_fields() {
    let (_tmp, _db, server) = setup_mcp_server();
    let error = server
        .ingest_json_for_test(json!({
            "content": "Evidence should not carry knowledge governance metadata",
            "wing": "mempal",
            "memory_kind": "evidence",
            "scope_constraints": "Task 2 only"
        }))
        .await
        .expect_err("knowledge-only fields should be rejected");
    let message = error.to_string();

    assert!(
        message.contains("evidence") && message.contains("knowledge-only"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn test_knowledge_drawer_requires_statement_and_supporting_refs() {
    let (_tmp, _db, server) = setup_mcp_server();
    let error = server
        .ingest_json_for_test(json!({
            "content": "Knowledge body without the bootstrap metadata contract",
            "wing": "mempal",
            "memory_kind": "knowledge",
            "domain": "skill",
            "field": "debugging",
            "tier": "shu",
            "status": "promoted"
        }))
        .await
        .expect_err("knowledge drawers should require statement and supporting refs");
    let message = error.to_string();

    assert!(
        message.contains("knowledge") && message.contains("statement"),
        "unexpected error: {message}"
    );
    assert!(
        message.contains("supporting_refs"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn test_knowledge_drawer_rejects_non_drawer_supporting_refs() {
    let (_tmp, _db, server) = setup_mcp_server();
    let error = server
        .ingest_json_for_test(json!({
            "content": "Knowledge body with malformed refs",
            "wing": "mempal",
            "memory_kind": "knowledge",
            "statement": "Debug by reproducing before patching.",
            "tier": "shu",
            "status": "promoted",
            "supporting_refs": ["not-a-drawer-id"]
        }))
        .await
        .expect_err("knowledge drawers should reject malformed supporting refs");
    let message = error.to_string();

    assert!(
        message.contains("supporting_refs") && message.contains("drawer"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn test_knowledge_drawer_rejects_non_drawer_other_ref_lists() {
    let (_tmp, _db, server) = setup_mcp_server();
    for field in ["counterexample_refs", "teaching_refs", "verification_refs"] {
        let error = server
            .ingest_json_for_test(json!({
                "content": format!("Knowledge body with malformed {field}"),
                "wing": "mempal",
                "memory_kind": "knowledge",
                "statement": "Debug by reproducing before patching.",
                "tier": "shu",
                "status": "promoted",
                "supporting_refs": ["drawer_ev_001"],
                field: ["not-a-drawer-id"]
            }))
            .await
            .expect_err("knowledge drawers should reject malformed ref lists");
        let message = error.to_string();

        assert!(
            message.contains(field) && message.contains("drawer"),
            "unexpected error for {field}: {message}"
        );
    }
}

#[tokio::test]
async fn test_dao_tian_rejects_noncanonical_status() {
    let (_tmp, _db, server) = setup_mcp_server();
    let error = server
        .ingest_json_for_test(json!({
            "content": "Canonical epistemic policy",
            "wing": "mempal",
            "memory_kind": "knowledge",
            "statement": "Evidence precedes assertion.",
            "tier": "dao_tian",
            "status": "candidate",
            "supporting_refs": ["drawer_ev_001"]
        }))
        .await
        .expect_err("dao_tian candidate should be rejected");
    let message = error.to_string();

    assert!(
        message.contains("dao_tian")
            && message.contains("canonical")
            && message.contains("demoted"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn test_mcp_ingest_same_content_different_anchors_stays_distinct() {
    let (_tmp, db, server) = setup_mcp_server();
    let first = server
        .ingest_json_for_test(json!({
            "content": "Anchor-local memory body",
            "wing": "mempal",
            "memory_kind": "evidence",
            "anchor_kind": "repo",
            "anchor_id": "repo://anchor-a"
        }))
        .await
        .expect("first ingest should succeed");
    let second = server
        .ingest_json_for_test(json!({
            "content": "Anchor-local memory body",
            "wing": "mempal",
            "memory_kind": "knowledge",
            "statement": "Anchor-local memory body.",
            "tier": "shu",
            "status": "promoted",
            "supporting_refs": ["drawer_ev_001"],
            "anchor_kind": "repo",
            "anchor_id": "repo://anchor-b"
        }))
        .await
        .expect("second ingest should succeed");

    assert_ne!(first.drawer_id, second.drawer_id);
    let first_drawer = db
        .get_drawer(&first.drawer_id)
        .expect("load first drawer")
        .expect("first drawer exists");
    let second_drawer = db
        .get_drawer(&second.drawer_id)
        .expect("load second drawer")
        .expect("second drawer exists");
    assert_ne!(first_drawer.anchor_id, second_drawer.anchor_id);
    assert_ne!(first_drawer.memory_kind, second_drawer.memory_kind);
}

#[tokio::test]
async fn test_mcp_ingest_rejects_malformed_explicit_anchor() {
    let (_tmp, _db, server) = setup_mcp_server();
    let error = server
        .ingest_json_for_test(json!({
            "content": "Malformed explicit anchor",
            "wing": "mempal",
            "anchor_kind": "worktree",
            "anchor_id": "/tmp/repo"
        }))
        .await
        .expect_err("malformed explicit anchor should fail");
    let message = error.to_string();

    assert!(
        message.contains("invalid explicit anchor") && message.contains("worktree://"),
        "unexpected error: {message}"
    );
}

#[tokio::test]
async fn test_evidence_drawer_accepts_explicit_runtime_or_research_provenance() {
    let (_tmp, db, server) = setup_mcp_server();
    let runtime = server
        .ingest_json_for_test(json!({
            "content": "Runtime evidence body",
            "wing": "mempal",
            "memory_kind": "evidence",
            "provenance": "runtime",
            "anchor_kind": "repo",
            "anchor_id": "repo://runtime"
        }))
        .await
        .expect("runtime provenance evidence should succeed");
    let research = server
        .ingest_json_for_test(json!({
            "content": "Research evidence body",
            "wing": "mempal",
            "memory_kind": "evidence",
            "provenance": "research",
            "anchor_kind": "repo",
            "anchor_id": "repo://research"
        }))
        .await
        .expect("research provenance evidence should succeed");

    assert_eq!(
        db.get_drawer(&runtime.drawer_id)
            .expect("load runtime drawer")
            .expect("runtime drawer exists")
            .provenance,
        Some(Provenance::Runtime)
    );
    assert_eq!(
        db.get_drawer(&research.drawer_id)
            .expect("load research drawer")
            .expect("research drawer exists")
            .provenance,
        Some(Provenance::Research)
    );
}

#[tokio::test]
async fn test_anchor_derivation_handles_git_and_non_git_cwd() {
    let (tmp, db, server) = setup_mcp_server();
    let git_root = tmp.path().join("repo");
    fs::create_dir_all(&git_root).expect("create git root");
    init_git_repo(&git_root);

    let non_git = tmp.path().join("standalone");
    fs::create_dir_all(&non_git).expect("create standalone dir");

    let git_response = server
        .ingest_json_for_test(json!({
            "content": "Git anchored evidence",
            "wing": "mempal",
            "cwd": git_root.to_string_lossy()
        }))
        .await
        .expect("git cwd ingest should succeed");
    let non_git_response = server
        .ingest_json_for_test(json!({
            "content": "Standalone anchored evidence",
            "wing": "mempal",
            "cwd": non_git.to_string_lossy()
        }))
        .await
        .expect("non-git cwd ingest should succeed");

    let git_drawer = db
        .get_drawer(&git_response.drawer_id)
        .expect("load git drawer")
        .expect("git drawer exists");
    let non_git_drawer = db
        .get_drawer(&non_git_response.drawer_id)
        .expect("load non-git drawer")
        .expect("non-git drawer exists");

    assert_eq!(git_drawer.anchor_kind, AnchorKind::Worktree);
    assert!(git_drawer.anchor_id.starts_with("worktree://"));
    assert!(
        git_drawer
            .parent_anchor_id
            .as_deref()
            .is_some_and(|value| value.starts_with("repo://"))
    );

    assert_eq!(non_git_drawer.anchor_kind, AnchorKind::Worktree);
    assert!(non_git_drawer.anchor_id.starts_with("worktree://"));
    assert_eq!(non_git_drawer.parent_anchor_id, None);
}

#[tokio::test]
async fn test_knowledge_drawer_gets_synthetic_knowledge_source_uri() {
    let (_tmp, db, server) = setup_mcp_server();
    let response = server
        .ingest_json_for_test(json!({
            "content": "Use source-backed verification before load-bearing claims.",
            "wing": "mempal",
            "memory_kind": "knowledge",
            "domain": "skill",
            "field": "debugging",
            "statement": "Debug by reproducing before patching.",
            "tier": "dao_tian",
            "status": "canonical",
            "supporting_refs": ["drawer_ev_001"]
        }))
        .await
        .expect("knowledge ingest should succeed");

    let drawer = db
        .get_drawer(&response.drawer_id)
        .expect("load knowledge drawer")
        .expect("knowledge drawer exists");
    let source = drawer.source_file.expect("knowledge source uri");
    assert!(source.starts_with("knowledge://skill/debugging/dao_tian/"));
}

#[tokio::test]
async fn test_bootstrap_identity_ignores_ref_and_hint_order() {
    let (_tmp, _db, server) = setup_mcp_server();
    let first = server
        .ingest_json_for_test(json!({
            "content": "Order-insensitive bootstrap identity",
            "wing": "mempal",
            "memory_kind": "knowledge",
            "domain": "skill",
            "field": "debugging",
            "statement": "Debug by reproducing before patching.",
            "tier": "shu",
            "status": "promoted",
            "supporting_refs": ["drawer_ev_002", "drawer_ev_001"],
            "counterexample_refs": ["drawer_cex_002", "drawer_cex_001"],
            "teaching_refs": ["drawer_teach_002", "drawer_teach_001"],
            "verification_refs": ["drawer_verify_002", "drawer_verify_001"],
            "trigger_hints": {
                "intent_tags": ["zeta", "alpha"],
                "workflow_bias": ["later", "earlier"],
                "tool_needs": ["tool-b", "tool-a"]
            },
            "anchor_kind": "repo",
            "anchor_id": "repo://identity"
        }))
        .await
        .expect("first dry-run-ish identity request should succeed");
    let second = server
        .ingest_json_for_test(json!({
            "content": "Order-insensitive bootstrap identity",
            "wing": "mempal",
            "memory_kind": "knowledge",
            "domain": "skill",
            "field": "debugging",
            "statement": "Debug by reproducing before patching.",
            "tier": "shu",
            "status": "promoted",
            "supporting_refs": ["drawer_ev_001", "drawer_ev_002"],
            "counterexample_refs": ["drawer_cex_001", "drawer_cex_002"],
            "teaching_refs": ["drawer_teach_001", "drawer_teach_002"],
            "verification_refs": ["drawer_verify_001", "drawer_verify_002"],
            "trigger_hints": {
                "intent_tags": ["alpha", "zeta"],
                "workflow_bias": ["earlier", "later"],
                "tool_needs": ["tool-a", "tool-b"]
            },
            "anchor_kind": "repo",
            "anchor_id": "repo://identity",
            "dry_run": true
        }))
        .await
        .expect("second identity request should succeed");

    assert_eq!(first.drawer_id, second.drawer_id);
}
