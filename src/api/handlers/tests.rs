use super::*;

#[tokio::test]
async fn stale_daemon_response_identifies_restart_safe_boundary() {
    let response = ApiError::stale_daemon(crate::stale_daemon::StaleDaemonDiagnostic {
        stale_daemon: true,
        daemon_pid: 706_141,
        exe_deleted: true,
        retry_safe_after_restart: true,
    })
    .into_response();

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read stale-daemon response body");
    let body: serde_json::Value =
        serde_json::from_slice(&body).expect("parse stale-daemon response body");
    let error = &body["error"];
    assert_eq!(error["kind"], "stale_daemon");
    assert_eq!(error["stale_daemon"], true);
    assert_eq!(error["daemon_pid"], 706_141);
    assert_eq!(error["exe_deleted"], true);
    assert_eq!(error["retryable"], false);
    assert_eq!(error["retry_safe_after_restart"], true);
    assert!(error.get("exe_path").is_none());
    assert!(
        error["recovery_hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("mempal daemon restart"))
    );
}

fn sqlite_lock_error(code: rusqlite::ErrorCode, extended_code: i32) -> DbError {
    DbError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error {
            code,
            extended_code,
        },
        Some("database is locked".to_string()),
    ))
}

#[test]
fn sqlite_busy_maps_to_retryable_503() {
    let error = db_error_to_api_error(sqlite_lock_error(
        rusqlite::ErrorCode::DatabaseBusy,
        rusqlite::ffi::SQLITE_BUSY,
    ));

    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.kind, "database_busy");
    assert_eq!(error.retryable, Some(true));
    assert_eq!(error.recovery_hint, Some(REST_WRITE_DATABASE_BUSY_HINT));
}

#[test]
fn sqlite_protocol_maps_to_retryable_503() {
    let error = db_error_to_api_error(sqlite_lock_error(
        rusqlite::ErrorCode::FileLockingProtocolFailed,
        rusqlite::ffi::SQLITE_PROTOCOL,
    ));

    assert_eq!(error.status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.kind, "database_busy");
    assert_eq!(error.retryable, Some(true));
}

#[tokio::test]
async fn sqlite_busy_response_body_is_retryable_non_500() {
    let response = db_error_to_api_error(sqlite_lock_error(
        rusqlite::ErrorCode::DatabaseLocked,
        rusqlite::ffi::SQLITE_LOCKED,
    ))
    .into_response();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let body = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("read body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(body["error"]["kind"], "database_busy");
    assert_eq!(body["error"]["retryable"], true);
    assert_ne!(body["error"]["status"], 500);
    assert!(
        body["error"]["recovery_hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("retry")),
        "body={body}"
    );
}
