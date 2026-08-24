use super::*;

#[cfg(unix)]
#[test]
fn test_daemon_sigterm_graceful() {
    let (tmp, db_path, _config_path) = setup_daemon_home();
    let mut server = Server::new();
    let _mock = server
        .mock("POST", "/embeddings")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(r#"{"data":[{"embedding":[0.1,0.2,0.3]}]}"#)
        .create();
    fs::write(
        tmp.path().join(".mempal/config.toml"),
        format!(
            r#"
db_path = "{}"

[embed]
backend = "openai_compat"

[embed.openai_compat]
base_url = "{}"
model = "test-embed"
dim = 3
request_timeout_secs = 5

[hooks]
enabled = true
daemon_poll_interval_ms = 100

[daemon]
log_path = "{}"
"#,
            db_path.display(),
            server.url(),
            tmp.path().join(".mempal/daemon.log").display()
        ),
    )
    .expect("rewrite config");
    let store = PendingMessageStore::new(&db_path).expect("store");
    let envelope = CapturedHookEnvelope {
        event: HookEvent::SessionStart.display_name().to_string(),
        kind: HookEvent::SessionStart.queue_kind().to_string(),
        agent: "claude".to_string(),
        captured_at: "123".to_string(),
        claude_cwd: "/tmp/project".to_string(),
        payload: Some(r#"{"session_id":"abc","cwd":"/tmp/project"}"#.to_string()),
        payload_path: None,
        payload_preview: None,
        original_size_bytes: 32,
        truncated: false,
    };
    let payload = serde_json::to_string(&envelope).expect("serialize envelope");
    store
        .enqueue(HookEvent::SessionStart.queue_kind(), &payload)
        .expect("enqueue");

    let mut child = spawn_foreground_daemon(tmp.path(), "sigterm-graceful");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let count = Database::with_diagnostic_read_only(&db_path, |db| db.drawer_count())
            .expect("diag")
            .expect("count");
        if count > 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    child.signal_or_panic(libc::SIGTERM, "failed to send SIGTERM");
    let status = child.wait_or_panic("failed to wait for daemon");
    assert!(
        status.success(),
        "daemon must exit cleanly after SIGTERM: {status:?}\n{}",
        child.diagnostics()
    );

    let conn = rusqlite::Connection::open(&db_path).expect("open sqlite");
    let claimed: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM pending_messages WHERE status = 'claimed'",
            [],
            |row| row.get(0),
        )
        .expect("claimed count");
    assert_eq!(claimed, 0, "no message may remain claimed after SIGTERM");
    let pid_path = tmp.path().join(".mempal/daemon.pid");
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && pid_path.exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_path.exists(), "daemon pid file must be removed");
}
