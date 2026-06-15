use std::fs;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
#[cfg(unix)]
use std::sync::mpsc;
#[cfg(unix)]
use std::thread;
#[cfg(unix)]
use std::time::Duration;

use mempal::core::{db::Database, queue::PendingMessageStore};
use rusqlite::Connection;
use serde_json::Value;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_home() -> (TempDir, PathBuf) {
    setup_home_with_extra_config("")
}

fn setup_home_without_opening_db() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    fs::write(
        mempal_home.join("config.toml"),
        format!(
            r#"
db_path = "{}"

[hooks]
enabled = true
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    (tmp, db_path)
}

fn setup_home_with_extra_config(extra_config: &str) -> (TempDir, PathBuf) {
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

[hooks]
enabled = true
{extra_config}
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    (tmp, db_path)
}

fn run_hook(home: &TempDir, command: &str, payload: &[u8]) -> std::process::Output {
    let mut child = Command::new(mempal_bin())
        .args(["hook", command])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook command");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload)
        .expect("write payload");
    child.wait_with_output().expect("wait output")
}

fn pending_message_count(db_path: &PathBuf) -> i64 {
    let conn = Connection::open(db_path).expect("open sqlite");
    conn.query_row("SELECT COUNT(*) FROM pending_messages", [], |row| {
        row.get(0)
    })
    .expect("query pending count")
}

#[cfg(unix)]
fn hold_sqlite_write_lock(db_path: PathBuf, hold_for: Duration) -> thread::JoinHandle<()> {
    let (ready_tx, ready_rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let conn = Connection::open(db_path).expect("open sqlite lock connection");
        conn.execute_batch("BEGIN IMMEDIATE;")
            .expect("hold SQLite write lock");
        ready_tx.send(()).expect("signal lock ready");
        thread::sleep(hold_for);
        conn.execute_batch("ROLLBACK;").expect("release lock");
    });
    ready_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("SQLite write lock ready");
    handle
}

#[cfg(unix)]
fn spawn_fake_daemon_ipc(
    home: &TempDir,
    response: &'static str,
    response_delay: Option<Duration>,
) -> thread::JoinHandle<String> {
    let socket_path = home.path().join(".mempal").join("daemon-hook.sock");
    let _ = fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind fake daemon IPC socket");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept hook IPC connection");
        let mut request = String::new();
        let mut reader = BufReader::new(stream.try_clone().expect("clone IPC stream"));
        reader
            .read_line(&mut request)
            .expect("read hook IPC request");
        if let Some(delay) = response_delay {
            thread::sleep(delay);
        }
        let _ = stream.write_all(response.as_bytes());
        if !response.as_bytes().ends_with(b"\n") {
            let _ = stream.write_all(b"\n");
        }
        let _ = stream.flush();
        request
    })
}

#[cfg(unix)]
fn spawn_fake_persisting_daemon_ipc(
    home: &TempDir,
    db_path: PathBuf,
    response_delay: Duration,
) -> thread::JoinHandle<String> {
    spawn_fake_persisting_daemon_ipc_with_response(
        home,
        db_path,
        response_delay,
        Some(r#"{"status":"accepted"}"#),
    )
}

#[cfg(unix)]
fn spawn_fake_persisting_daemon_ipc_with_response(
    home: &TempDir,
    db_path: PathBuf,
    response_delay: Duration,
    response: Option<&'static str>,
) -> thread::JoinHandle<String> {
    let socket_path = home.path().join(".mempal").join("daemon-hook.sock");
    let _ = fs::remove_file(&socket_path);
    let listener = UnixListener::bind(&socket_path).expect("bind fake daemon IPC socket");
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept hook IPC connection");
        let mut request = String::new();
        let mut reader = BufReader::new(stream.try_clone().expect("clone IPC stream"));
        reader
            .read_line(&mut request)
            .expect("read hook IPC request");
        thread::sleep(response_delay);

        let request_json: Value = serde_json::from_str(request.trim()).expect("request json");
        let kind = request_json["kind"].as_str().expect("kind");
        let payload = request_json["payload"].as_str().expect("payload");
        let idempotency_key = request_json["idempotency_key"]
            .as_str()
            .expect("idempotency key");
        PendingMessageStore::new_without_reclaim(&db_path)
            .enqueue_idempotent_with_key(kind, payload, idempotency_key)
            .expect("fake daemon persist");

        if let Some(response) = response {
            let _ = stream.write_all(response.as_bytes());
            if !response.as_bytes().ends_with(b"\n") {
                let _ = stream.write_all(b"\n");
            }
            let _ = stream.flush();
        }
        request
    })
}

