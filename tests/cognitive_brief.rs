use std::fs;
use std::io::Read;
use std::net::TcpListener;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use mempal::core::anchor;
use mempal::core::db::Database;
use mempal::core::types::{
    AnchorKind, Drawer, KnowledgeCard, KnowledgeEvidenceLink, KnowledgeEvidenceRole,
    KnowledgeStatus, KnowledgeTier, MemoryDomain, MemoryKind, Provenance, RuntimeAdoptionFilter,
    SourceType,
};
use serde_json::Value;
use tempfile::TempDir;

#[path = "common/harness/cli_deadline.rs"]
mod cli_deadline;
#[path = "support/openai_embedding_stub.rs"]
mod openai_embedding_stub;

const _: fn() = cli_deadline::reference_shared_cli_deadline_api;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_cli_home() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_dir = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_dir).expect("create .mempal");
    let db = Database::open(&mempal_dir.join("palace.db")).expect("open cli db");
    (tmp, db)
}

static LOAD_DOTENV: OnceLock<()> = OnceLock::new();

/// Hermetic embed env: dotenv-forwarded `MEMPAL_EMBED_*`, then `extra` overlays.
fn embed_env_pairs(extra: &[(String, String)]) -> Vec<(String, String)> {
    LOAD_DOTENV.get_or_init(|| {
        dotenvy::dotenv().ok();
    });
    let mut pairs = Vec::new();
    for key in [
        "MEMPAL_EMBED_BACKEND",
        "MEMPAL_EMBED_BASE_URL",
        "MEMPAL_EMBED_MODEL",
        "MEMPAL_EMBED_DIM",
    ] {
        if let Ok(val) = std::env::var(key) {
            pairs.push((key.to_string(), val));
        }
    }
    if std::env::var("MEMPAL_EMBED_BACKEND").is_err() {
        pairs.push(("MEMPAL_EMBED_BACKEND".to_string(), "stub".to_string()));
    }
    for (key, value) in extra {
        pairs.push((key.clone(), value.clone()));
    }
    pairs
}

fn openai_compat_env(base_url: &str, dim: &str) -> Vec<(String, String)> {
    vec![
        (
            "MEMPAL_EMBED_BACKEND".to_string(),
            "openai_compat".to_string(),
        ),
        ("MEMPAL_EMBED_BASE_URL".to_string(), base_url.to_string()),
        ("MEMPAL_EMBED_MODEL".to_string(), "test-model".to_string()),
        ("MEMPAL_EMBED_DIM".to_string(), dim.to_string()),
    ]
}

fn run_mempal(home: &TempDir, args: &[&str]) -> Output {
    run_mempal_with_env(home, args, &[])
}

fn run_mempal_with_env(home: &TempDir, args: &[&str], extra: &[(String, String)]) -> Output {
    let env_pairs = embed_env_pairs(extra);
    cli_deadline::run_cli_output(
        "brief cli",
        |spec| {
            cli_deadline::with_home(spec, home.path());
            cli_deadline::push_args(spec, args.iter().copied());
            for (key, value) in &env_pairs {
                spec.env(key, value);
            }
        },
        cli_deadline::CLI_HELPER_DEADLINE,
    )
}

fn run_mempal_with_env_timeout(
    home: &TempDir,
    args: &[&str],
    extra: &[(String, String)],
    ready: mpsc::Receiver<()>,
    timeout: Duration,
) -> Output {
    let mut cmd = Command::new(mempal_bin());
    cmd.args(args)
        .env("HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in embed_env_pairs(extra) {
        cmd.env(key, value);
    }
    let mut child = cmd.spawn().expect("spawn mempal");
    if ready.recv_timeout(timeout).is_err() {
        child.kill().expect("kill unready mempal");
        let _ = child.wait_with_output();
        panic!("embedding request should start before the command deadline");
    }
    let started = Instant::now();
    loop {
        if child.try_wait().expect("poll mempal").is_some() {
            return child.wait_with_output().expect("collect mempal output");
        }
        if started.elapsed() > timeout {
            child.kill().expect("kill timed out mempal");
            let _ = child.wait_with_output();
            panic!("mempal command exceeded {:?}", timeout);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn vector() -> Vec<f32> {
    vec![0.25; 384]
}

fn evidence_drawer(id: &str, content: &str) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: "mempal".to_string(),
        room: Some("brief".to_string()),
        source_file: Some(format!("tests://brief/{id}")),
        source_type: SourceType::UserExplicit,
        added_at: "1710000000".to_string(),
        chunk_index: Some(0),
        normalize_version: 1,
        importance: 3,
        memory_kind: MemoryKind::Evidence,
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: anchor::LEGACY_REPO_ANCHOR_ID.to_string(),
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
        is_pinned: false,
        pin_order: None,
        supersedes: None,
        effective_importance: 3.0,
        compacted_into: None,
        confidence: 1.0,
    }
}

fn knowledge_drawer(id: &str, statement: &str, content: &str, evidence_id: &str) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: "mempal".to_string(),
        room: Some("brief".to_string()),
        source_file: Some(format!("knowledge://project/brief/{id}")),
        source_type: SourceType::UserExplicit,
        added_at: "1710000000".to_string(),
        chunk_index: Some(0),
        normalize_version: 1,
        importance: 4,
        memory_kind: MemoryKind::Knowledge,
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: anchor::LEGACY_REPO_ANCHOR_ID.to_string(),
        parent_anchor_id: None,
        provenance: None,
        statement: Some(statement.to_string()),
        tier: Some(KnowledgeTier::Shu),
        status: Some(KnowledgeStatus::Promoted),
        supporting_refs: vec![evidence_id.to_string()],
        counterexample_refs: Vec::new(),
        teaching_refs: Vec::new(),
        verification_refs: Vec::new(),
        scope_constraints: None,
        is_pinned: false,
        pin_order: None,
        supersedes: None,
        effective_importance: 4.0,
        compacted_into: None,
        confidence: 1.0,
        trigger_hints: None,
    }
}

