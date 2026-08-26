use super::*;

fn setup_rest_mcp_server() -> (
    TempDir,
    std::path::PathBuf,
    MempalMcpServer,
    mempal::api::ApiState,
) {
    let tmp = short_tempdir();
    let db_path = tmp.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize db"));
    let factory = Arc::new(StubEmbedderFactory {
        vector: vec![0.1, 0.2, 0.3],
    });
    let server = MempalMcpServer::new_with_factory_and_config(
        db_path.clone(),
        isolated_mcp_test_config(&db_path),
        factory.clone(),
    )
    .expect("create MCP server");
    let state = mempal::api::ApiState::with_write_queue_config(
        db_path.clone(),
        factory,
        10,
        Duration::from_secs(2),
    );
    (tmp, db_path, server, state)
}

#[test]
fn test_mcp_fixture_config_fails_closed_without_a_live_rest_daemon() {
    let tmp = short_tempdir();
    let db_path = tmp.path().join("palace.db");
    let config = isolated_mcp_test_config(&db_path);

    assert_eq!(config.db_path, db_path.display().to_string());
    assert_eq!(config.api.addr, HERMETIC_REST_ADDR);
}

async fn post_rest_ingest(state: mempal::api::ApiState, payload: Value) -> (StatusCode, Value) {
    let response = mempal::api::router(state)
        .oneshot(
            Request::builder()
                .method(Method::POST)
                .uri("/api/ingest")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(
                    serde_json::to_vec(&payload).expect("serialize rest payload"),
                ))
                .expect("build rest request"),
        )
        .await
        .expect("rest request should complete");
    let status = response.status();
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read rest response body");
    let body = serde_json::from_slice(&bytes).expect("parse rest response body");
    (status, body)
}

#[tokio::test]
async fn test_rest_ingest_default_evidence_drawer_id_matches_mcp() {
    let (_tmp, _db_path, server, state) = setup_rest_mcp_server();
    let content = "REST default identity body";
    let (status, body) = post_rest_ingest(
        state,
        json!({
            "content": content,
            "wing": "mempal",
            "room": "identity"
        }),
    )
    .await;
    let mcp = server
        .ingest_json_for_test(json!({
            "content": content,
            "wing": "mempal",
            "room": "identity",
            "dry_run": true
        }))
        .await
        .expect("mcp dry-run should succeed");

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["drawer_id"], mcp.drawer_id);
    assert_ne!(
        body["drawer_id"],
        build_drawer_id("mempal", Some("identity"), content)
    );
}

#[tokio::test]
async fn test_rest_ingest_stores_source_confidence_params() {
    let (_tmp, db_path, _server, state) = setup_rest_mcp_server();
    let (status, body) = post_rest_ingest(
        state,
        json!({
            "content": "REST source confidence body",
            "wing": "mempal",
            "room": "confidence",
            "source_type": "agent_observation",
            "confidence": 0.73
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    let drawer_id = body["drawer_id"].as_str().expect("drawer_id string");
    let db = Database::open(&db_path).expect("open db after REST ingest");
    let drawer = db
        .get_drawer(drawer_id)
        .expect("load drawer")
        .expect("drawer exists");

    assert_eq!(drawer.source_type, SourceType::AgentObservation);
    assert!((drawer.confidence - 0.73).abs() < f64::EPSILON);
}

#[tokio::test]
async fn test_rest_after_mcp_default_ingest_reuses_existing_bootstrap_drawer() {
    let (_tmp, db_path, server, state) = setup_rest_mcp_server();
    let content = "Shared default identity body";
    let mcp = server
        .ingest_json_for_test(json!({
            "content": content,
            "wing": "mempal",
            "room": "identity"
        }))
        .await
        .expect("mcp write should succeed");

    let (status, body) = post_rest_ingest(
        state,
        json!({
            "content": content,
            "wing": "mempal",
            "room": "identity"
        }),
    )
    .await;

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["drawer_id"], mcp.drawer_id);
    let db = Database::open(&db_path).expect("open db after REST and MCP ingest");
    assert_eq!(db.drawer_count().expect("drawer count"), 1);
}

#[tokio::test]
async fn test_rest_ingest_preserves_typed_fields_on_bootstrap_identity() {
    let (_tmp, db_path, _server, state) = setup_rest_mcp_server();
    let content = "REST typed fields are persisted";
    let statement = "REST should preserve typed knowledge metadata.";
    let (status, body) = post_rest_ingest(
        state,
        json!({
            "content": content,
            "wing": "mempal",
            "room": "identity",
            "memory_kind": "knowledge",
            "statement": statement,
            "tier": "shu",
            "status": "promoted",
            "supporting_refs": ["drawer_ev_001"]
        }),
    )
    .await;
    let expected = expected_bootstrap_evidence_id(
        "mempal",
        Some("identity"),
        content,
        &SourceType::AgentInference,
    );

    assert_eq!(status, StatusCode::CREATED);
    assert_eq!(body["drawer_id"], expected);
    let db = Database::open(&db_path).expect("open db after REST ingest");
    let drawer = db
        .get_drawer(&expected)
        .expect("load rest drawer")
        .expect("rest drawer exists");
    assert_eq!(drawer.memory_kind, MemoryKind::Knowledge);
    assert_eq!(drawer.statement.as_deref(), Some(statement));
    assert_eq!(drawer.tier, Some(KnowledgeTier::Shu));
    assert_eq!(drawer.status, Some(KnowledgeStatus::Promoted));
    assert_eq!(drawer.supporting_refs, vec!["drawer_ev_001"]);
}
