use std::fmt;

use anyhow::{Context, Result};

use crate::core::{
    db::{Database, DbError},
    types::RuntimeWriterLease,
};

#[derive(Debug)]
struct DaemonWriteError(anyhow::Error);

impl fmt::Display for DaemonWriteError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl std::error::Error for DaemonWriteError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

impl From<DbError> for DaemonWriteError {
    fn from(error: DbError) -> Self {
        Self(error.into())
    }
}

pub(super) fn with_daemon_runtime_writer_lease_write<T>(
    db: &Database,
    lease: Option<&RuntimeWriterLease>,
    operation: &'static str,
    mut write: impl FnMut() -> Result<T>,
) -> Result<T> {
    db.with_runtime_writer_lease_write_retry(lease, operation, || write().map_err(DaemonWriteError))
        .map_err(|error| error.0)
        .with_context(|| format!("daemon writer mutation failed during {operation}"))
}

#[cfg(test)]
mod tests {
    use crate::core::{
        db::Database,
        types::{BootstrapEvidenceArgs, Drawer, SourceType},
    };

    #[test]
    fn leased_automatic_hook_insert_consumes_reserve_and_retries_after_sqlite_full() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db_path = temp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open database");
        let lease = db
            .runtime_writer_lease_acquire(
                super::super::SQLITE_WRITER_LEASE_NAME,
                "daemon-enospc-test",
                "daemon",
                300,
                None,
            )
            .expect("acquire daemon writer lease")
            .expect("daemon writer lease available");
        let reserve_path = db_path.with_file_name(".palace.db.write-reserve");
        assert!(
            reserve_path.exists(),
            "write reserve must exist before SQLITE_FULL"
        );
        let drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
            id: "leased-hook-drawer".to_string(),
            content: "durable automatic hook drawer".to_string(),
            wing: "test-wing".to_string(),
            room: Some("test-room".to_string()),
            source_file: Some("hook.json".to_string()),
            source_type: SourceType::SystemGenerated,
            added_at: "2026-08-28T00:00:00Z".to_string(),
            chunk_index: Some(0),
            importance: 0,
        });
        let mut attempts = 0;

        super::with_daemon_runtime_writer_lease_write(
            &db,
            Some(&lease),
            "insert daemon hook drawer before model work",
            || {
                attempts += 1;
                if attempts == 1 {
                    return Err(anyhow::Error::new(rusqlite::Error::SqliteFailure(
                        rusqlite::ffi::Error {
                            code: rusqlite::ErrorCode::DiskFull,
                            extended_code: rusqlite::ffi::SQLITE_FULL,
                        },
                        Some("database or disk is full".to_string()),
                    )));
                }
                db.insert_drawer_with_project(&drawer, None)
                    .map_err(anyhow::Error::from)
            },
        )
        .expect("leased automatic hook insert must retry after SQLITE_FULL");

        assert_eq!(attempts, 2, "SQLITE_FULL must retry exactly once");
        assert!(
            !reserve_path.exists(),
            "SQLITE_FULL must consume the write reserve"
        );
        assert!(
            db.drawer_exists(&drawer.id)
                .expect("read committed automatic hook drawer"),
            "retry must commit the automatic hook drawer"
        );
    }
}