#[test]
fn test_hook_post_tool_enqueues_to_queue() {
    let (home, db_path) = setup_home();
    let payload = r#"{"tool_name":"Bash","input":"ls","exit_code":0,"output":"ok"}"#;

    let mut child = Command::new(mempal_bin())
        .args(["hook", "hook_post_tool"])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook command");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    let output = child.wait_with_output().expect("wait output");

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "successful hook stderr must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let conn = Connection::open(db_path).expect("open sqlite");
    let (kind, envelope): (String, String) = conn
        .query_row(
            "SELECT kind, payload FROM pending_messages ORDER BY created_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("query queue");
    assert_eq!(kind, "hook_post_tool");
    let envelope_json: Value = serde_json::from_str(&envelope).expect("envelope json");
    assert_eq!(envelope_json["event"], "PostToolUse");
    assert_eq!(envelope_json["payload"], payload);
}

#[test]
fn test_hook_direct_fallback_initializes_missing_db_before_enqueue() {
    let (home, db_path) = setup_home_without_opening_db();
    let payload = r#"{"prompt":"first run fallback initializes queue"}"#;

    let output = run_hook(&home, "hook_user_prompt", payload.as_bytes());

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "first-run fallback success must stay quiet, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        pending_message_count(&db_path),
        1,
        "hook fallback must create and migrate the queue database before enqueue"
    );
}

#[cfg(unix)]
#[test]
fn test_hook_direct_fallback_waits_for_transient_sqlite_busy() {
    let (home, db_path) = setup_home();
    let lock = hold_sqlite_write_lock(db_path.clone(), Duration::from_millis(300));
    let payload = r#"{"prompt":"transient sqlite busy should be durable"}"#;

    let output = run_hook(&home, "hook_user_prompt", payload.as_bytes());

    lock.join().expect("lock thread");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "fallback success must stay quiet, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        pending_message_count(&db_path),
        1,
        "hook fallback must persist after a transient SQLite write lock"
    );
}

