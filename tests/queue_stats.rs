use std::fs;
#[cfg(feature = "integration")]
use std::io::{Read, Write};
#[cfg(feature = "integration")]
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::Command;
#[cfg(feature = "integration")]
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use mempal::core::db::{Database, apply_fork_ext_migrations_to};
use mempal::core::db_admission::ProfileDbAdmission;
use mempal::core::queue::{
    PendingMessageStore, QueueConfig, QueueFailureDisposition, QueueFailureFilter,
};
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

fn setup_home_with_database() -> (TempDir, PathBuf, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let db = Database::open(&db_path).expect("open db");
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
    (tmp, db_path, db)
}

fn setup_home() -> (TempDir, PathBuf) {
    let (tmp, db_path, _db) = setup_home_with_database();
    (tmp, db_path)
}

fn run_status_in_home(home: &TempDir) -> String {
    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", home.path())
        .output()
        .expect("run mempal status");

    assert!(
        output.status.success(),
        "status failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("status stdout utf8")
}

fn run_status_with_daemon_pidfile(content: &str) -> String {
    let (home, db_path) = setup_home();
    let pid_path = db_path.parent().expect("mempal home").join("daemon.pid");
    fs::write(pid_path, content).expect("write daemon pidfile");

    run_status_in_home(&home)
}

#[test]
fn test_status_command_reports_missing_daemon_pidfile_as_stopped() {
    let (home, _db_path) = setup_home();
    let stdout = run_status_in_home(&home);

    assert!(stdout.contains("Daemon:"), "{stdout}");
    assert!(stdout.contains("running: false"), "{stdout}");
    assert!(stdout.contains("pid: none"), "{stdout}");
    assert!(!stdout.contains("daemon pidfile"), "{stdout}");
}

#[test]
fn test_status_command_warns_on_empty_daemon_pidfile() {
    let stdout = run_status_with_daemon_pidfile("");

    assert!(stdout.contains("Daemon:"), "{stdout}");
    assert!(stdout.contains("running: false"), "{stdout}");
    assert!(stdout.contains("pid: none"), "{stdout}");
    assert!(stdout.contains("Warnings:"), "{stdout}");
    assert!(stdout.contains("daemon pidfile"), "{stdout}");
    assert!(stdout.contains("empty"), "{stdout}");
}

#[test]
fn test_status_command_warns_on_corrupt_daemon_pidfile() {
    let stdout = run_status_with_daemon_pidfile("not-a-pid\n");

    assert!(stdout.contains("Daemon:"), "{stdout}");
    assert!(stdout.contains("running: false"), "{stdout}");
    assert!(stdout.contains("pid: none"), "{stdout}");
    assert!(stdout.contains("Warnings:"), "{stdout}");
    assert!(stdout.contains("daemon pidfile"), "{stdout}");
    assert!(stdout.contains("not a valid integer"), "{stdout}");
    assert!(
        !stdout.contains("not-a-pid"),
        "status must not echo corrupt pidfile payload: {stdout}"
    );
}

fn insert_status_drawer(db: &Database, id: &str, source_type: SourceType) {
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

fn recreate_vectors_with_metric(db: &Database, metric: &str) {
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
        for _ in 0..3 {
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
        .arg("--full")
        .env("HOME", home.path())
        .output()
        .expect("run mempal status");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(stdout.contains("Endpoints:"), "{stdout}");
    assert!(stdout.contains("embedding: reachable ("), "{stdout}");
    assert!(
        stdout.contains("llm_control_plane: reachable ("),
        "{stdout}"
    );
    assert!(stdout.contains("llm_generation: reachable ("), "{stdout}");

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
fn test_queue_stats_readonly_handles_pre_v22_failure_class_schema() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let conn = Connection::open(&db_path).expect("open db");
    conn.execute_batch(
        r#"
        CREATE TABLE fork_ext_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );
        INSERT INTO fork_ext_meta (key, value) VALUES ('fork_ext_version', '21');

        CREATE TABLE pending_messages (
            id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            source_hash TEXT NOT NULL,
            status TEXT NOT NULL,
            payload TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            last_error TEXT,
            op_state TEXT NOT NULL
        );
        "#,
    )
    .expect("create v21 queue schema");
    conn.execute(
        r#"
        INSERT INTO pending_messages (
            id,
            kind,
            source_hash,
            status,
            payload,
            created_at,
            last_error,
            op_state
        )
        VALUES ('legacy-v21-failed', 'hook_event', 'legacy-v21-failed', 'failed', '{}', 1700000000, 'legacy failure', 'failed')
        "#,
        [],
    )
    .expect("insert v21 failed row");
    drop(conn);

    let stats = mempal::core::queue::queue_stats_readonly(&db_path).expect("readonly stats");
    assert_eq!(stats.failed, 1);
    assert_eq!(stats.failed_retryable, 0);
    assert_eq!(stats.failed_terminal, 1);
    assert_eq!(stats.failed_retryable_embed, 0);
    assert_eq!(stats.failed_retryable_llm, 0);
    assert_eq!(stats.last_auto_requeue_at_unix_ms, None);
}

#[test]
fn test_fork_ext_v22_classifies_legacy_failed_rows_conservatively() {
    let tmp = TempDir::new().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    let conn = db.conn();
    conn.execute(
        "UPDATE fork_ext_meta SET value = '21' WHERE key = 'fork_ext_version'",
        [],
    )
    .expect("rewind fork ext version for v22 fixture");

    for (id, kind, last_error) in [
        ("transient-timeout", "llm_task", "429 Too Many Requests"),
        (
            "transient-hook-post-tool",
            "hook_post_tool",
            "timeout contacting embedding endpoint",
        ),
        (
            "transient-hook-user-prompt",
            "hook_user_prompt",
            "connection reset by peer",
        ),
        (
            "transient-hook-session-start",
            "hook_session_start",
            "503 Service Unavailable",
        ),
        (
            "transient-hook-session-end",
            "hook_session_end",
            "rate limit exceeded",
        ),
        ("terminal-invalid", "llm_task", "invalid json payload"),
        ("terminal-unknown", "hook_event", "worker rejected row"),
        (
            "terminal-unknown-kind-timeout",
            "future_model_task",
            "timeout acquiring database lock",
        ),
    ] {
        conn.execute(
            r#"
            INSERT INTO pending_messages (
                id,
                kind,
                source_hash,
                status,
                payload,
                created_at,
                last_error,
                op_state
            )
            VALUES (?1, ?2, ?3, 'failed', '{}', 1700000000, ?4, 'failed')
            "#,
            params![id, kind, id, last_error],
        )
        .expect("insert legacy failed row");
    }

    apply_fork_ext_migrations_to(conn, 22).expect("apply ext v22 migration");
    let failure_class_for = |id: &str| -> String {
        conn.query_row(
            "SELECT failure_class FROM pending_messages WHERE id = ?1",
            [id],
            |row| row.get::<_, String>(0),
        )
        .expect("read failure_class")
    };

    assert_eq!(failure_class_for("transient-timeout"), "retryable_model");
    assert_eq!(
        failure_class_for("transient-hook-post-tool"),
        "retryable_model"
    );
    assert_eq!(
        failure_class_for("transient-hook-user-prompt"),
        "retryable_model"
    );
    assert_eq!(
        failure_class_for("transient-hook-session-start"),
        "retryable_model"
    );
    assert_eq!(
        failure_class_for("transient-hook-session-end"),
        "retryable_model"
    );
    assert_eq!(failure_class_for("terminal-invalid"), "terminal");
    assert_eq!(failure_class_for("terminal-unknown"), "terminal");
    assert_eq!(
        failure_class_for("terminal-unknown-kind-timeout"),
        "terminal"
    );
}

#[test]
fn test_queue_stats_reflects_current_state() {
    let (_tmp, db_path, store) = new_store(QueueConfig {
        base_delay_ms: 0,
        max_delay_ms: 0,
        max_retries: 0,
        ..QueueConfig::default()
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
    let (home, db_path, db) = setup_home_with_database();
    insert_status_drawer(&db, "drawer-status-user", SourceType::UserExplicit);
    let store = PendingMessageStore::with_config(
        &db_path,
        QueueConfig {
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_retries: 0,
            ..QueueConfig::default()
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
    assert!(stdout.contains("active_payload_bytes:"), "{stdout}");
    assert!(
        stdout.contains("active_ingest_payload_bytes: 0"),
        "{stdout}"
    );
    assert!(stdout.contains("ingest_payload_limit_bytes:"), "{stdout}");
    assert!(stdout.contains("rejected_oversize: 0"), "{stdout}");
    assert!(stdout.contains("failed: 1"), "{stdout}");
    assert!(stdout.contains("rate_per_min: 0.1"), "{stdout}");
    assert!(stdout.contains("avg_processing_ms:"), "{stdout}");
    assert!(stdout.contains("eta_secs: 600"), "{stdout}");
    assert!(stdout.contains("oldest_pending_age_secs:"), "{stdout}");
    assert!(stdout.contains("Source Types:"), "{stdout}");
    assert!(stdout.contains("user_explicit: 1"), "{stdout}");
}

#[test]
fn test_status_command_live_queue_counts_ignore_stale_completion_op_state() {
    let (home, db_path) = setup_home();
    let store = PendingMessageStore::new(&db_path).expect("create store");
    let failed_id = store.enqueue("hook_event", r#"{"n":1}"#).expect("enqueue");
    let failed = store
        .claim_next("worker-failed", 60)
        .expect("claim")
        .expect("failed row");
    assert_eq!(failed.id, failed_id);
    store
        .mark_failed_with_disposition(&failed, "boom", QueueFailureDisposition::Terminal)
        .expect("mark terminal failed");

    Connection::open(&db_path)
        .expect("open sqlite")
        .execute_batch(
            r#"
            INSERT INTO pending_message_completions (
                message_id,
                kind,
                created_at,
                claimed_at,
                completed_at,
                processing_ms,
                op_state
            )
            VALUES
                ('history-running', 'hook_event', 1700000000, 1700000001, 1700000002, 1000, 'running'),
                ('history-queued', 'hook_event', 1700000003, 1700000004, 1700000005, 1000, 'queued');
            "#,
        )
        .expect("insert stale completion history");

    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", home.path())
        .output()
        .expect("run mempal status");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(stdout.contains("Queue:"), "{stdout}");
    assert!(stdout.contains("pending: 0"), "{stdout}");
    assert!(stdout.contains("claimed: 0"), "{stdout}");
    assert!(stdout.contains("failed: 1"), "{stdout}");
    assert!(stdout.contains("embed_fail_count: 1"), "{stdout}");
}

#[test]
fn test_status_command_shows_vector_index_stale_flag() {
    let (home, db_path, db) = setup_home_with_database();
    let snapshot = ProfileDbAdmission::snapshot(&db_path).expect("snapshot fixture admission");
    assert_eq!(snapshot.active_holders, 1);
    recreate_vectors_with_metric(&db, "l2");

    let output = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", home.path())
        .output()
        .expect("run mempal status");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("status stdout utf8");
    assert!(stdout.contains("vector_index_stale: true"), "{stdout}");
}

#[test]
fn test_status_command_flags_empty_vector_table() {
    let (home, _db_path, db) = setup_home_with_database();
    insert_status_drawer(&db, "drawer-empty-1", SourceType::AgentInference);
    // Correct (cosine) metric but zero rows: the post-recreate empty-table state.
    recreate_vectors_with_metric(&db, "cosine");

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
            "UPDATE pending_messages SET status = 'failed', failure_class = 'retryable_model', retry_count = 4, last_error = 'boom' WHERE id IN (?1, ?2)",
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
fn test_queue_stats_split_failed_retryable_and_terminal_model_work() {
    let (_tmp, _db_path, store) = new_store(QueueConfig::default());
    let retryable_embed = store
        .enqueue("hook_event", r#"{"n":1}"#)
        .expect("enqueue retryable embed");
    let retryable_llm = store
        .enqueue("llm_task", r#"{"n":2}"#)
        .expect("enqueue retryable llm");
    let terminal_embed = store
        .enqueue("hook_event", r#"{"n":3}"#)
        .expect("enqueue terminal embed");

    store
        .mark_model_task_failed_retryable(&retryable_embed, "temporary embed outage")
        .expect("mark retryable embed failed");
    store
        .mark_model_task_failed_retryable(&retryable_llm, "temporary llm outage")
        .expect("mark retryable llm failed");
    let terminal_claim = store
        .claim_next("worker-terminal", 60)
        .expect("claim")
        .expect("terminal claim");
    assert_eq!(terminal_claim.id, terminal_embed);
    store
        .mark_failed_with_disposition(
            &terminal_claim,
            "invalid configuration",
            QueueFailureDisposition::Terminal,
        )
        .expect("mark terminal failed");

    let stats = store.stats().expect("stats");
    assert_eq!(stats.failed, 3);
    assert_eq!(stats.failed_retryable, 2);
    assert_eq!(stats.failed_terminal, 1);
    assert_eq!(stats.failed_retryable_embed, 1);
    assert_eq!(stats.failed_retryable_llm, 1);
}

#[test]
fn test_queue_stats_classifies_failed_reasons_and_retrying_backoff() {
    let (_tmp, db_path, store) = new_store(QueueConfig::default());
    let malformed = store
        .enqueue("ingest_async", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue malformed");
    let gate = store
        .enqueue("hook_post_tool", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue gate");
    let llm_decode = store
        .enqueue("hook_post_tool", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue llm decode");
    let storage_terminal = store
        .enqueue("hook_post_tool", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue storage terminal");
    let storage_retrying = store
        .enqueue("hook_post_tool", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue storage retrying");
    let writer_lease_retrying = store
        .enqueue("hook_post_tool", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue writer lease retrying");
    let now = now_secs();

    let conn = Connection::open(&db_path).expect("open sqlite");
    for (id, kind, status, failure_class, retry_count, next_attempt_at, last_error) in [
        (
            malformed.as_str(),
            "ingest_async",
            "failed",
            Some("terminal"),
            18_i64,
            now,
            "failed to decode queued hook envelope",
        ),
        (
            gate.as_str(),
            "hook_post_tool",
            "failed",
            Some("terminal"),
            3_i64,
            now,
            "automatic hook LLM gate failed before durable insert",
        ),
        (
            llm_decode.as_str(),
            "hook_post_tool",
            "failed",
            Some("terminal"),
            4_i64,
            now,
            "automatic hook LLM gate failed before durable insert: LLM gating request failed: failed to decode LLM response: error decoding response body",
        ),
        (
            storage_terminal.as_str(),
            "hook_post_tool",
            "failed",
            Some("terminal"),
            46_i64,
            now,
            "failed to reopen db for merge drawer_hooks_raw_bash_69633781",
        ),
        (
            storage_retrying.as_str(),
            "hook_post_tool",
            "pending",
            None,
            2_i64,
            now + 60,
            "failed to reopen db for merge drawer_hooks_raw_bash_69633781",
        ),
        (
            writer_lease_retrying.as_str(),
            "hook_post_tool",
            "pending",
            None,
            3_i64,
            now + 120,
            "SQLite writer lease `sqlite-writer` for mempal-daemon-1000 was lost before build daemon hook drawer records",
        ),
    ] {
        conn.execute(
            r#"
            UPDATE pending_messages
            SET kind = ?2,
                status = ?3,
                failure_class = ?4,
                retry_count = ?5,
                next_attempt_at = ?6,
                last_error = ?7,
                op_state = CASE WHEN ?3 = 'failed' THEN 'failed' ELSE 'queued' END
            WHERE id = ?1
            "#,
            params![
                id,
                kind,
                status,
                failure_class,
                retry_count,
                next_attempt_at,
                last_error
            ],
        )
        .expect("update queue fixture");
    }
    drop(conn);

    let stats = store.stats().expect("stats");
    assert_eq!(stats.failed, 4);
    assert_eq!(stats.failed_retryable, 0);
    assert_eq!(stats.failed_terminal, 4);
    assert_eq!(stats.retrying, 2);
    assert_eq!(stats.next_retry_at_unix_secs, Some((now + 60) as u64));

    let failed_reason_count = |reason: &str| -> u64 {
        stats
            .failed_buckets
            .iter()
            .find(|bucket| bucket.reason_code == reason)
            .map_or(0, |bucket| bucket.count)
    };
    assert_eq!(failed_reason_count("invalid_hook_envelope"), 1);
    assert_eq!(failed_reason_count("automatic_hook_llm_gate"), 1);
    assert_eq!(failed_reason_count("llm_response_decode"), 1);
    assert_eq!(failed_reason_count("storage_merge_reopen"), 1);
    let retrying_storage = stats
        .retrying_buckets
        .iter()
        .find(|bucket| bucket.reason_code == "storage_merge_reopen")
        .expect("retrying storage bucket");
    assert_eq!(retrying_storage.retry_class, "retrying_backoff");
    assert_eq!(retrying_storage.count, 1);
    let retrying_writer_lease = stats
        .retrying_buckets
        .iter()
        .find(|bucket| bucket.reason_code == "writer_lease_lost")
        .expect("retrying writer lease bucket");
    assert_eq!(retrying_writer_lease.retry_class, "retrying_backoff");
    assert_eq!(retrying_writer_lease.count, 1);
    for bucket in stats
        .failed_buckets
        .iter()
        .chain(stats.retrying_buckets.iter())
    {
        assert!(
            !bucket
                .sanitized_message
                .contains("RAW_PAYLOAD_SHOULD_NOT_APPEAR"),
            "{bucket:?}"
        );
    }
}

#[test]
fn test_daemon_start_requeues_previous_identity_writer_lease_failures() {
    let (_tmp, _db_path, store) = new_store(QueueConfig {
        base_delay_ms: 60_000,
        max_delay_ms: 60_000,
        ..QueueConfig::default()
    });
    let id = store
        .enqueue("hook_post_tool", r#"{"event":"PostToolUse"}"#)
        .expect("enqueue hook");
    let previous_owner = "mempal-daemon-1000-previous-start";
    let current_owner = "mempal-daemon-1000-current-start";
    let claim = store
        .claim_next(previous_owner, 60)
        .expect("claim hook")
        .expect("hook available");
    store
        .mark_failed(
            &claim,
            "SQLite writer lease `sqlite-writer` for mempal-daemon-1000-previous-start was lost before build daemon hook drawer records",
        )
        .expect("record previous identity lease loss");
    store
        .enqueue("hook_post_tool", r#"{"event":"PostToolUseCurrent"}"#)
        .expect("enqueue current daemon hook");
    let current_claim = store
        .claim_next(current_owner, 60)
        .expect("claim current daemon hook")
        .expect("current daemon hook available");
    store
        .mark_failed(
            &current_claim,
            "SQLite writer lease `sqlite-writer` for mempal-daemon-1000-current-start was lost before build daemon hook drawer records",
        )
        .expect("record current identity lease loss");

    assert_eq!(
        store
            .requeue_writer_lease_failures_for_daemon_start(current_owner)
            .expect("requeue previous identity failures"),
        1
    );
    let reclaimed = store
        .claim_next(current_owner, 60)
        .expect("claim rebound hook")
        .expect("rebound hook must be runnable immediately");
    assert_eq!(reclaimed.id, id);
    assert_eq!(reclaimed.retry_count, 0);
    assert_eq!(
        store.stats().expect("stats after daemon rebind").retrying,
        1,
        "current daemon identity failure must retain its retry backoff"
    );
}

#[test]
fn test_queue_failed_preview_retry_and_archive_are_filtered() {
    let (_tmp, db_path, store) = new_store(QueueConfig::default());
    let malformed = store
        .enqueue("ingest_async", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue malformed");
    let storage = store
        .enqueue("hook_post_tool", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue storage");
    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET status = 'failed', failure_class = 'terminal', retry_count = 1, last_error = 'failed to decode queued hook envelope', op_state = 'failed' WHERE id = ?1",
        [malformed.as_str()],
    )
    .expect("mark malformed failed");
    conn.execute(
        "UPDATE pending_messages SET status = 'failed', failure_class = 'terminal', retry_count = 2, last_error = 'failed to reopen db for merge drawer_hooks_raw_bash_69633781', op_state = 'failed' WHERE id = ?1",
        [storage.as_str()],
    )
    .expect("mark storage failed");
    drop(conn);

    assert!(
        store
            .retry_failed_messages(QueueFailureFilter::default())
            .is_err(),
        "unfiltered retry must be rejected"
    );
    assert!(
        store
            .archive_failed_messages(QueueFailureFilter::default())
            .is_err(),
        "unfiltered archive must be rejected"
    );

    let malformed_filter = QueueFailureFilter {
        kind: Some("ingest_async".to_string()),
        retry_class: None,
        reason_code: Some("invalid_hook_envelope".to_string()),
    };
    let preview = store
        .preview_failed_action(malformed_filter.clone())
        .expect("preview malformed archive");
    assert_eq!(preview.matched, 1);
    let failed_before = store.stats().expect("stats before archive").failed;
    assert_eq!(failed_before, 2, "preview must not mutate failed rows");

    let archived = store
        .archive_failed_messages(malformed_filter)
        .expect("archive malformed");
    assert_eq!(archived.changed, 1);
    let storage_filter = QueueFailureFilter {
        kind: Some("hook_post_tool".to_string()),
        retry_class: Some("terminal".to_string()),
        reason_code: Some("storage_merge_reopen".to_string()),
    };
    let retried = store
        .retry_failed_messages(storage_filter)
        .expect("retry storage row");
    assert_eq!(retried.changed, 1);

    let conn = Connection::open(&db_path).expect("open sqlite");
    let pending_failed = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_messages WHERE status = 'failed'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count failed rows");
    let pending_ready = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_messages WHERE status = 'pending' AND last_error IS NULL",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count pending rows");
    let archived_failed = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_message_completions WHERE op_state = 'failed'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count archived rows");
    assert_eq!(pending_failed, 0);
    assert_eq!(pending_ready, 1);
    assert_eq!(archived_failed, 1);
    assert_eq!(
        store.stats().expect("stats after actions").failed_archived,
        1
    );
}

#[test]
fn test_manual_failed_ingest_requeue_respects_active_byte_budget() {
    let (_tmp, _db_path, store) = new_store(QueueConfig {
        max_ingest_active_bytes: 1_000,
        ..QueueConfig::default()
    });
    for _ in 0..2 {
        let id = store
            .enqueue("ingest_async", &"m".repeat(600))
            .expect("enqueue failed ingest candidate");
        store
            .mark_model_task_failed_retryable(&id, "temporary embedding outage")
            .expect("mark ingest candidate failed");
    }

    let outcome = store
        .retry_failed_messages(QueueFailureFilter {
            kind: Some("ingest_async".to_string()),
            retry_class: None,
            reason_code: None,
        })
        .expect("retry failed ingest rows");

    assert_eq!(outcome.matched, 2);
    assert_eq!(outcome.changed, 1);
    let stats = store.stats().expect("queue stats after manual retry");
    assert_eq!(stats.pending, 1);
    assert_eq!(stats.failed, 1);
    assert_eq!(stats.active_ingest_payload_bytes, 600);
}

#[test]
fn test_queue_retry_and_archive_revalidate_filter_at_execute_time() {
    let (_tmp, db_path, store) = new_store(QueueConfig::default());
    let retry_target = store
        .enqueue("hook_post_tool", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue retry target");
    let archive_target = store
        .enqueue("hook_post_tool", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue archive target");
    let storage_filter = QueueFailureFilter {
        kind: Some("hook_post_tool".to_string()),
        retry_class: Some("terminal".to_string()),
        reason_code: Some("storage_merge_reopen".to_string()),
    };
    let conn = Connection::open(&db_path).expect("open sqlite");
    for id in [&retry_target, &archive_target] {
        conn.execute(
            "UPDATE pending_messages SET status = 'failed', failure_class = 'terminal', retry_count = 2, last_error = 'failed to reopen db for merge drawer_hooks_raw_bash_69633781', op_state = 'failed' WHERE id = ?1",
            [id.as_str()],
        )
        .expect("mark storage failed");
    }
    drop(conn);

    let preview = store
        .preview_failed_action(storage_filter.clone())
        .expect("preview storage filter");
    assert_eq!(preview.matched, 2);

    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET last_error = 'automatic hook LLM gate failed before durable insert' WHERE id = ?1",
        [retry_target.as_str()],
    )
    .expect("change retry target reason");
    conn.execute(
        "UPDATE pending_messages SET failure_class = 'retryable_model' WHERE id = ?1",
        [archive_target.as_str()],
    )
    .expect("change archive target class");
    drop(conn);

    let retried = store
        .retry_failed_messages(storage_filter.clone())
        .expect("retry with stale filter");
    assert_eq!(retried.matched, 0);
    assert_eq!(retried.changed, 0);
    let archived = store
        .archive_failed_messages(storage_filter)
        .expect("archive with stale filter");
    assert_eq!(archived.matched, 0);
    assert_eq!(archived.changed, 0);

    let still_failed = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT COUNT(*) FROM pending_messages WHERE status = 'failed'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count failed rows");
    assert_eq!(still_failed, 2);
}

#[test]
fn test_failed_archived_counts_only_queue_archive_provenance() {
    let (_tmp, db_path, store) = new_store(QueueConfig::default());
    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute_batch(
        r#"
        INSERT INTO pending_message_completions (
            message_id,
            kind,
            created_at,
            claimed_at,
            completed_at,
            processing_ms,
            op_state,
            rejected_reason
        )
        VALUES (
            'ordinary-failed-completion',
            'ingest_async',
            1700000000000,
            1700000001000,
            1700000002000,
            1000,
            'failed',
            'normal_ingest_failure'
        );
        "#,
    )
    .expect("insert ordinary failed completion");
    drop(conn);

    assert_eq!(store.stats().expect("ordinary stats").failed_archived, 0);

    let malformed = store
        .enqueue("ingest_async", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue malformed");
    Connection::open(&db_path)
        .expect("open sqlite")
        .execute(
            "UPDATE pending_messages SET status = 'failed', failure_class = 'terminal', retry_count = 1, last_error = 'failed to decode queued hook envelope', op_state = 'failed' WHERE id = ?1",
            [malformed.as_str()],
        )
        .expect("mark malformed failed");
    store
        .archive_failed_messages(QueueFailureFilter {
            kind: Some("ingest_async".to_string()),
            retry_class: None,
            reason_code: Some("invalid_hook_envelope".to_string()),
        })
        .expect("archive malformed");
    assert_eq!(store.stats().expect("archived stats").failed_archived, 1);
}

#[test]
fn test_queue_cli_failed_summary_and_retry_dry_run_are_aggregate_only() {
    let (home, db_path) = setup_home();
    let store = PendingMessageStore::new(&db_path).expect("create store");
    let malformed = store
        .enqueue("ingest_async", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue malformed");
    let storage = store
        .enqueue("hook_post_tool", "RAW_PAYLOAD_SHOULD_NOT_APPEAR")
        .expect("enqueue storage");
    let conn = Connection::open(&db_path).expect("open sqlite");
    conn.execute(
        "UPDATE pending_messages SET status = 'failed', failure_class = 'terminal', retry_count = 1, last_error = 'failed to decode queued hook envelope', op_state = 'failed' WHERE id = ?1",
        [malformed.as_str()],
    )
    .expect("mark malformed failed");
    conn.execute(
        "UPDATE pending_messages SET status = 'failed', failure_class = 'terminal', retry_count = 2, last_error = 'failed to reopen db for merge drawer_hooks_raw_bash_69633781', op_state = 'failed' WHERE id = ?1",
        [storage.as_str()],
    )
    .expect("mark storage failed");
    drop(conn);

    let stats_output = Command::new(mempal_bin())
        .arg("stats")
        .env("HOME", home.path())
        .output()
        .expect("run mempal stats");
    assert!(stats_output.status.success(), "{stats_output:?}");
    let stats_stdout = String::from_utf8(stats_output.stdout).expect("stats stdout utf8");
    assert!(stats_stdout.contains("failed: 2"), "{stats_stdout}");
    assert!(
        stats_stdout.contains("failed_terminal: 2"),
        "{stats_stdout}"
    );
    assert!(
        stats_stdout.contains("failed_retryable: 0"),
        "{stats_stdout}"
    );
    assert!(
        stats_stdout.contains("failed_by_kind_class_reason:"),
        "{stats_stdout}"
    );
    assert!(
        stats_stdout.contains("invalid_hook_envelope"),
        "{stats_stdout}"
    );
    assert!(
        stats_stdout.contains("storage_merge_reopen"),
        "{stats_stdout}"
    );
    assert!(
        !stats_stdout.contains("RAW_PAYLOAD_SHOULD_NOT_APPEAR"),
        "{stats_stdout}"
    );

    let failed_output = Command::new(mempal_bin())
        .args(["queue", "failed", "--reason", "storage_merge_reopen"])
        .env("HOME", home.path())
        .output()
        .expect("run mempal queue failed");
    assert!(failed_output.status.success(), "{failed_output:?}");
    let failed_stdout = String::from_utf8(failed_output.stdout).expect("queue failed stdout utf8");
    assert!(failed_stdout.contains("filter: reason=storage_merge_reopen"));
    assert!(failed_stdout.contains("matched_failed: 1"));
    assert!(failed_stdout.contains("storage_merge_reopen"));
    assert!(!failed_stdout.contains("RAW_PAYLOAD_SHOULD_NOT_APPEAR"));

    let retry_dry_run = Command::new(mempal_bin())
        .args([
            "queue",
            "retry-failed",
            "--kind",
            "hook_post_tool",
            "--reason",
            "storage_merge_reopen",
        ])
        .env("HOME", home.path())
        .output()
        .expect("run mempal queue retry dry-run");
    assert!(retry_dry_run.status.success(), "{retry_dry_run:?}");
    let retry_stdout = String::from_utf8(retry_dry_run.stdout).expect("retry stdout utf8");
    assert!(retry_stdout.contains("dry_run: true"), "{retry_stdout}");
    assert!(retry_stdout.contains("matched: 1"), "{retry_stdout}");
    assert!(
        !retry_stdout.contains("RAW_PAYLOAD_SHOULD_NOT_APPEAR"),
        "{retry_stdout}"
    );
    let archive_dry_run = Command::new(mempal_bin())
        .args([
            "queue",
            "archive-failed",
            "--kind",
            "ingest_async",
            "--reason",
            "invalid_hook_envelope",
        ])
        .env("HOME", home.path())
        .output()
        .expect("run mempal queue archive dry-run");
    assert!(archive_dry_run.status.success(), "{archive_dry_run:?}");
    let archive_stdout = String::from_utf8(archive_dry_run.stdout).expect("archive stdout utf8");
    assert!(archive_stdout.contains("dry_run: true"), "{archive_stdout}");
    assert!(archive_stdout.contains("matched: 1"), "{archive_stdout}");
    assert!(
        !archive_stdout.contains("RAW_PAYLOAD_SHOULD_NOT_APPEAR"),
        "{archive_stdout}"
    );
    let failed_after_dry_run = Connection::open(&db_path)
        .expect("open sqlite")
        .query_row(
            "SELECT COUNT(*) FROM pending_messages WHERE status = 'failed'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("count failed after dry-run");
    assert_eq!(failed_after_dry_run, 2);

    let unfiltered_execute = Command::new(mempal_bin())
        .args(["queue", "retry-failed", "--execute"])
        .env("HOME", home.path())
        .output()
        .expect("run unfiltered queue retry");
    assert!(!unfiltered_execute.status.success());
    let stderr = String::from_utf8(unfiltered_execute.stderr).expect("stderr utf8");
    assert!(
        stderr.contains("requires at least one explicit --kind, --class, or --reason filter"),
        "{stderr}"
    );
}

#[test]
fn test_queue_help_documents_failed_recovery_examples() {
    let output = Command::new(mempal_bin())
        .args(["queue", "--help"])
        .output()
        .expect("run mempal queue help");
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("help stdout utf8");
    assert!(stdout.contains("invalid_hook_envelope"), "{stdout}");
    assert!(stdout.contains("automatic_hook_llm_gate"), "{stdout}");
    assert!(stdout.contains("llm_response_decode"), "{stdout}");
    assert!(stdout.contains("storage_merge_reopen"), "{stdout}");
    assert!(stdout.contains("retry-failed"), "{stdout}");
    assert!(stdout.contains("archive-failed"), "{stdout}");
}

#[test]
fn test_auto_requeue_model_tasks_only_requeues_retryable_failed_kind() {
    let (_tmp, db_path, store) = new_store(QueueConfig::default());
    let retryable_embed = store
        .enqueue("hook_event", r#"{"n":1}"#)
        .expect("enqueue retryable embed");
    let retryable_llm = store
        .enqueue("llm_task", r#"{"n":2}"#)
        .expect("enqueue retryable llm");
    let terminal_embed = store
        .enqueue("hook_event", r#"{"n":3}"#)
        .expect("enqueue terminal embed");

    store
        .mark_model_task_failed_retryable(&retryable_embed, "temporary embed outage")
        .expect("mark retryable embed failed");
    store
        .mark_model_task_failed_retryable(&retryable_llm, "temporary llm outage")
        .expect("mark retryable llm failed");
    let terminal_claim = store
        .claim_next("worker-terminal", 60)
        .expect("claim")
        .expect("terminal claim");
    assert_eq!(terminal_claim.id, terminal_embed);
    store
        .mark_failed_with_disposition(
            &terminal_claim,
            "invalid configuration",
            QueueFailureDisposition::Terminal,
        )
        .expect("mark terminal failed");

    let outcome = store
        .auto_requeue_failed_model_tasks("embedding")
        .expect("auto requeue embedding");

    assert_eq!(outcome.requeued, 1);
    assert_eq!(outcome.skipped, 0);
    let conn = Connection::open(&db_path).expect("open sqlite");
    let status_for = |id: &str| -> (String, i64, Option<String>) {
        conn.query_row(
            "SELECT status, retry_count, last_error FROM pending_messages WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read queue row")
    };
    assert_eq!(
        status_for(&retryable_embed),
        ("pending".to_string(), 0, None)
    );
    assert_eq!(
        status_for(&retryable_llm),
        (
            "failed".to_string(),
            0,
            Some("temporary llm outage".to_string())
        )
    );
    assert_eq!(
        status_for(&terminal_embed),
        (
            "failed".to_string(),
            1,
            Some("invalid configuration".to_string())
        )
    );

    let last_auto_requeue_at = conn
        .query_row(
            "SELECT value FROM fork_ext_meta WHERE key = 'queue.auto_requeue.last_at_unix_ms'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("last auto requeue timestamp");
    assert!(
        last_auto_requeue_at
            .parse::<u64>()
            .is_ok_and(|value| value > 0),
        "{last_auto_requeue_at}"
    );
}

#[test]
fn test_auto_failed_ingest_requeue_respects_active_byte_budget() {
    let (_tmp, _db_path, store) = new_store(QueueConfig {
        max_ingest_active_bytes: 1_000,
        ..QueueConfig::default()
    });
    for _ in 0..2 {
        let id = store
            .enqueue("ingest_async", &"m".repeat(600))
            .expect("enqueue failed ingest candidate");
        store
            .mark_model_task_failed_retryable(&id, "temporary embedding outage")
            .expect("mark ingest candidate failed");
    }

    let outcome = store
        .auto_requeue_failed_model_tasks("embedding")
        .expect("auto requeue failed ingest rows");

    assert_eq!(outcome.requeued, 1);
    assert_eq!(outcome.skipped, 1);
    let stats = store.stats().expect("queue stats after automatic retry");
    assert_eq!(stats.pending, 1);
    assert_eq!(stats.failed, 1);
    assert_eq!(stats.active_ingest_payload_bytes, 600);
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
