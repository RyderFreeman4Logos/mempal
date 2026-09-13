use super::*;

#[test]
fn test_mcp_context_surfaces_query_read_timeout_error() {
    let (_tempdir, _db_path, server) = setup_server();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .max_blocking_threads(1)
        .enable_all()
        .build()
        .expect("build constrained runtime");
    runtime
        .block_on(server.reader_db())
        .expect("pre-open query-only pool before occupying the blocking pool");
    let server = server.with_mcp_deadline_for_test(Duration::from_millis(20));
    let (release_tx, release_rx) = mpsc::channel::<()>();
    let (started_tx, started_rx) = mpsc::channel();
    runtime.spawn_blocking(move || {
        started_tx.send(()).expect("signal blocking pool occupancy");
        let _ = release_rx.recv_timeout(Duration::from_secs(2));
    });
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("blocking pool is saturated");

    let result = runtime.block_on(async {
        tokio::time::timeout(
            Duration::from_millis(500),
            server.context_json_for_test(serde_json::json!({
                "query": "debug",
                "max_items": 3
            })),
        )
        .await
    });
    drop(release_tx);
    runtime.shutdown_timeout(Duration::from_secs(1));

    let result = result.expect("context should return before client timeout");
    let error = match result {
        Ok(_) => panic!("context must not convert read timeout to empty success"),
        Err(error) => error,
    };
    assert!(
        error
            .to_string()
            .contains("mempal_context query-only database read exceeded"),
        "unexpected error: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_mcp_context_embed_deadline_override_returns_error_without_waiting_for_gate() {
    let tempdir = TempDir::new().expect("short tempdir");
    let db_path = tempdir.path().join("palace.db");
    Database::open(&db_path).expect("open database fixture");
    let call_count = Arc::new(AtomicUsize::new(0));
    let server = MempalMcpServer::new_with_factory(
        db_path,
        Arc::new(BlockingEmbedderFactory {
            vector: vec![0.1, 0.2, 0.3],
            call_count: Arc::clone(&call_count),
            started: Arc::new(Notify::new()),
            gate: Arc::new(Notify::new()),
            released: Arc::new(AtomicBool::new(false)),
        }),
    )
    .expect("create MCP server")
    .with_mcp_deadline_for_test(Duration::from_millis(20));

    let result = tokio::time::timeout(
        Duration::from_millis(500),
        server.context_json_for_test(serde_json::json!({
            "query": "debug",
            "max_items": 3
        })),
    )
    .await
    .expect("context must not wait for the production embed deadline");
    let error = match result {
        Ok(_) => panic!("context must fail closed when query embedding exceeds its deadline"),
        Err(error) => error,
    };

    assert_eq!(call_count.load(Ordering::SeqCst), 1);
    assert!(
        error
            .to_string()
            .contains("mempal_context embedding exceeded"),
        "unexpected error: {error}"
    );
}