#[cfg(unix)]
#[test]
fn test_hook_daemon_ipc_success_does_not_open_sqlite_when_db_locked() {
    let (home, db_path) = setup_home();
    let lock_conn = Connection::open(&db_path).expect("open lock connection");
    lock_conn
        .execute_batch("BEGIN IMMEDIATE;")
        .expect("hold SQLite write lock");
    let ipc = spawn_fake_daemon_ipc(&home, r#"{"status":"accepted"}"#, None);
    let payload = r#"{"tool_name":"Bash","input":"printf ok","exit_code":0,"output":"ok"}"#;

    let output = run_hook(&home, "hook_post_tool", payload.as_bytes());

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "successful hook stderr must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    lock_conn.execute_batch("ROLLBACK;").expect("release lock");
    assert_eq!(
        pending_message_count(&db_path),
        0,
        "fake daemon ACK means hook must not fall back to direct SQLite enqueue"
    );

    let request = ipc.join().expect("fake daemon IPC thread");
    let request_json: Value = serde_json::from_str(request.trim()).expect("request json");
    assert_eq!(request_json["kind"], "hook_post_tool");
    assert!(
        request_json["idempotency_key"]
            .as_str()
            .is_some_and(|key| !key.is_empty()),
        "daemon IPC request must carry a per-attempt idempotency key"
    );
    let envelope: Value =
        serde_json::from_str(request_json["payload"].as_str().expect("payload string"))
            .expect("envelope json");
    assert_eq!(envelope["event"], "PostToolUse");
    assert_eq!(envelope["payload"], payload);
}

#[cfg(unix)]
#[test]
fn test_hook_ipc_timeout_fallback_dedupes_with_slow_daemon_persist() {
    let (home, db_path) = setup_home();
    let ipc = spawn_fake_persisting_daemon_ipc(&home, db_path.clone(), Duration::from_millis(700));
    let payload = r#"{"prompt":"timeout fallback persists"}"#;

    let output = run_hook(&home, "hook_user_prompt", payload.as_bytes());

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "fallback success must stay quiet, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    let _ = ipc.join().expect("fake daemon IPC thread");
    assert_eq!(
        pending_message_count(&db_path),
        1,
        "slow daemon persist and timeout fallback must share the same attempt key"
    );
}

#[cfg(unix)]
#[test]
fn test_hook_ipc_lost_or_malformed_ack_fallback_dedupes_after_daemon_persist() {
    let cases = [
        ("lost response", None),
        ("malformed response", Some("not-json")),
    ];

    for (label, response) in cases {
        let (home, db_path) = setup_home();
        let ipc = spawn_fake_persisting_daemon_ipc_with_response(
            &home,
            db_path.clone(),
            Duration::from_millis(0),
            response,
        );
        let payload = format!(r#"{{"prompt":"{label} fallback persists once"}}"#);

        let output = run_hook(&home, "hook_user_prompt", payload.as_bytes());

        assert_eq!(output.status.code(), Some(0), "{label}");
        assert!(
            output.stdout.is_empty(),
            "{label}: stdout must stay empty, got {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.stderr.is_empty(),
            "{label}: fallback success must stay quiet, got {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        let _ = ipc.join().expect("fake daemon IPC thread");
        assert_eq!(
            pending_message_count(&db_path),
            1,
            "{label}: daemon persist and fallback must share the same attempt key"
        );
    }
}

#[cfg(unix)]
#[test]
fn test_hook_no_daemon_direct_fallback_preserves_repeated_identical_captures() {
    let (home, db_path) = setup_home();
    let payload = r#"{"prompt":"same prompt repeated while daemon is down"}"#;

    let first = run_hook(&home, "hook_user_prompt", payload.as_bytes());
    let second = run_hook(&home, "hook_user_prompt", payload.as_bytes());

    for output in [first, second] {
        assert_eq!(output.status.code(), Some(0));
        assert!(
            output.stdout.is_empty(),
            "stdout must stay empty, got {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.stderr.is_empty(),
            "direct fallback success must stay quiet, got {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let conn = Connection::open(db_path).expect("open sqlite");
    let ids = conn
        .prepare("SELECT id FROM pending_messages ORDER BY id")
        .expect("prepare id query")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query ids")
        .collect::<rusqlite::Result<Vec<_>>>()
        .expect("collect ids");
    assert_eq!(ids.len(), 2, "direct fallback must keep both hook events");
    assert!(
        ids.iter().all(|id| !id.starts_with("msg-dedup-")),
        "direct fallback must use fresh queue IDs, got {ids:?}"
    );
}

#[cfg(unix)]
#[test]
fn test_hook_ipc_persistence_error_falls_back_to_sqlite_enqueue() {
    let (home, db_path) = setup_home();
    let ipc = spawn_fake_daemon_ipc(
        &home,
        r#"{"status":"error","message":"failed to persist hook IPC capture: database is locked"}"#,
        None,
    );
    let payload = r#"{"prompt":"persistence error fallback persists"}"#;

    let output = run_hook(&home, "hook_user_prompt", payload.as_bytes());

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stderr.is_empty(),
        "fallback success must stay quiet, got {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(pending_message_count(&db_path), 1);
    let _ = ipc.join().expect("fake daemon IPC thread");
}

#[test]
fn test_hook_writes_nothing_to_stdout() {
    let (home, _db_path) = setup_home();
    let payload = r#"{"tool_name":"Bash","input":"ls","exit_code":0,"output":"ok"}"#;

    let mut child = Command::new(mempal_bin())
        .args(["hook", "hook_post_tool"])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook command");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    let output = child.wait_with_output().expect("wait output");

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_passive_hook_success_paths_are_quiet() {
    let cases = [
        (
            "PostToolUse",
            r#"{"tool_name":"shell","tool_input":{"command":"true"},"tool_response":{"exit_code":0,"stdout":"","stderr":""}}"#,
        ),
        ("UserPromptSubmit", r#"{"prompt":"remember the decision"}"#),
        ("SessionStart", r#"{"session_id":"sess-quiet"}"#),
        ("SessionEnd", r#"{"session_id":"sess-quiet"}"#),
    ];

    for (command, payload) in cases {
        let (home, _db_path) = setup_home();
        let output = run_hook(&home, command, payload.as_bytes());

        assert_eq!(output.status.code(), Some(0), "{command} failed");
        assert!(
            output.stdout.is_empty(),
            "{command} stdout is Codex hook protocol; got {:?}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(
            output.stderr.is_empty(),
            "{command} successful stderr must stay empty; got {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn test_hook_envelopes_oversized_payload() {
    let (home, db_path) = setup_home();
    let mut oversized = String::from("{\"payload\":\"");
    oversized.push_str(&"a".repeat((10 * 1024 * 1024) - 64));
    oversized.push('你');
    oversized.push_str(&"b".repeat(1024 * 1024));
    oversized.push_str("\"}");

    let mut child = Command::new(mempal_bin())
        .args(["hook", "hook_post_tool"])
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook command");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(oversized.as_bytes())
        .expect("write payload");
    let output = child.wait_with_output().expect("wait output");

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("payload envelope-wrapped"),
        "stderr should mention envelope-wrapped, got: {stderr}"
    );

    let conn = Connection::open(db_path).expect("open sqlite");
    let envelope: String = conn
        .query_row(
            "SELECT payload FROM pending_messages ORDER BY created_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("query queue");
    let envelope_json: Value = serde_json::from_str(&envelope).expect("envelope json");

    assert_eq!(envelope_json["truncated"], true);
    assert!(
        envelope_json["original_size_bytes"]
            .as_u64()
            .expect("original size")
            > 10_000_000
    );
    let preview = envelope_json["payload_preview"]
        .as_str()
        .expect("preview string");
    assert!(preview.len() <= 4096);
    assert!(
        envelope_json["payload_path"].is_null(),
        "oversized automatic hook capture must not persist raw payload before LLM gate"
    );
    assert!(
        !home.path().join(".mempal").join("hook-oversize").exists(),
        "oversized automatic hook capture must not create hook-oversize files"
    );
}

#[test]
fn test_hook_storage_mode_off_skips_raw_turn_enqueue() {
    let (home, db_path) = setup_home_with_extra_config(
        r#"
[turns]
storage_mode = "off"
raw_turn_wings = ["hooks-raw"]
raw_turn_rooms = []
"#,
    );
    let payload = r#"{"prompt":"do not persist raw turn"}"#;

    let output = run_hook(&home, "hook_user_prompt", payload.as_bytes());

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(pending_message_count(&db_path), 0);
}

#[test]
fn test_hook_storage_mode_off_skips_oversize_payload_file() {
    let (home, db_path) = setup_home_with_extra_config(
        r#"
[turns]
storage_mode = "off"
raw_turn_wings = ["hooks-raw"]
raw_turn_rooms = []
"#,
    );
    let oversized = vec![b'a'; 11 * 1024 * 1024];

    let output = run_hook(&home, "hook_user_prompt", &oversized);

    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "stdout must stay empty, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(pending_message_count(&db_path), 0);
    assert!(
        !home.path().join(".mempal").join("hook-oversize").exists(),
        "off-mode raw turn capture must not write oversize files"
    );
}
