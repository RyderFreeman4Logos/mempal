use super::*;

#[tokio::test]
async fn daemon_mcp_listen_port_reserves_session_activity_before_eviction() -> Result<()> {
    let (_tempdir, db_path, _config, server) = fixture()?;
    let daemon_db =
        AsyncDb::open_for(&db_path, 4, DbHolderClass::Daemon).context("open daemon-owned pool")?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    let (incoming, incoming_rx) = tokio::sync::mpsc::channel(32);
    let (_outgoing_tx, outgoing_rx) = tokio::sync::mpsc::channel(32);
    let request_lock = Arc::new(tokio::sync::Mutex::new(()));
    let active_requests = Arc::new(AtomicUsize::new(0));
    let target = HttpSession {
        incoming,
        outgoing: Arc::new(tokio::sync::Mutex::new(outgoing_rx)),
        request_lock: Arc::clone(&request_lock),
        active_requests: Arc::clone(&active_requests),
        last_used: Arc::new(Mutex::new(Instant::now())),
    };
    let target_id = "target";
    {
        let mut sessions_guard = sessions.lock().expect("session registry lock");
        sessions_guard.insert(target_id.to_string(), target.clone());
        for id in 0..MAX_HTTP_SESSIONS - 1 {
            let (incoming, _incoming_rx) = tokio::sync::mpsc::channel(1);
            let (_outgoing_tx, outgoing_rx) = tokio::sync::mpsc::channel(1);
            sessions_guard.insert(
                format!("other-{id}"),
                HttpSession {
                    incoming,
                    outgoing: Arc::new(tokio::sync::Mutex::new(outgoing_rx)),
                    request_lock: Arc::new(tokio::sync::Mutex::new(())),
                    active_requests: Arc::new(AtomicUsize::new(0)),
                    last_used: Arc::new(Mutex::new(Instant::now())),
                },
            );
        }
    }
    let state = HttpState {
        base_server: server.with_daemon_owned_async_db(daemon_db),
        sessions: Arc::clone(&sessions),
        next_session_id: Arc::new(AtomicU64::new(0)),
        pending_sessions: Arc::new(AtomicUsize::new(0)),
        bound_addr: Some(address),
        initialize_gate: None,
    };
    let _request_lock_guard = request_lock.lock_owned().await;
    let request = Request::builder()
        .uri("/")
        .header(header::HOST, address.to_string())
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header(MCP_SESSION_ID_HEADER, target_id)
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":1,"method":"roots/list","params":{}}"#,
        ))?;
    let dispatch = tokio::spawn(handle(State(state.clone()), request));
    tokio::time::timeout(Duration::from_secs(1), async {
        loop {
            if active_requests.load(Ordering::Acquire) == 1 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("in-flight request did not reserve session activity")?;

    let initialize = Request::builder()
        .uri("/")
        .header(header::HOST, address.to_string())
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"race","version":"0.1"}}}"#,
        ))?;
    let response = handle(State(state), initialize).await;
    anyhow::ensure!(
        response.status() == StatusCode::OK,
        "at-capacity initialize failed: {}",
        response.status()
    );
    anyhow::ensure!(
        sessions
            .lock()
            .expect("session registry lock")
            .contains_key(target_id),
        "in-flight session was evicted during initialize"
    );
    drop(incoming_rx);
    dispatch.abort();
    let _ = dispatch.await;
    Ok(())
}

#[tokio::test]
async fn daemon_mcp_listen_port_rejects_expired_session_on_lookup() -> Result<()> {
    let (_tempdir, db_path, _config, server) = fixture()?;
    let daemon_db =
        AsyncDb::open_for(&db_path, 4, DbHolderClass::Daemon).context("open daemon-owned pool")?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let sessions = Arc::new(Mutex::new(HashMap::new()));
    let (incoming, _incoming_rx) = tokio::sync::mpsc::channel(1);
    let (_outgoing_tx, outgoing_rx) = tokio::sync::mpsc::channel(1);
    let session_id = "expired";
    sessions.lock().expect("session registry lock").insert(
        session_id.to_string(),
        HttpSession {
            incoming,
            outgoing: Arc::new(tokio::sync::Mutex::new(outgoing_rx)),
            request_lock: Arc::new(tokio::sync::Mutex::new(())),
            active_requests: Arc::new(AtomicUsize::new(0)),
            last_used: Arc::new(Mutex::new(
                Instant::now() - HTTP_SESSION_TTL - Duration::from_secs(1),
            )),
        },
    );
    let state = HttpState {
        base_server: server.with_daemon_owned_async_db(daemon_db),
        sessions: Arc::clone(&sessions),
        next_session_id: Arc::new(AtomicU64::new(0)),
        pending_sessions: Arc::new(AtomicUsize::new(0)),
        bound_addr: Some(address),
        initialize_gate: None,
    };
    let request = Request::builder()
        .uri("/")
        .header(header::HOST, address.to_string())
        .header(header::ACCEPT, "application/json, text/event-stream")
        .header(header::CONTENT_TYPE, "application/json")
        .header(MCP_SESSION_ID_HEADER, session_id)
        .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#))?;
    let response = handle(State(state), request).await;
    anyhow::ensure!(
        response.status() == StatusCode::NOT_FOUND,
        "expired MCP session was admitted: {}",
        response.status()
    );
    anyhow::ensure!(
        !sessions
            .lock()
            .expect("session registry lock")
            .contains_key(session_id),
        "expired MCP session remained in the registry"
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn daemon_mcp_listen_port_releases_pending_session_on_initialize_cancel() -> Result<()> {
    let (_tempdir, db_path, _config, server) = fixture()?;
    let daemon_db =
        AsyncDb::open_for(&db_path, 4, DbHolderClass::Daemon).context("open daemon-owned pool")?;
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    drop(listener);
    let initialize_gate = Arc::new(Notify::new());
    let mut state = HttpState {
        base_server: server.with_daemon_owned_async_db(daemon_db),
        sessions: Arc::new(Mutex::new(HashMap::new())),
        next_session_id: Arc::new(AtomicU64::new(0)),
        pending_sessions: Arc::new(AtomicUsize::new(0)),
        bound_addr: Some(address),
        initialize_gate: Some(Arc::clone(&initialize_gate)),
    };

    let initialize = || {
        Request::builder()
            .uri("/")
            .header(header::HOST, address.to_string())
            .header(header::ACCEPT, "application/json, text/event-stream")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"cancel","version":"0.1"}}}"#,
            ))
            .expect("build initialize request")
    };

    for _ in 0..MAX_PENDING_HTTP_SESSIONS {
        let pending_sessions = Arc::clone(&state.pending_sessions);
        let task = tokio::spawn(handle(State(state.clone()), initialize()));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if pending_sessions.load(Ordering::Acquire) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("initialize did not reach its cancellable session start")?;
        task.abort();
        let _ = task.await;
        anyhow::ensure!(
            pending_sessions.load(Ordering::Acquire) == 0,
            "cancelled initialize leaked pending admission"
        );
    }

    state.initialize_gate = None;
    let response = handle(State(state.clone()), initialize()).await;
    anyhow::ensure!(
        response.status() == StatusCode::OK,
        "valid initialize was rejected after cancelled requests: {}",
        response.status()
    );
    anyhow::ensure!(
        state.pending_sessions.load(Ordering::Acquire) == 0,
        "valid initialize left pending admission occupied"
    );
    Ok(())
}
