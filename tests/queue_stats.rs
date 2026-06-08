use std::fs;
#[cfg(feature = "integration")]
use std::io::{Read, Write};
#[cfg(feature = "integration")]
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(feature = "integration")]
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use mempal::core::db::{Database, apply_fork_ext_migrations_to};
use mempal::core::queue::{PendingMessageStore, QueueConfig, QueueFailureDisposition};
use mempal::core::types::{BootstrapEvidenceArgs, Drawer, SourceType};
use rusqlite::{Connection, params};
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time")
        .as_secs() as i64
}

fn new_store(config: QueueConfig) -> (TempDir, PathBuf, PendingMessageStore) {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    Database::open(&db_path).expect("open db");
    let store = PendingMessageStore::with_config(&db_path, config).expect("create store");
    (tmp, db_path, store)
}

fn setup_home() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"
db_path = "{}"
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    (tmp, db_path)
}

fn insert_status_drawer(db_path: &Path, id: &str, source_type: SourceType) {
    let db = Database::open(db_path).expect("open db for status drawer");
    let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: format!("status fixture {id}"),
        wing: "mempal".to_string(),
        room: Some("status".to_string()),
        source_file: Some(format!("{id}.md")),
        source_type,
        added_at: "1700000000".to_string(),
        chunk_index: Some(0),
        importance: 1,
    });
    db.insert_drawer(&drawer).expect("insert status drawer");
}

fn recreate_vectors_with_metric(db_path: &Path, metric: &str) {
    let db = Database::open(db_path).expect("open db for vector table");
    db.conn()
        .execute_batch(&format!(
            r#"
            DROP TABLE IF EXISTS drawer_vectors;
            CREATE VIRTUAL TABLE drawer_vectors USING vec0(
                id TEXT PRIMARY KEY,
                embedding FLOAT[3] distance_metric={metric},
                +project_id TEXT
            );
            "#
        ))
        .expect("recreate vector table");
}

#[cfg(feature = "integration")]
fn setup_home_with_status_endpoints(base_url: &str) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open db");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"
db_path = "{}"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}"
model = "embed-test"

[llm]
enabled = true
base_url = "{}"
model = "llm-test"
"#,
            db_path.display(),
            base_url,
            base_url
        ),
    )
    .expect("write config");
    (tmp, db_path)
}

#[cfg(feature = "integration")]
#[test]
fn test_status_command_shows_endpoint_health() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let handle = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut buf = [0_u8; 1024];
            let _ = stream.read(&mut buf);
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 29\r\nconnection: close\r\n\r\n{\"object\":\"list\",\"data\":[]}",
                )
                .expect("write response");
        }
    });

    let (home, _db_path) = setup_home_with_status_endpoints(&format!("http://{addr}/v1"));
    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", home.path())
        .output()
        .expect("run mempal status");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(stdout.contains("Endpoints:"), "{stdout}");
    assert!(stdout.contains("embedding: reachable ("), "{stdout}");
    assert!(stdout.contains("llm: reachable ("), "{stdout}");

    handle.join().expect("server join");
}

#[test]
fn test_fork_ext_migration_v0_to_v1_creates_pending_messages_table() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let conn = Connection::open(&db_path).expect("open sqlite");

    let upstream_user_version_before = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .expect("read initial user_version");
    assert_eq!(upstream_user_version_before, 0);

    apply_fork_ext_migrations_to(&conn, 1).expect("apply ext v1 migration");

    let fork_ext_version = conn
        .query_row(
            "SELECT value FROM fork_ext_meta WHERE key = 'fork_ext_version'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("read fork_ext_version");
    assert_eq!(fork_ext_version, "1");

    let upstream_user_version_after = conn
        .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
        .expect("read final user_version");
    assert_eq!(upstream_user_version_after, 0);

    let table_exists = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='pending_messages'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query pending_messages table");
    assert_eq!(table_exists, 1);

    let index_exists = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_pending_next_attempt'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("query pending_messages index");
    assert_eq!(index_exists, 1);
}

