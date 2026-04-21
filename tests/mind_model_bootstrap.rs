//! Integration tests for P12 stage-1 mind-model bootstrap schema/core work.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::Arc;
use std::thread;
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

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn init_git_repo(path: &Path) {
    Command::new("git")
        .arg("init")
        .current_dir(path)
        .output()
        .expect("git init should run");
    fs::write(path.join("README.md"), "seed\n").expect("write seed file");
    Command::new("git")
        .args(["add", "README.md"])
        .current_dir(path)
        .output()
        .expect("git add should run");
    Command::new("git")
        .args([
            "-c",
            "user.name=Test User",
            "-c",
            "user.email=test@example.com",
            "commit",
            "-m",
            "init",
        ])
        .current_dir(path)
        .output()
        .expect("git commit should run");
}

fn setup_cli_home() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_dir = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_dir).expect("create mempal home");
    let db = Database::open(&mempal_dir.join("palace.db")).expect("open cli db");
    (tmp, db)
}

fn start_openai_embedding_stub(vector: Vec<f32>) -> (String, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind embedding stub");
    listener
        .set_nonblocking(true)
        .expect("set embedding stub nonblocking");
    let address = listener.local_addr().expect("local addr");

    let handle = thread::spawn(move || {
        let (mut stream, _) = (0..50)
            .find_map(|_| match listener.accept() {
                Ok(connection) => Some(connection),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(std::time::Duration::from_millis(100));
                    None
                }
                Err(error) => panic!("accept request: {error}"),
            })
            .expect("embedding stub timed out waiting for request");
        let mut request = [0_u8; 4096];
        let bytes_read = stream.read(&mut request).expect("read embedding request");
        assert!(
            bytes_read > 0 && String::from_utf8_lossy(&request[..bytes_read]).contains("POST"),
            "expected HTTP POST request"
        );

        let body = serde_json::to_string(&json!({
            "data": [{ "embedding": vector }]
        }))
        .expect("serialize response body");
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write embedding response");
    });

    (format!("http://{address}/v1/embeddings"), handle)
}

fn vector_of(dimensions: usize, value: f32) -> Vec<f32> {
    vec![value; dimensions]
}

fn bootstrap_drawer(
    id: &str,
    content: &str,
    memory_kind: MemoryKind,
    tier: Option<KnowledgeTier>,
    status: Option<KnowledgeStatus>,
    statement: Option<&str>,
) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: "mempal".to_string(),
        room: Some("bootstrap".to_string()),
        source_file: Some(match memory_kind {
            MemoryKind::Evidence => format!("tests://{id}"),
            MemoryKind::Knowledge => format!("knowledge://project/bootstrap/{id}"),
        }),
        source_type: SourceType::Manual,
        added_at: "1710009999".to_string(),
        chunk_index: Some(0),
        importance: 2,
        memory_kind: memory_kind.clone(),
        domain: MemoryDomain::Project,
        field: anchor::DEFAULT_FIELD.to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: format!("repo://{id}"),
        parent_anchor_id: None,
        provenance: match memory_kind {
            MemoryKind::Evidence => Some(Provenance::Human),
            MemoryKind::Knowledge => None,
        },
        statement: statement.map(ToOwned::to_owned),
        tier,
        status,
        supporting_refs: if matches!(memory_kind, MemoryKind::Knowledge) {
            vec!["drawer_ev_search_source".to_string()]
        } else {
            Vec::new()
        },
        counterexample_refs: Vec::new(),
        teaching_refs: Vec::new(),
        verification_refs: Vec::new(),
        scope_constraints: None,
        trigger_hints: None,
    }
}

