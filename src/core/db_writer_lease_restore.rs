//! Crash-safe restoration of a fenced runtime writer lease.

use super::*;

impl Database {
    /// Restore a previously-owned lease only when its process is live, its
    /// generation is still current, and no holder owns the name.
    pub fn runtime_writer_lease_restore_if_unheld(
        &self,
        lease: &RuntimeWriterLease,
        ttl_secs: u64,
    ) -> Result<bool, DbError> {
        let mut restored = false;
        self.with_immediate_tx(|| {
            self.runtime_writer_lease_cleanup_expired_tx(true)?;
            if !runtime_writer_lease_holder_is_live(
                &lease.owner,
                lease.pid,
                lease.boot_id.as_deref(),
                &lease.mode,
            ) {
                return Ok(());
            }
            let last_generation = self
                .conn
                .query_row(
                    "SELECT last_generation FROM runtime_writer_lease_generations WHERE name = ?1",
                    params![&lease.name],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?;
            if last_generation.and_then(|value| u64::try_from(value).ok())
                != Some(lease.generation)
            {
                return Ok(());
            }
            let same_lease_exists = self.conn.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM runtime_writer_leases
                     WHERE name = ?1 AND owner = ?2 AND session_id = ?3 AND generation = ?4
                 )",
                params![
                    &lease.name,
                    &lease.owner,
                    &lease.session_id,
                    lease.generation as i64
                ],
                |row| row.get::<_, i64>(0),
            )?;
            if same_lease_exists != 0 {
                restored = true;
                return Ok(());
            }
            let holder_count = self.conn.query_row(
                "SELECT COUNT(*) FROM runtime_writer_leases WHERE name = ?1",
                params![&lease.name],
                |row| row.get::<_, i64>(0),
            )?;
            if holder_count != 0 {
                return Ok(());
            }
            let now_time = SystemTime::now();
            let now = crate::cowork::peek::format_rfc3339(now_time);
            let expires_at = crate::cowork::peek::format_rfc3339(
                now_time + Duration::from_secs(ttl_secs),
            );
            let rows = self.conn.execute(
                "INSERT OR IGNORE INTO runtime_writer_leases \
                 (name, owner, generation, pid, boot_id, session_id, acquired_at, expires_at, heartbeat_at, mode, metadata_json) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?7, ?9, ?10)",
                params![
                    &lease.name,
                    &lease.owner,
                    lease.generation as i64,
                    lease.pid as i64,
                    &lease.boot_id,
                    &lease.session_id,
                    &now,
                    &expires_at,
                    &lease.mode,
                    &lease.metadata_json,
                ],
            )?;
            restored = rows > 0;
            Ok(())
        })?;
        Ok(restored)
    }
}