#[test]
fn test_queue_stats_reflects_current_state() {
    let (_tmp, db_path, store) = new_store(QueueConfig {
        base_delay_ms: 0,
        max_delay_ms: 0,
        max_retries: 0,
    });

    let pending_id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    let claimed_id = store.enqueue("hook_event", r#"{"n":2}"#).expect("enqueue");
    let done_id = store.enqueue("hook_event", r#"{"n":3}"#).expect("enqueue");
    let failed_id = store.enqueue("hook_event", r#"{"n":4}"#).expect("enqueue");

    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET created_at = ?2, next_attempt_at = ?3 WHERE id = ?1",
        params![pending_id, now_secs() - 120, now_secs() + 3_600],
    )
    .expect("age pending row");

    let claimed = store
        .claim_next("worker-claimed", 60)
        .expect("claim")
        .expect("claimed row");
    assert_eq!(claimed.id, claimed_id);

    let done = store
        .claim_next("worker-done", 60)
        .expect("claim")
        .expect("done row");
    assert_eq!(done.id, done_id);
    store.confirm(&done).expect("confirm");

    let failed = store
        .claim_next("worker-failed", 60)
        .expect("claim")
        .expect("failed row");
    assert_eq!(failed.id, failed_id);
    store
        .mark_failed_with_disposition(&failed, "boom", QueueFailureDisposition::Terminal)
        .expect("mark terminal failed");

    let stats = store.stats().expect("stats");
    assert_eq!(stats.pending, 1);
    assert_eq!(stats.claimed, 1);
    assert_eq!(stats.failed, 1);
    assert!((stats.rate_per_min - 0.1).abs() < f64::EPSILON, "{stats:?}");
    assert!(stats.avg_processing_ms.is_some(), "{stats:?}");
    assert_eq!(stats.eta_secs, Some(600), "{stats:?}");
    assert!(
        stats.oldest_pending_age_secs.is_some_and(|age| age >= 100),
        "{stats:?}"
    );

    let remaining_claimed = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT id FROM pending_messages WHERE status = 'claimed'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("claimed row id");
    assert_eq!(remaining_claimed, claimed_id);
}

#[test]
fn test_oldest_pending_age_none_when_empty() {
    let (_tmp, _db_path, store) = new_store(QueueConfig::default());

    let stats = store.stats().expect("stats");
    assert_eq!(stats.pending, 0);
    assert_eq!(stats.claimed, 0);
    assert_eq!(stats.failed, 0);
    assert_eq!(stats.oldest_pending_age_secs, None);
    assert_eq!(stats.rate_per_min, 0.0);
    assert_eq!(stats.avg_processing_ms, None);
    assert_eq!(stats.eta_secs, None);
}

#[test]
fn test_status_command_shows_queue_stats() {
    let (home, db_path) = setup_home();
    insert_status_drawer(&db_path, "drawer-status-user", SourceType::UserExplicit);
    let store = PendingMessageStore::with_config(
        &db_path,
        QueueConfig {
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_retries: 0,
        },
    )
    .expect("create store");

    let pending_id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    let claimed_id = store.enqueue("hook_event", r#"{"n":2}"#).expect("enqueue");
    let done_id = store.enqueue("hook_event", r#"{"n":3}"#).expect("enqueue");
    let failed_id = store.enqueue("hook_event", r#"{"n":4}"#).expect("enqueue");

    Connection::open(&db_path)
        .expect("open sqlite")
        .execute(
            "UPDATE pending_messages SET created_at = ?2, next_attempt_at = ?3 WHERE id = ?1",
            params![pending_id, now_secs() - 90, now_secs() + 3_600],
        )
        .expect("age pending row");

    let claimed = store
        .claim_next("worker-claimed", 60)
        .expect("claim")
        .expect("claimed");
    assert_eq!(claimed.id, claimed_id);
    let done = store
        .claim_next("worker-done", 60)
        .expect("claim")
        .expect("done");
    assert_eq!(done.id, done_id);
    store.confirm(&done).expect("confirm");
    let failed = store
        .claim_next("worker-failed", 60)
        .expect("claim")
        .expect("failed");
    assert_eq!(failed.id, failed_id);
    store
        .mark_failed_with_disposition(&failed, "boom", QueueFailureDisposition::Terminal)
        .expect("mark terminal failed");

    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", home.path())
        .output()
        .expect("run mempal status");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(stdout.contains("Queue:"), "{stdout}");
    assert!(stdout.contains("embed_fail_count: 1"), "{stdout}");
    assert!(stdout.contains("pending: 1"), "{stdout}");
    assert!(stdout.contains("claimed: 1"), "{stdout}");
    assert!(stdout.contains("failed: 1"), "{stdout}");
    assert!(stdout.contains("rate_per_min: 0.1"), "{stdout}");
    assert!(stdout.contains("avg_processing_ms:"), "{stdout}");
    assert!(stdout.contains("eta_secs: 600"), "{stdout}");
    assert!(stdout.contains("oldest_pending_age_secs:"), "{stdout}");
    assert!(stdout.contains("Source Types:"), "{stdout}");
    assert!(stdout.contains("user_explicit: 1"), "{stdout}");
}

#[test]
fn test_status_command_shows_vector_index_stale_flag() {
    let (home, db_path) = setup_home();
    recreate_vectors_with_metric(&db_path, "l2");

    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", home.path())
        .output()
        .expect("run mempal status");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(stdout.contains("vector_index_stale: true"), "{stdout}");
}

/// #302 regression: an empty-but-correct-metric `drawer_vectors` table (0 rows,
/// cosine, with drawers present) is silently healthy to the #295 metric-only
/// staleness check. `mempal status` MUST surface the empty state via
/// `vector_index_empty: true` / `vector_rows: 0` while still reporting
/// `vector_index_stale: false` (the cosine metric is correct). Fails red
/// against pre-#302 code, which has no empty/row-count signal.
#[test]
fn test_status_command_flags_empty_vector_table() {
    let (home, db_path) = setup_home();
    insert_status_drawer(&db_path, "drawer-empty-1", SourceType::AgentInference);
    // Correct (cosine) metric but zero rows: the post-recreate empty-table state.
    recreate_vectors_with_metric(&db_path, "cosine");

    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", home.path())
        .output()
        .expect("run mempal status");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    // #295 metric check is unchanged: cosine table is not "stale".
    assert!(stdout.contains("vector_index_stale: false"), "{stdout}");
    // #302 adds the row-count dimension so the empty index is non-silent.
    assert!(stdout.contains("vector_index_empty: true"), "{stdout}");
    assert!(stdout.contains("vector_rows: 0"), "{stdout}");
}

#[test]
fn test_reindex_failed_requeues_only_failed_embed_queue_items() {
    let (home, db_path) = setup_home();
    let store = PendingMessageStore::new(&db_path).expect("create store");
    let failed_embed = store
        .enqueue("hook_event", r#"{"n":1}"#)
        .expect("enqueue failed embed");
    let failed_llm = store
        .enqueue("llm_task", r#"{"n":2}"#)
        .expect("enqueue failed llm");
    let pending_embed = store
        .enqueue("hook_event", r#"{"n":3}"#)
        .expect("enqueue pending embed");

    Connection::open(&db_path)
        .expect("open sqlite")
        .execute(
            "UPDATE pending_messages SET status = 'failed', retry_count = 4, last_error = 'boom' WHERE id IN (?1, ?2)",
            params![failed_embed, failed_llm],
        )
        .expect("mark failed rows");

    let output = Command::new(mempal_bin())
        .args(["reindex", "--failed"])
        .env("HOME", home.path())
        .output()
        .expect("run mempal reindex --failed");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("reindex stdout utf8");
    assert!(
        stdout.contains("requeued failed embed queue items: 1"),
        "{stdout}"
    );

    let conn = Connection::open(db_path).expect("open sqlite");
    let status_for = |id: &str| -> (String, i64) {
        conn.query_row(
            "SELECT status, retry_count FROM pending_messages WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read queue row")
    };
    assert_eq!(status_for(&failed_embed), ("pending".to_string(), 0));
    assert_eq!(status_for(&failed_llm), ("failed".to_string(), 4));
    assert_eq!(status_for(&pending_embed), ("pending".to_string(), 0));
}

#[test]
fn test_queue_module_no_unwrap() {
    let queue_source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/core/queue.rs");
    let content = fs::read_to_string(&queue_source).expect("read queue source");
    let offenders = content
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.contains("// SAFETY:") && line.contains(".unwrap()"))
        .map(|(index, line)| format!("{}:{}", index + 1, line.trim()))
        .collect::<Vec<_>>();

    assert!(
        offenders.is_empty(),
        "queue module contains .unwrap():\n{}",
        offenders.join("\n")
    );
}