fn insert_search_fixture(db: &Database, drawer: &Drawer, vector: &[f32]) {
    db.insert_drawer(drawer).expect("insert search drawer");
    db.insert_vector(&drawer.id, vector)
        .expect("insert search vector");
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
async fn test_knowledge_drawer_keeps_statement_separate_from_content() {
    let (_tmp, db, server) = setup_mcp_server();
    let statement = "Debug by reproducing before patching.";
    let content = "Start from a concrete reproduction, then isolate scope before patching.";

    let response = server
        .ingest_json_for_test(json!({
            "content": content,
            "wing": "mempal",
            "memory_kind": "knowledge",
            "domain": "skill",
            "field": "debugging",
            "statement": statement,
            "tier": "shu",
            "status": "promoted",
            "supporting_refs": ["drawer_ev_001"]
        }))
        .await
        .expect("knowledge ingest should succeed");

    let drawer = db
        .get_drawer(&response.drawer_id)
        .expect("load knowledge drawer")
        .expect("knowledge drawer exists");

    assert_eq!(drawer.memory_kind, MemoryKind::Knowledge);
    assert_eq!(drawer.domain, MemoryDomain::Skill);
    assert_eq!(drawer.field, "debugging");
    assert_eq!(drawer.statement.as_deref(), Some(statement));
    assert_eq!(drawer.content, content);
    assert_ne!(drawer.statement.as_deref(), Some(drawer.content.as_str()));
    assert_eq!(drawer.tier, Some(KnowledgeTier::Shu));
    assert_eq!(drawer.status, Some(KnowledgeStatus::Promoted));
    assert_eq!(drawer.supporting_refs, vec!["drawer_ev_001"]);
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
async fn test_git_worktree_derives_worktree_anchor_and_repo_parent() {
    let (tmp, db, server) = setup_mcp_server();
    let repo_root = tmp.path().join("repo");
    fs::create_dir_all(&repo_root).expect("create git root");
    init_git_repo(&repo_root);
    let worktree = tmp.path().join("repo-worktree");
    let output = Command::new("git")
        .args([
            "worktree",
            "add",
            "-b",
            "mind-model-bootstrap",
            worktree.to_str().expect("utf8 worktree path"),
        ])
        .current_dir(&repo_root)
        .output()
        .expect("git worktree add should run");
    assert!(
        output.status.success(),
        "git worktree add failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let response = server
        .ingest_json_for_test(json!({
            "content": "Git anchored evidence",
            "wing": "mempal",
            "cwd": worktree.to_string_lossy()
        }))
        .await
        .expect("git cwd ingest should succeed");

    let drawer = db
        .get_drawer(&response.drawer_id)
        .expect("load git drawer")
        .expect("git drawer exists");

    let expected_worktree = format!(
        "worktree://{}",
        worktree
            .canonicalize()
            .expect("canonicalize worktree")
            .display()
    );
    let expected_repo_parent = format!(
        "repo://{}",
        repo_root
            .join(".git")
            .canonicalize()
            .expect("canonicalize git dir")
            .display()
    );

    assert_eq!(drawer.anchor_kind, AnchorKind::Worktree);
    assert_eq!(drawer.anchor_id, expected_worktree);
    assert_eq!(
        drawer.parent_anchor_id.as_deref(),
        Some(expected_repo_parent.as_str())
    );
}

#[tokio::test]
async fn test_non_git_cwd_falls_back_to_standalone_worktree_anchor() {
    let (tmp, db, server) = setup_mcp_server();
    let non_git = tmp.path().join("standalone");
    fs::create_dir_all(&non_git).expect("create standalone dir");

    let response = server
        .ingest_json_for_test(json!({
            "content": "Standalone anchored evidence",
            "wing": "mempal",
            "cwd": non_git.to_string_lossy()
        }))
        .await
        .expect("non-git cwd ingest should succeed");

    let drawer = db
        .get_drawer(&response.drawer_id)
        .expect("load non-git drawer")
        .expect("non-git drawer exists");

    let expected_worktree = format!(
        "worktree://{}",
        non_git
            .canonicalize()
            .expect("canonicalize standalone dir")
            .display()
    );

    assert_eq!(drawer.anchor_kind, AnchorKind::Worktree);
    assert_eq!(drawer.anchor_id, expected_worktree);
    assert_eq!(drawer.parent_anchor_id, None);
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

#[tokio::test]
async fn test_search_result_exposes_knowledge_metadata_without_rewriting_content() {
    let (_tmp, db, server) = setup_mcp_server();
    let raw_content =
        "Raw knowledge body: preserve this exact content even when statement differs.";
    let statement = "Promote the normalized statement, not the stored body.";
    let drawer = bootstrap_drawer(
        "drawer_search_knowledge",
        raw_content,
        MemoryKind::Knowledge,
        Some(KnowledgeTier::Shu),
        Some(KnowledgeStatus::Promoted),
        Some(statement),
    );
    insert_search_fixture(&db, &drawer, &[0.1, 0.2, 0.3]);

    let response = server
        .search_json_for_test(json!({
            "query": "preserve exact content",
            "wing": "mempal",
            "room": "bootstrap",
            "top_k": 5
        }))
        .await
        .expect("search should succeed");

    let result = response.results.first().expect("search result");
    assert_eq!(result.drawer_id, drawer.id);
    assert_eq!(result.content, raw_content);
    assert_ne!(result.content, statement);
    assert_eq!(result.memory_kind, "knowledge");
    assert_eq!(result.domain, "project");
    assert_eq!(result.field, anchor::DEFAULT_FIELD);
    assert_eq!(result.statement.as_deref(), Some(statement));
    assert_eq!(result.tier.as_deref(), Some("shu"));
    assert_eq!(result.status.as_deref(), Some("promoted"));
    assert_eq!(result.anchor_kind, "repo");
    assert_eq!(result.anchor_id, "repo://drawer_search_knowledge");
    assert_eq!(result.parent_anchor_id, None);
}

#[tokio::test]
async fn test_search_filters_by_memory_kind_and_tier_without_rerank_changes() {
    let (_tmp, db, server) = setup_mcp_server();
    let evidence = bootstrap_drawer(
        "drawer_search_evidence",
        "alpha alpha alpha evidence body",
        MemoryKind::Evidence,
        None,
        None,
        None,
    );
    let knowledge_shu = bootstrap_drawer(
        "drawer_search_knowledge_shu",
        "alpha alpha knowledge shu body",
        MemoryKind::Knowledge,
        Some(KnowledgeTier::Shu),
        Some(KnowledgeStatus::Promoted),
        Some("Knowledge shu statement"),
    );
    let knowledge_qi = bootstrap_drawer(
        "drawer_search_knowledge_qi",
        "alpha knowledge qi body",
        MemoryKind::Knowledge,
        Some(KnowledgeTier::Qi),
        Some(KnowledgeStatus::Candidate),
        Some("Knowledge qi statement"),
    );

    insert_search_fixture(&db, &evidence, &[0.1, 0.2, 0.3]);
    insert_search_fixture(&db, &knowledge_shu, &[0.2, 0.2, 0.3]);
    insert_search_fixture(&db, &knowledge_qi, &[0.3, 0.2, 0.3]);

    let unfiltered = server
        .search_json_for_test(json!({
            "query": "alpha",
            "wing": "mempal",
            "room": "bootstrap",
            "top_k": 3
        }))
        .await
        .expect("unfiltered search should succeed");
    let knowledge_only = server
        .search_json_for_test(json!({
            "query": "alpha",
            "wing": "mempal",
            "room": "bootstrap",
            "memory_kind": "knowledge",
            "top_k": 3
        }))
        .await
        .expect("knowledge-only search should succeed");
    let shu_only = server
        .search_json_for_test(json!({
            "query": "alpha",
            "wing": "mempal",
            "room": "bootstrap",
            "memory_kind": "knowledge",
            "tier": "shu",
            "top_k": 3
        }))
        .await
        .expect("shu-only search should succeed");

    let unfiltered_ids: Vec<&str> = unfiltered
        .results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect();
    let knowledge_only_ids: Vec<&str> = knowledge_only
        .results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect();
    let shu_only_ids: Vec<&str> = shu_only
        .results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect();

    assert_eq!(
        unfiltered_ids,
        vec![
            "drawer_search_evidence",
            "drawer_search_knowledge_shu",
            "drawer_search_knowledge_qi"
        ]
    );
    assert_eq!(
        knowledge_only_ids,
        vec!["drawer_search_knowledge_shu", "drawer_search_knowledge_qi"]
    );
    assert_eq!(shu_only_ids, vec!["drawer_search_knowledge_shu"]);
}

#[tokio::test]
async fn test_search_filters_by_domain_field_status_and_anchor_kind() {
    let (_tmp, db, server) = setup_mcp_server();

    let domain_skill = Drawer {
        domain: MemoryDomain::Skill,
        ..bootstrap_drawer(
            "drawer_filter_domain_skill",
            "domain focus domain focus",
            MemoryKind::Knowledge,
            Some(KnowledgeTier::Shu),
            Some(KnowledgeStatus::Promoted),
            Some("Skill-domain statement"),
        )
    };
    let domain_agent = Drawer {
        domain: MemoryDomain::Agent,
        ..bootstrap_drawer(
            "drawer_filter_domain_agent",
            "domain focus domain focus",
            MemoryKind::Knowledge,
            Some(KnowledgeTier::Shu),
            Some(KnowledgeStatus::Promoted),
            Some("Agent-domain statement"),
        )
    };
    let field_debugging = Drawer {
        domain: MemoryDomain::Skill,
        field: "debugging".to_string(),
        ..bootstrap_drawer(
            "drawer_filter_field_debugging",
            "field focus field focus",
            MemoryKind::Knowledge,
            Some(KnowledgeTier::Shu),
            Some(KnowledgeStatus::Promoted),
            Some("Debugging-field statement"),
        )
    };
    let field_tooling = Drawer {
        domain: MemoryDomain::Skill,
        field: "tooling".to_string(),
        ..bootstrap_drawer(
            "drawer_filter_field_tooling",
            "field focus field focus",
            MemoryKind::Knowledge,
            Some(KnowledgeTier::Shu),
            Some(KnowledgeStatus::Promoted),
            Some("Tooling-field statement"),
        )
    };
    let status_promoted = Drawer {
        domain: MemoryDomain::Skill,
        field: "debugging".to_string(),
        status: Some(KnowledgeStatus::Promoted),
        ..bootstrap_drawer(
            "drawer_filter_status_promoted",
            "status focus status focus",
            MemoryKind::Knowledge,
            Some(KnowledgeTier::Shu),
            Some(KnowledgeStatus::Promoted),
            Some("Promoted-status statement"),
        )
    };
    let status_retired = Drawer {
        domain: MemoryDomain::Skill,
        field: "debugging".to_string(),
        status: Some(KnowledgeStatus::Retired),
        ..bootstrap_drawer(
            "drawer_filter_status_retired",
            "status focus status focus",
            MemoryKind::Knowledge,
            Some(KnowledgeTier::Shu),
            Some(KnowledgeStatus::Retired),
            Some("Retired-status statement"),
        )
    };
    let anchor_repo = Drawer {
        domain: MemoryDomain::Skill,
        field: "debugging".to_string(),
        status: Some(KnowledgeStatus::Promoted),
        anchor_kind: AnchorKind::Repo,
        anchor_id: "repo://filter-anchor".to_string(),
        ..bootstrap_drawer(
            "drawer_filter_anchor_repo",
            "anchor focus anchor focus",
            MemoryKind::Knowledge,
            Some(KnowledgeTier::Shu),
            Some(KnowledgeStatus::Promoted),
            Some("Repo-anchor statement"),
        )
    };
    let anchor_worktree = Drawer {
        domain: MemoryDomain::Skill,
        field: "debugging".to_string(),
        status: Some(KnowledgeStatus::Promoted),
        anchor_kind: AnchorKind::Worktree,
        anchor_id: "worktree:///tmp/filter-anchor".to_string(),
        ..bootstrap_drawer(
            "drawer_filter_anchor_worktree",
            "anchor focus anchor focus",
            MemoryKind::Knowledge,
            Some(KnowledgeTier::Shu),
            Some(KnowledgeStatus::Promoted),
            Some("Worktree-anchor statement"),
        )
    };

    for (index, drawer) in [
        &domain_skill,
        &domain_agent,
        &field_debugging,
        &field_tooling,
        &status_promoted,
        &status_retired,
        &anchor_repo,
        &anchor_worktree,
    ]
    .into_iter()
    .enumerate()
    {
        insert_search_fixture(&db, drawer, &[0.1 + index as f32, 0.2, 0.3]);
    }

    let domain_results = server
        .search_json_for_test(json!({
            "query": "domain focus",
            "wing": "mempal",
            "room": "bootstrap",
            "domain": "skill",
            "top_k": 5
        }))
        .await
        .expect("domain-filtered search should succeed");
    let field_results = server
        .search_json_for_test(json!({
            "query": "field focus",
            "wing": "mempal",
            "room": "bootstrap",
            "field": "debugging",
            "top_k": 5
        }))
        .await
        .expect("field-filtered search should succeed");
    let status_results = server
        .search_json_for_test(json!({
            "query": "status focus",
            "wing": "mempal",
            "room": "bootstrap",
            "status": "promoted",
            "top_k": 5
        }))
        .await
        .expect("status-filtered search should succeed");
    let anchor_results = server
        .search_json_for_test(json!({
            "query": "anchor focus",
            "wing": "mempal",
            "room": "bootstrap",
            "anchor_kind": "repo",
            "top_k": 5
        }))
        .await
        .expect("anchor-filtered search should succeed");

    let domain_ids: Vec<&str> = domain_results
        .results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect();
    let field_ids: Vec<&str> = field_results
        .results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect();
    let status_ids: Vec<&str> = status_results
        .results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect();
    let anchor_ids: Vec<&str> = anchor_results
        .results
        .iter()
        .map(|result| result.drawer_id.as_str())
        .collect();

    assert!(domain_ids.contains(&"drawer_filter_domain_skill"));
    assert!(!domain_ids.contains(&"drawer_filter_domain_agent"));
    assert!(
        domain_results
            .results
            .iter()
            .all(|result| result.domain == "skill")
    );

    assert!(field_ids.contains(&"drawer_filter_field_debugging"));
    assert!(!field_ids.contains(&"drawer_filter_field_tooling"));
    assert!(
        field_results
            .results
            .iter()
            .all(|result| result.field == "debugging")
    );

    assert!(status_ids.contains(&"drawer_filter_status_promoted"));
    assert!(!status_ids.contains(&"drawer_filter_status_retired"));
    assert!(
        status_results
            .results
            .iter()
            .all(|result| result.status.as_deref() == Some("promoted"))
    );

    assert!(anchor_ids.contains(&"drawer_filter_anchor_repo"));
    assert!(!anchor_ids.contains(&"drawer_filter_anchor_worktree"));
    assert!(
        anchor_results
            .results
            .iter()
            .all(|result| result.anchor_kind == "repo")
    );
}

#[test]
fn test_cli_search_json_exposes_bootstrap_metadata_fields() {
    let (tmp, db) = setup_cli_home();
    let (endpoint, server_handle) = start_openai_embedding_stub(vector_of(384, 0.25));
    let config_path = tmp.path().join(".mempal").join("config.toml");
    fs::write(
        &config_path,
        format!(
            "[embed]\nbackend = \"api\"\napi_endpoint = \"{endpoint}\"\napi_model = \"test-model\"\n"
        ),
    )
    .expect("write cli config");
    let config = mempal::core::config::Config::load_from(&config_path).expect("load cli config");
    assert_eq!(config.embed.backend, "api");

    let target = Drawer {
        domain: MemoryDomain::Skill,
        field: "debugging".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: "repo://cli-metadata".to_string(),
        ..bootstrap_drawer(
            "drawer_cli_metadata",
            "cli metadata focus cli metadata focus",
            MemoryKind::Knowledge,
            Some(KnowledgeTier::Shu),
            Some(KnowledgeStatus::Promoted),
            Some("CLI statement stays separate."),
        )
    };
    let distractor = Drawer {
        domain: MemoryDomain::Agent,
        field: "tooling".to_string(),
        anchor_kind: AnchorKind::Worktree,
        anchor_id: "worktree:///tmp/cli-distractor".to_string(),
        ..bootstrap_drawer(
            "drawer_cli_distractor",
            "cli metadata focus cli metadata focus",
            MemoryKind::Knowledge,
            Some(KnowledgeTier::Qi),
            Some(KnowledgeStatus::Candidate),
            Some("Distractor statement."),
        )
    };

    insert_search_fixture(&db, &target, &vector_of(384, 0.25));
    insert_search_fixture(&db, &distractor, &vector_of(384, 0.5));

    let output = Command::new(mempal_bin())
        .args([
            "search",
            "cli metadata focus",
            "--wing",
            "mempal",
            "--room",
            "bootstrap",
            "--memory-kind",
            "knowledge",
            "--domain",
            "skill",
            "--field",
            "debugging",
            "--status",
            "promoted",
            "--anchor-kind",
            "repo",
            "--top-k",
            "5",
            "--json",
        ])
        .env("HOME", tmp.path())
        .output()
        .expect("run mempal search");

    assert!(
        output.status.success(),
        "search command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    server_handle.join().expect("join embedding stub");

    let results: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("parse cli search json");
    let results = results.as_array().expect("json result array");
    assert_eq!(results.len(), 1, "expected one filtered search result");
    let result = &results[0];

    assert_eq!(result["drawer_id"], "drawer_cli_metadata");
    assert_eq!(result["content"], target.content);
    assert_eq!(result["memory_kind"], "knowledge");
    assert_eq!(result["domain"], "skill");
    assert_eq!(result["field"], "debugging");
    assert_eq!(result["statement"], "CLI statement stays separate.");
    assert_eq!(result["tier"], "shu");
    assert_eq!(result["status"], "promoted");
    assert_eq!(result["anchor_kind"], "repo");
    assert_eq!(result["anchor_id"], "repo://cli-metadata");
    assert!(result["parent_anchor_id"].is_null());
}