fn insert_drawer_with_vector(db: &Database, drawer: &Drawer) {
    db.insert_drawer(drawer).expect("insert drawer");
    db.insert_vector(&drawer.id, &vector())
        .expect("insert vector");
}

fn insert_card_with_link(db: &Database, card_id: &str, evidence_id: &str) {
    db.insert_knowledge_card(&KnowledgeCard {
        id: card_id.to_string(),
        statement: "Alice pricing card: pricing risk needs review.".to_string(),
        content: "Alice pricing card content.".to_string(),
        tier: KnowledgeTier::Shu,
        status: KnowledgeStatus::Promoted,
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: anchor::LEGACY_REPO_ANCHOR_ID.to_string(),
        parent_anchor_id: None,
        scope_constraints: None,
        trigger_hints: None,
        created_at: "1710000000".to_string(),
        updated_at: "1710000000".to_string(),
        auto_generated: false,
        crystallization_score: None,
        source_drawer_ids: vec![],
    })
    .expect("insert card");
    db.insert_knowledge_evidence_link(&KnowledgeEvidenceLink {
        id: format!("link_{card_id}_{evidence_id}"),
        card_id: card_id.to_string(),
        evidence_drawer_id: evidence_id.to_string(),
        role: KnowledgeEvidenceRole::Supporting,
        note: None,
        created_at: "1710000000".to_string(),
    })
    .expect("insert card link");
}

struct StalledEmbeddingStub {
    endpoint: String,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl StalledEmbeddingStub {
    fn join(mut self) {
        self.handle
            .take()
            .expect("stalled embedding stub already joined")
            .join()
            .expect("stalled embedding stub");
    }
}

impl Drop for StalledEmbeddingStub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn start_stalled_openai_embedding_stub(
    expected_query: &str,
) -> (StalledEmbeddingStub, mpsc::Receiver<()>) {
    let stop = Arc::new(AtomicBool::new(false));
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind stalled embedding stub");
    listener
        .set_nonblocking(true)
        .expect("set stalled embedding stub nonblocking");
    let address = listener.local_addr().expect("local addr");
    let expected_query = expected_query.to_string();
    let (request_started_tx, request_started_rx) = mpsc::channel();
    let stop_for_thread = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        let (mut stream, _) = loop {
            if stop_for_thread.load(Ordering::Relaxed) {
                return;
            }
            match listener.accept() {
                Ok(connection) => break connection,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(error) => panic!("accept stalled request: {error}"),
            }
        };
        let mut request = [0_u8; 4096];
        let bytes_read = stream.read(&mut request).expect("read embedding request");
        let request = String::from_utf8_lossy(&request[..bytes_read]);
        let (_, body) = request
            .split_once("\r\n\r\n")
            .expect("request should contain JSON body");
        let payload: Value = serde_json::from_str(body).expect("parse embedding request");
        assert_eq!(payload["input"][0], expected_query);
        request_started_tx
            .send(())
            .expect("signal embedding request started");
        match stream.read(&mut [0_u8; 1]) {
            Ok(0) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
                ) => {}
            Ok(_) => panic!("embedding client wrote unexpected data after the request"),
            Err(error) => panic!("wait for embedding client cancellation: {error}"),
        }
    });
    (
        StalledEmbeddingStub {
            endpoint: format!("http://{address}/v1"),
            stop,
            handle: Some(handle),
        },
        request_started_rx,
    )
}

