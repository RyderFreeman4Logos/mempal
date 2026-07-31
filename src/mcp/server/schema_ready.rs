use anyhow::Context;

use super::{MempalMcpServer, SQLITE_WRITER_LEASE_NAME, daemon_ingest_ipc_available_for_db};
use crate::core::db::{
    CURRENT_FORK_EXT_VERSION, CURRENT_SCHEMA_VERSION, Database, read_fork_ext_version,
};

impl MempalMcpServer {
    pub(super) async fn ensure_schema_ready(&self) -> anyhow::Result<()> {
        let db_path = self.db_path.clone();
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            if db_path.exists() {
                let db = Database::open_query_only(&db_path).with_context(|| {
                    format!(
                        "failed to inspect MCP database schema at {}",
                        db_path.display()
                    )
                })?;
                let schema_version = db.schema_version()?;
                let fork_ext_version = read_fork_ext_version(db.conn())?;
                if schema_version == CURRENT_SCHEMA_VERSION
                    && fork_ext_version == CURRENT_FORK_EXT_VERSION
                {
                    return Ok(());
                }

                let lease_table_exists = db.conn().query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'runtime_writer_leases')",
                    [],
                    |row| row.get::<_, bool>(0),
                )?;
                let live_daemon_writer = daemon_ingest_ipc_available_for_db(&db_path)
                    || (lease_table_exists
                        && db.runtime_writer_lease_has_live_daemon(SQLITE_WRITER_LEASE_NAME)?);
                if live_daemon_writer {
                    anyhow::bail!(
                        "database schema is not ready for MCP while a live daemon writer is active: user_version={schema_version} (required {CURRENT_SCHEMA_VERSION}), fork_ext_version={fork_ext_version} (required {CURRENT_FORK_EXT_VERSION}); restart the daemon with this mempal binary before starting MCP"
                    );
                }
            }

            let db = Database::open(&db_path).with_context(|| {
                format!("failed to migrate MCP database at {}", db_path.display())
            })?;
            let schema_version = db.schema_version()?;
            let fork_ext_version = read_fork_ext_version(db.conn())?;
            anyhow::ensure!(
                schema_version == CURRENT_SCHEMA_VERSION
                    && fork_ext_version == CURRENT_FORK_EXT_VERSION,
                "MCP database migration did not reach the required schema: user_version={schema_version} (required {CURRENT_SCHEMA_VERSION}), fork_ext_version={fork_ext_version} (required {CURRENT_FORK_EXT_VERSION})"
            );
            Ok(())
        })
        .await
        .context("blocking MCP schema preparation task failed")??;
        Ok(())
    }
}
