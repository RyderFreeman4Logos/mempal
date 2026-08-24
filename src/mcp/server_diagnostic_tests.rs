#[test]
fn test_status_database_diagnostic_classifies_sqlite_failures() {
    let busy = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseBusy,
            extended_code: rusqlite::ffi::SQLITE_BUSY,
        },
        Some("database is locked".to_string()),
    );
    let permission = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::PermissionDenied,
            extended_code: rusqlite::ffi::SQLITE_PERM,
        },
        Some("permission denied".to_string()),
    );
    let invalid = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::NotADatabase,
            extended_code: rusqlite::ffi::SQLITE_NOTADB,
        },
        Some("file is not a database".to_string()),
    );
    let protocol = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::FileLockingProtocolFailed,
            extended_code: rusqlite::ffi::SQLITE_PROTOCOL,
        },
        Some("Database lock protocol error".to_string()),
    );

    assert_eq!(status_db_failure_kind(&busy), "locked_or_busy");
    assert_eq!(status_db_failure_kind(&protocol), "locked_or_busy");
    assert_eq!(status_db_failure_kind(&permission), "path_or_permission");
    assert_eq!(status_db_failure_kind(&invalid), "corrupt_or_invalid");

    let audit_write = crate::core::db::DbError::AuditWrite {
        path: PathBuf::from("/tmp/private-audit.log"),
        source: std::io::Error::other("audit write failed"),
    };
    assert_eq!(status_db_failure_kind(&audit_write), "audit_write_failed");

    let non_budget_admission_io = crate::core::db_admission::DbAdmissionError::Io {
        path: PathBuf::from("/tmp/profile-database-holder-budget-exceeded.db"),
        source: std::io::Error::other("holder budget exceeded while reading metadata"),
    };
    assert_eq!(status_db_failure_kind(&non_budget_admission_io), "unknown");
}

#[test]
fn test_mcp_ingest_database_write_refused_error_classifies_sqlite_protocol() {
    let protocol = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::FileLockingProtocolFailed,
            extended_code: rusqlite::ffi::SQLITE_PROTOCOL,
        },
        Some("Database lock protocol error".to_string()),
    );

    let error =
        database_write_refused_error(Path::new("/tmp/palace.db"), "async_db", &protocol);

    assert!(error.message.contains("locked_or_busy"));
    let data = error.data.expect("structured error data");
    assert_eq!(
        data.get("reason").and_then(Value::as_str),
        Some("database_locked")
    );
    assert_eq!(
        data.get("action").and_then(Value::as_str),
        Some("retry_after_transient_lock")
    );
    let diagnostic = data
        .get("database_diagnostic")
        .expect("database diagnostic payload");
    assert_eq!(
        diagnostic.get("failure_kind").and_then(Value::as_str),
        Some("locked_or_busy")
    );
}

#[test]
fn test_mcp_writer_lease_lost_diagnostic_is_typed_and_redacted() {
    let error = crate::core::db::DbError::RuntimeWriterLeaseLost {
        lease_name: "sqlite-writer".to_string(),
        owner: "mempal-daemon-test".to_string(),
        generation: 7,
        operation: "record MCP ingest tier2 audit",
    };

    let error = database_write_refused_error(
        Path::new("/tmp/palace.db"),
        "record MCP ingest tier2 audit",
        &error,
    );

    assert!(
        error
            .message
            .contains("SQLite writer lease was lost before record MCP ingest tier2 audit")
    );
    assert!(!error.message.contains("mempal-daemon-test"));
    let data = error.data.expect("structured error data");
    let diagnostic = data
        .get("database_diagnostic")
        .expect("database diagnostic payload");
    assert_eq!(
        diagnostic.get("failure_kind").and_then(Value::as_str),
        Some("writer_lease_lost")
    );
    assert_eq!(
        diagnostic.get("source").and_then(Value::as_str),
        Some("record MCP ingest tier2 audit")
    );
    assert!(diagnostic.get("path").is_none());
    assert!(diagnostic.get("summary").is_none());
}

