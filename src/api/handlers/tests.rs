use super::*;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, header::CONTENT_TYPE};
use tower::ServiceExt;

struct UnusedEmbedderFactory;

#[async_trait]
impl crate::embed::EmbedderFactory for UnusedEmbedderFactory {
    async fn build(&self) -> crate::embed::Result<Box<dyn crate::embed::Embedder>> {
        Err(crate::embed::EmbedError::Runtime(
            "durable admission must not build an embedder".to_string(),
        ))
    }
}

#[tokio::test]
async fn durable_ingest_receipt_route_is_idempotent() {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let db_path = tempdir.path().join("palace.db");
    Database::open(&db_path).expect("initialize database");
    let state = ApiState::with_write_queue_config(
        db_path,
        Arc::new(UnusedEmbedderFactory),
        4,
        Duration::from_secs(1),
    );
    let request_body = serde_json::json!({
        "idempotency_key": "route-event-1",
        "request": {
            "content": "synthetic route receipt",
            "wing": "receipt-test",
            "room": "route"
        }
    });

    let first = router(state.clone())
        .oneshot(
            Request::post("/api/ingest/durable")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(request_body.to_string()))
                .expect("build durable request"),
        )
        .await
        .expect("durable admission response");
    assert_eq!(first.status(), StatusCode::ACCEPTED);
    let first_body = axum::body::to_bytes(first.into_body(), usize::MAX)
        .await
        .expect("read first receipt");
    let first_receipt: crate::durable_ingest::DurableOperationReceipt =
        serde_json::from_slice(&first_body).expect("decode first receipt");
    assert!(!first_receipt.operation_id.is_empty());
    assert!(!first_receipt.accepted_at.is_empty());
    assert_eq!(first_receipt.state, "queued");

    let replay = router(state.clone())
        .oneshot(
            Request::post("/api/ingest/durable")
                .header(CONTENT_TYPE, "application/json")
                .body(Body::from(request_body.to_string()))
                .expect("build replay request"),
        )
        .await
        .expect("durable replay response");
    let replay_body = axum::body::to_bytes(replay.into_body(), usize::MAX)
        .await
        .expect("read replay receipt");
    let replay_receipt: crate::durable_ingest::DurableOperationReceipt =
        serde_json::from_slice(&replay_body).expect("decode replay receipt");
    assert_eq!(first_receipt.operation_id, replay_receipt.operation_id);

    let status = router(state)
        .oneshot(
            Request::get(format!("/api/operations/{}", first_receipt.operation_id))
                .body(Body::empty())
                .expect("build status request"),
        )
        .await
        .expect("operation status response");
    assert_eq!(status.status(), StatusCode::OK);
}

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