fn write_brief_deadline_config(home: &TempDir, bm25_fallback: bool, deadline_secs: u64) {
    let config = format!(
        "[search]\nbm25_fallback = {bm25_fallback}\n\n[embed.retry]\nsearch_deadline_secs = {deadline_secs}\n"
    );
    fs::write(home.path().join(".mempal/config.toml"), config).expect("write brief config");
}

fn seed_brief_fixture(db: &Database) {
    let evidence = evidence_drawer(
        "brief_evidence_alice",
        "Alice pricing meeting: three unresolved action items remain before the next call.",
    );
    insert_drawer_with_vector(db, &evidence);
    let knowledge = knowledge_drawer(
        "brief_knowledge_alice",
        "Alice pricing has unresolved action items.",
        "Use the Alice pricing evidence before making commitments.",
        "brief_evidence_alice",
    );
    insert_drawer_with_vector(db, &knowledge);
    insert_card_with_link(db, "brief_card_alice", "brief_evidence_alice");
}

#[test]
fn test_cli_brief_delayed_client_is_served_after_former_stub_accept_budget() {
    let (home, db) = setup_cli_home();
    seed_brief_fixture(&db);
    let query = "Alice pricing";
    let stub = openai_embedding_stub::start(query, vector());
    thread::sleep(Duration::from_millis(5500));
    let output = run_mempal_with_env(
        &home,
        &["brief", query, "--format", "json"],
        &openai_compat_env(stub.endpoint(), "384"),
    );
    let outcome = stub.stop_and_join();
    assert!(
        output.status.success(),
        "delayed brief client failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(outcome, openai_embedding_stub::StubOutcome::Served);
    let brief: Value = serde_json::from_slice(&output.stdout).expect("brief json");
    assert_eq!(brief["query"], query);
    assert_eq!(
        brief["key_facts"][0]["citation"]["drawer_id"],
        "brief_knowledge_alice"
    );
    assert_eq!(
        brief["key_facts"][0]["citation"]["source_file"],
        "knowledge://project/brief/brief_knowledge_alice"
    );
}

#[test]
fn test_cli_brief_json_includes_citations_uncertainty_and_actions() {
    let (home, db) = setup_cli_home();
    seed_brief_fixture(&db);
    let query = "Alice pricing";
    let stub = openai_embedding_stub::start(query, vector());
    let output = run_mempal_with_env(
        &home,
        &["brief", query, "--format", "json"],
        &openai_compat_env(stub.endpoint(), "384"),
    );
    let outcome = stub.stop_and_join();
    assert!(
        output.status.success(),
        "brief failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(outcome, openai_embedding_stub::StubOutcome::Served);
    let brief: Value = serde_json::from_slice(&output.stdout).expect("brief json");
    assert_eq!(brief["query"], query);
    assert!(
        brief["summary"]["narrative"]
            .as_str()
            .unwrap()
            .contains("cited")
    );
    assert!(!brief["key_facts"].as_array().unwrap().is_empty());
    assert_eq!(
        brief["key_facts"][0]["citation"]["drawer_id"],
        "brief_knowledge_alice"
    );
    assert_eq!(
        brief["key_facts"][0]["citation"]["source_file"],
        "knowledge://project/brief/brief_knowledge_alice"
    );
    assert!(!brief["evidence"].as_array().unwrap().is_empty());
    assert_eq!(brief["cards"][0]["card_id"], "brief_card_alice");
    assert_eq!(
        brief["cards"][0]["evidence_citations"][0]["evidence_drawer_id"],
        "brief_evidence_alice"
    );
    assert!(brief["uncertainty"].is_array());
    assert!(!brief["next_actions"].as_array().unwrap().is_empty());

    let events = db
        .list_runtime_adoption_events(&RuntimeAdoptionFilter::default(), 10)
        .expect("list adoption events");
    assert!(events.is_empty());
}

#[test]
fn test_cli_brief_plain_lists_sections_and_citations() {
    let (home, db) = setup_cli_home();
    seed_brief_fixture(&db);
    let query = "Alice pricing";
    let stub = openai_embedding_stub::start(query, vector());
    let output = run_mempal_with_env(
        &home,
        &["brief", query],
        &openai_compat_env(stub.endpoint(), "384"),
    );
    let outcome = stub.stop_and_join();
    assert!(
        output.status.success(),
        "brief failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(outcome, openai_embedding_stub::StubOutcome::Served);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("## Summary"));
    assert!(stdout.contains("## Key Facts"));
    assert!(stdout.contains("## Evidence"));
    assert!(stdout.contains("## Uncertainty"));
    assert!(stdout.contains("## Next Actions"));
    assert!(stdout.contains("drawer: brief_knowledge_alice"));
    assert!(stdout.contains("source: knowledge://project/brief/brief_knowledge_alice"));
}

#[test]
fn test_cli_brief_no_evidence_reports_uncertainty() {
    let (home, _db) = setup_cli_home();
    let query = "Unknown account";
    let stub = openai_embedding_stub::start(query, vector());
    let output = run_mempal_with_env(
        &home,
        &["brief", query, "--format", "json"],
        &openai_compat_env(stub.endpoint(), "384"),
    );
    let outcome = stub.stop_and_join();
    assert!(
        output.status.success(),
        "brief failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(outcome, openai_embedding_stub::StubOutcome::Served);
    let brief: Value = serde_json::from_slice(&output.stdout).expect("brief json");
    assert_eq!(brief["evidence"].as_array().unwrap().len(), 0);
    assert!(
        brief["uncertainty"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["kind"] == "no_evidence")
    );
    assert!(
        brief["next_actions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item.as_str().unwrap().contains("Ingest"))
    );
}

#[test]
fn test_cli_brief_embedding_deadline_falls_back_to_bm25_json() {
    let (home, db) = setup_cli_home();
    write_brief_deadline_config(&home, true, 1);
    let drawer = evidence_drawer(
        "brief_evidence_deadline",
        "Synthetic deadline query evidence exists for BM25 fallback citation.",
    );
    db.insert_drawer(&drawer).expect("insert drawer");
    let query = "Synthetic deadline query";
    let (stub, request_started) = start_stalled_openai_embedding_stub(query);
    let output = run_mempal_with_env_timeout(
        &home,
        &["brief", query, "--format", "json"],
        &openai_compat_env(&stub.endpoint, "384"),
        request_started,
        Duration::from_secs(5),
    );
    assert!(
        output.status.success(),
        "brief fallback should exit 0; stderr bytes={}",
        output.stderr.len()
    );
    assert!(!output.stdout.is_empty());
    let brief: Value = serde_json::from_slice(&output.stdout).expect("brief json");
    assert_eq!(brief["search_mode"], "bm25_only");
    assert!(
        brief["warnings"]
            .as_array()
            .expect("warnings array")
            .iter()
            .any(|warning| warning.as_str().is_some_and(|text| {
                text.contains("embedding deadline exceeded after 1s")
                    && text.contains("BM25-only search")
            }))
    );
    assert_eq!(
        brief["evidence"][0]["citation"]["drawer_id"],
        "brief_evidence_deadline"
    );
    stub.join();
}

#[test]
fn test_cli_brief_embedding_dimension_mismatch_falls_back_to_bm25_json() {
    let (home, db) = setup_cli_home();
    write_brief_deadline_config(&home, true, 5);
    let drawer = evidence_drawer(
        "brief_evidence_dimension_mismatch",
        "Synthetic dimension query evidence exists for BM25 fallback citation.",
    );
    db.insert_drawer(&drawer).expect("insert drawer");
    db.insert_vector(&drawer.id, &[0.8, 0.2])
        .expect("insert 2d vector");
    let query = "Synthetic dimension query";
    let stub = openai_embedding_stub::start(query, vec![0.1, 0.2, 0.3]);
    let output = run_mempal_with_env(
        &home,
        &["brief", query, "--format", "json"],
        &openai_compat_env(stub.endpoint(), "3"),
    );
    let outcome = stub.stop_and_join();
    assert!(
        output.status.success(),
        "brief fallback should exit 0: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(outcome, openai_embedding_stub::StubOutcome::Served);
    assert!(!output.stdout.is_empty());
    let brief: Value = serde_json::from_slice(&output.stdout).expect("brief json");
    assert_eq!(brief["search_mode"], "bm25_only");
    assert!(
        brief["warnings"]
            .as_array()
            .expect("warnings array")
            .iter()
            .any(|warning| warning
                .as_str()
                .is_some_and(|text| text.contains("dimension mismatch")))
    );
    assert!(
        brief["summary"]["narrative"]
            .as_str()
            .is_some_and(|summary| !summary.is_empty())
    );
    assert_eq!(
        brief["evidence"][0]["citation"]["drawer_id"],
        "brief_evidence_dimension_mismatch"
    );
    assert!(
        brief["evidence"][0]["citation"]["source_file"]
            .as_str()
            .is_some_and(|source| !source.is_empty())
    );
}

#[test]
fn test_cli_brief_plain_no_results_is_not_blank() {
    let (home, _db) = setup_cli_home();
    let output = run_mempal(&home, &["brief", "No matching memory"]);
    assert!(
        output.status.success(),
        "brief no-results failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!output.stdout.is_empty());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("## Summary"));
    assert!(stdout.contains("No cited memory was found"));
    assert!(stdout.contains("## Uncertainty"));
    assert!(stdout.contains("## Next Actions"));
}

#[test]

fn test_cli_brief_rejects_invalid_format() {
    let (home, _db) = setup_cli_home();
    let output = run_mempal(&home, &["brief", "Alice pricing", "--format", "yaml"]);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unsupported brief format"));
}
