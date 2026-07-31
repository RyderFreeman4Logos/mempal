use std::time::Duration;

use tempfile::TempDir;

use super::setup_server;
use crate::core::async_db::{AsyncDb, QueryOnlyAsyncDb, RESOURCE_BOUNDED_READERS};
use crate::core::config::Config;
use crate::core::db::{
    CURRENT_FORK_EXT_VERSION, CURRENT_SCHEMA_VERSION, Database, SQLITE_CACHE_SIZE_KIB_DEFAULT,
    read_fork_ext_version, set_fork_ext_version,
};
use crate::core::db_admission::{DbHolderClass, ProfileDbAdmission};
use crate::mcp::MempalMcpServer;

#[tokio::test]
async fn test_schema_ready_gate_migrates_fresh_database() {
    let tempdir = TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tempdir.path().join("palace.db");
    let config = Config {
        db_path: db_path.display().to_string(),
        ..Config::default()
    };
    let server = MempalMcpServer::new(db_path.clone(), config).expect("create MCP server");

    server
        .ensure_schema_ready()
        .await
        .expect("prepare fresh MCP database");

    let db = Database::open_query_only(&db_path).expect("open prepared database");
    assert_eq!(
        db.schema_version().expect("schema version"),
        CURRENT_SCHEMA_VERSION
    );
    assert_eq!(
        read_fork_ext_version(db.conn()).expect("fork-ext version"),
        CURRENT_FORK_EXT_VERSION
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn test_schema_ready_gate_rejects_stale_schema_with_live_daemon_writer() {
    let tempdir = TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tempdir.path().join("palace.db");
    let db = Database::open(&db_path).expect("initialize database");
    let lease = db
        .runtime_writer_lease_acquire(
            "sqlite-writer",
            &crate::core::process_identity::current_daemon_owner(),
            "daemon",
            300,
            None,
        )
        .expect("acquire daemon writer lease")
        .expect("daemon writer lease available");
    set_fork_ext_version(db.conn(), CURRENT_FORK_EXT_VERSION - 1)
        .expect("downgrade fork-ext version");
    let config = Config {
        db_path: db_path.display().to_string(),
        ..Config::default()
    };
    let server = MempalMcpServer::new(db_path.clone(), config).expect("create MCP server");

    let error = server
        .ensure_schema_ready()
        .await
        .expect_err("live daemon must own schema migration");

    assert!(
        error.to_string().contains("live daemon writer"),
        "{error:#}"
    );
    assert_eq!(
        read_fork_ext_version(db.conn()).expect("fork-ext version after refusal"),
        CURRENT_FORK_EXT_VERSION - 1
    );
    assert!(
        db.runtime_writer_lease_release(&lease)
            .expect("release daemon writer lease")
    );
}

#[tokio::test]
async fn test_status_read_survives_writer_pool_open_failure() {
    let tempdir = TempDir::new_in("/tmp").expect("short tempdir");
    let db_path = tempdir.path().join("palace.db");
    drop(Database::open(&db_path).expect("initialize status database"));
    let config = Config {
        db_path: db_path.display().to_string(),
        ..Config::default()
    };
    let server = MempalMcpServer::new(db_path.clone(), config)
        .expect("create MCP server")
        .with_async_db_open_error_for_test(
            "failed to open MCP async database pool: database is locked",
        );

    let status = server
        .mempal_status()
        .await
        .expect("status should not open the writer-capable pool")
        .0;

    assert_eq!(status.schema_version, CURRENT_SCHEMA_VERSION);
    assert!(
        status.database_diagnostic.is_none(),
        "query-only status must not inherit writer-pool failure: {:?}",
        status.database_diagnostic
    );
    assert!(status.resource_usage.sqlite.async_pool_loaded);
    assert_eq!(
        status.resource_usage.sqlite.async_reader_connections,
        RESOURCE_BOUNDED_READERS
    );
    assert_eq!(status.resource_usage.sqlite.async_writer_connections, 0);
    assert_eq!(
        status.resource_usage.sqlite.async_total_connections,
        RESOURCE_BOUNDED_READERS
    );
    let admission = ProfileDbAdmission::snapshot(&db_path).expect("status holder snapshot");
    let writer_pool_cache_bytes =
        (RESOURCE_BOUNDED_READERS as u64 + 1) * SQLITE_CACHE_SIZE_KIB_DEFAULT.unsigned_abs() * 1024;
    assert!(
        admission.holders.iter().any(|holder| {
            holder.holder_class == DbHolderClass::Mcp
                && holder.connection_count == RESOURCE_BOUNDED_READERS
        }),
        "status must load the MCP query-only pool: {:?}",
        admission.holders
    );
    assert!(
        admission.holders.iter().all(|holder| {
            holder.holder_class != DbHolderClass::Mcp
                || holder.connection_count != RESOURCE_BOUNDED_READERS + 1
                || holder.configured_cache_bytes != writer_pool_cache_bytes
        }),
        "status must not load an MCP writer-capable pool: {:?}",
        admission.holders
    );
}

#[tokio::test]
async fn test_status_aggregates_writer_and_query_only_pool_resources() {
    let (_tempdir, db_path, server) = setup_server();
    let writer_pool =
        AsyncDb::open(&db_path, RESOURCE_BOUNDED_READERS).expect("open writer-capable pool");
    let reader_pool =
        QueryOnlyAsyncDb::open(&db_path, RESOURCE_BOUNDED_READERS).expect("open query-only pool");
    let server = server
        .with_async_db_for_test(writer_pool)
        .with_query_only_async_db_for_test(reader_pool);

    let sqlite = server
        .mempal_status()
        .await
        .expect("status")
        .0
        .resource_usage
        .sqlite;

    assert!(sqlite.async_pool_loaded);
    assert_eq!(
        sqlite.async_reader_connections,
        RESOURCE_BOUNDED_READERS * 2
    );
    assert_eq!(sqlite.async_writer_connections, 1);
    assert_eq!(
        sqlite.async_total_connections,
        RESOURCE_BOUNDED_READERS * 2 + 1
    );
    assert_eq!(
        sqlite.configured_page_cache_bytes,
        (RESOURCE_BOUNDED_READERS as u64 * 2 + 1)
            * SQLITE_CACHE_SIZE_KIB_DEFAULT.unsigned_abs()
            * 1024
    );
}

#[tokio::test]
async fn test_bounded_mcp_read_bypasses_writer_capable_async_db_open_failure() {
    let (_tempdir, _db_path, server) = setup_server();
    let server = server.with_async_db_open_error_for_test(
        "failed to open MCP async database pool: database is locked",
    );

    let query_only = server
        .run_read_anyhow_bounded(
            |db| {
                db.conn()
                    .query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))
                    .map_err(anyhow::Error::from)
            },
            Duration::from_secs(1),
        )
        .await
        .expect("bounded MCP reads should not open the writer-capable pool");

    assert_eq!(query_only, Some(1));
}