#[test]
fn test_mcp_search_database_warning_classifies_sqlite_busy() {
    let busy = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseBusy,
            extended_code: rusqlite::ffi::SQLITE_BUSY,
        },
        Some("database is locked".to_string()),
    );
    let mut response_warnings = Vec::new();
    let mut system_warnings = Vec::new();

    let handled = push_mcp_search_database_warning(
        &mut response_warnings,
        &mut system_warnings,
        Path::new("/tmp/palace.db"),
        "BM25 fallback search",
        &busy,
    );

    assert!(handled);
    assert!(
        response_warnings
            .iter()
            .any(|warning| warning.contains("locked_or_busy")
                && warning.contains("bounded empty response")),
        "response warning should expose locked DB diagnostic: {response_warnings:?}"
    );
    assert!(system_warnings.iter().any(|warning| {
        warning.source == "database" && warning.message.contains("locked_or_busy")
    }));
}

#[test]
fn test_mcp_ingest_database_write_refused_error_classifies_sqlite_busy() {
    let busy = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code: rusqlite::ErrorCode::DatabaseBusy,
            extended_code: rusqlite::ffi::SQLITE_BUSY,
        },
        Some("database is locked".to_string()),
    );

    let error = database_write_refused_error(Path::new("/tmp/palace.db"), "async_db", &busy);

    assert!(error.message.contains("write admission was not confirmed"));
    assert!(error.message.contains("locked_or_busy"));
    let data = error.data.expect("structured error data");
    assert_eq!(
        data.get("reason").and_then(Value::as_str),
        Some("database_locked")
    );
    assert_eq!(
        data.get("action").and_then(Value::as_str),
        Some("retry_after_transient_lock")
    );
    let diagnostic = data
        .get("database_diagnostic")
        .expect("database diagnostic payload");
    assert!(
        diagnostic.get("path").is_none(),
        "database path must stay off the MCP wire"
    );
    assert_eq!(
        diagnostic.get("source").and_then(Value::as_str),
        Some("async_db")
    );
    assert_eq!(
        diagnostic.get("failure_kind").and_then(Value::as_str),
        Some("locked_or_busy")
    );
    assert!(
        diagnostic.get("summary").is_none(),
        "backend summary must stay off the MCP wire: {diagnostic}"
    );
    assert!(
        diagnostic
            .get("hint")
            .and_then(Value::as_str)
            .is_some_and(|hint| hint.contains("wait for the writer to finish")),
        "diagnostic hint should guide retry: {diagnostic}"
    );
    assert!(
        data.get("db_holder_summary").is_some(),
        "structured lock error should include holder summary: {data}"
    );
    assert!(
        data.get("safe_next_step").is_some(),
        "structured lock error should include safe next step: {data}"
    );
    assert!(
        data.get("system_warnings")
            .and_then(Value::as_array)
            .is_some_and(|warnings| warnings.iter().any(|warning| {
                warning.get("source").and_then(Value::as_str) == Some("database")
                    && warning
                        .get("message")
                        .and_then(Value::as_str)
                        .is_some_and(|message| message.contains("locked_or_busy"))
            })),
        "structured data should include database system warning: {data}"
    );
}

    #[tokio::test]
    async fn test_mcp_search_returns_diagnostic_when_database_cannot_open() {
        let tempdir = tempfile::tempdir().expect("tempdir");
        let db_path = tempdir.path().join("palace.db");
        fs::create_dir(&db_path).expect("create directory at db path");
        let server = MempalMcpServer::new_with_factory(
            db_path,
            Arc::new(StubEmbedderFactory {
                vector: vec![0.1, 0.2, 0.3],
            }),
        )
        .expect("create MCP server");

        let response = server
            .mempal_search(Parameters(SearchRequest {
                query: "open failure marker".to_string(),
                top_k: Some(1),
                disable_progressive: Some(true),
                ..SearchRequest::default()
            }))
            .await
            .expect("search should return an actionable diagnostic response")
            .0;

        assert_eq!(response.search_mode, SearchMode::Bm25Only.as_str());
        assert!(response.results.is_empty());
        assert!(
            response.warnings.iter().any(|warning| {
                warning.contains("path_or_permission") && warning.contains("bounded empty response")
            }),
            "response warning should expose database diagnostic: {:?}",
            response.warnings
        );
        assert!(response.system_warnings.iter().any(|warning| {
            warning.source == "database" && warning.message.contains("path_or_permission")
        }));
    }
