//! Profile-admitted raw SQLite connection boundary.
//!
//! Most callers should use [`super::db::Database`]. Narrow infrastructure
//! paths that cannot run migrations on every open use this RAII wrapper so a
//! raw connection cannot outlive its profile-wide admission record.

use std::path::Path;

use rusqlite::Connection;

use super::db::DbError;
use super::db_admission::{DbAdmissionRequest, DbHolderClass, ProfileDbAdmission};

/// One raw SQLite connection coupled to its profile-wide admission guard.
pub(crate) struct AdmittedSqliteConnection {
    // Declaration order is intentional: close SQLite before releasing admission.
    connection: Connection,
    _admission: ProfileDbAdmission,
}

impl AdmittedSqliteConnection {
    /// Open one connection using the standard per-connection profile budget.
    pub(crate) fn open_default(path: &Path) -> Result<Self, DbError> {
        Self::open(
            path,
            DbHolderClass::current_process(),
            super::db::SQLITE_CACHE_SIZE_KIB_DEFAULT,
        )
    }

    pub(crate) fn open(
        path: &Path,
        holder_class: DbHolderClass,
        cache_size_kib: i64,
    ) -> Result<Self, DbError> {
        let admission = ProfileDbAdmission::acquire(
            path,
            DbAdmissionRequest::new(
                holder_class,
                1,
                cache_size_kib.unsigned_abs().saturating_mul(1024),
            ),
        )?;
        let connection = Connection::open(path)?;
        connection.pragma_update(None, "cache_size", cache_size_kib)?;
        Ok(Self {
            connection,
            _admission: admission,
        })
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }
}

#[cfg(test)]
#[path = "db_connection_tests.rs"]
mod tests;
