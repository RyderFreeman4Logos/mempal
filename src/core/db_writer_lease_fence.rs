//! Atomic generation fencing for runtime writer mutations.

use rusqlite::{OptionalExtension, params};

use super::{
    Database, DbError, NoveltyAuditInsert, anchor, anchor_kind_as_str, content_hash_hex,
    encode_json, encode_optional_json, knowledge_status_as_str, knowledge_tier_as_str,
    memory_domain_as_str, memory_kind_as_str, provenance_as_str, source_type_as_str,
};
use crate::core::types::{Drawer, RuntimeWriterLease};

/// One atomic drawer merge plus its novelty audit record.
pub struct DrawerMergeWithNovelty<'a> {
    pub drawer_id: &'a str,
    pub merged_content: &'a str,
    pub updated_at: &'a str,
    pub vector: &'a [f32],
    pub expected_merge_count: u32,
    pub audit: NoveltyAuditInsert<'a>,
}

/// Session-ingest importance update applied as one atomic batch.
pub struct IngestBoostBatch<'a> {
    pub drawer_ids: &'a [String],
    pub now_ms: i64,
    pub boost_per_access: f64,
    pub boost_cap: f64,
    pub decay_rate: f64,
    pub floor: f64,
}

impl Database {
    /// Record a gating audit under the same write lock that validates `lease`.
    pub fn record_gating_audit_fenced(
        &self,
        lease: Option<&RuntimeWriterLease>,
        candidate_hash: &str,
        decision: &crate::ingest::gating::GatingDecision,
        project_id: Option<&str>,
        content: Option<&str>,
        operation: &'static str,
    ) -> Result<(), DbError> {
        let Some(lease) = lease else {
            return self.record_gating_audit(candidate_hash, decision, project_id, content);
        };
        self.with_runtime_writer_lease_write(Some(lease), operation, || {
            self.record_gating_audit_in_current_transaction(
                candidate_hash,
                decision,
                project_id,
                content,
            )
        })
    }

    pub fn upsert_drawer_and_replace_vector_fenced(
        &self,
        lease: Option<&RuntimeWriterLease>,
        drawer: &Drawer,
        vector: &[f32],
    ) -> Result<(), DbError> {
        anchor::validate_anchor_domain(&drawer.domain, &drawer.anchor_kind)
            .map_err(|message| DbError::InvalidDrawerMetadata(message.to_string()))?;
        self.with_runtime_writer_lease_transaction(
            lease,
            "upsert drawer and replace vector",
            || {
                self.ensure_vectors_table(vector.len())?;

                let existing = self
                    .conn
                    .query_row(
                        "SELECT 1 FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
                        [drawer.id.as_str()],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()?;

                if existing.is_none() {
                    self.insert_drawer(drawer)?;
                    return self.insert_vector(&drawer.id, vector);
                }

                let vector_json = serde_json::to_string(vector)?;
                let content_hash = content_hash_hex(&drawer.content);
                self.conn.execute(
                    r#"
                    UPDATE drawers
                    SET content = ?2,
                        wing = ?3,
                        room = ?4,
                        source_file = ?5,
                        source_type = ?6,
                        confidence = ?7,
                        added_at = ?8,
                        chunk_index = ?9,
                        normalize_version = ?10,
                        importance = ?11,
                        memory_kind = ?12,
                        domain = ?13,
                        field = ?14,
                        anchor_kind = ?15,
                        anchor_id = ?16,
                        parent_anchor_id = ?17,
                        provenance = ?18,
                        statement = ?19,
                        tier = ?20,
                        status = ?21,
                        supporting_refs = ?22,
                        counterexample_refs = ?23,
                        teaching_refs = ?24,
                        verification_refs = ?25,
                        scope_constraints = ?26,
                        trigger_hints = ?27,
                        is_pinned = ?28,
                        pin_order = ?29,
                        supersedes = ?30,
                        content_hash = ?31,
                        valid_from = ?32,
                        valid_until = NULL
                    WHERE id = ?1 AND deleted_at IS NULL
                    "#,
                    params![
                        drawer.id.as_str(),
                        drawer.content.as_str(),
                        drawer.wing.as_str(),
                        drawer.room.as_deref(),
                        drawer.source_file.as_deref(),
                        source_type_as_str(&drawer.source_type),
                        drawer.confidence,
                        drawer.added_at.as_str(),
                        drawer.chunk_index,
                        i64::from(drawer.normalize_version),
                        drawer.importance,
                        memory_kind_as_str(&drawer.memory_kind),
                        memory_domain_as_str(&drawer.domain),
                        drawer.field.as_str(),
                        anchor_kind_as_str(&drawer.anchor_kind),
                        drawer.anchor_id.as_str(),
                        drawer.parent_anchor_id.as_deref(),
                        drawer.provenance.as_ref().map(provenance_as_str),
                        drawer.statement.as_deref(),
                        drawer.tier.as_ref().map(knowledge_tier_as_str),
                        drawer.status.as_ref().map(knowledge_status_as_str),
                        encode_json(&drawer.supporting_refs)?,
                        encode_json(&drawer.counterexample_refs)?,
                        encode_json(&drawer.teaching_refs)?,
                        encode_json(&drawer.verification_refs)?,
                        drawer.scope_constraints.as_deref(),
                        encode_optional_json(drawer.trigger_hints.as_ref())?,
                        drawer.is_pinned,
                        drawer.pin_order,
                        drawer.supersedes.as_deref(),
                        content_hash,
                        drawer.added_at.as_str(),
                    ],
                )?;

                self.conn.execute(
                    "DELETE FROM drawer_vectors WHERE id = ?1",
                    [drawer.id.as_str()],
                )?;
                self.conn.execute(
                    "INSERT INTO drawer_vectors (id, embedding) VALUES (?1, vec_f32(?2))",
                    params![drawer.id.as_str(), vector_json.as_str()],
                )?;
                self.record_current_vector_metadata(&drawer.id, vector.len())?;

                Ok(())
            },
        )
    }

    /// Verify the lease and execute one mutation under the same write lock.
    ///
    /// A preflight call to `runtime_writer_lease_is_active` cannot fence a
    /// later mutation: another holder may take over between the check and the
    /// write. This boundary acquires `BEGIN IMMEDIATE` first and retains it
    /// through the mutation, so generation takeover and the write serialize.
    pub fn with_runtime_writer_lease_write<T, E>(
        &self,
        lease: Option<&RuntimeWriterLease>,
        operation: &'static str,
        write: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<DbError>,
    {
        let Some(lease) = lease else {
            return write();
        };

        self.with_runtime_writer_lease_transaction(Some(lease), operation, write)
    }

    pub(super) fn with_runtime_writer_lease_transaction<T, E>(
        &self,
        lease: Option<&RuntimeWriterLease>,
        operation: &'static str,
        write: impl FnOnce() -> Result<T, E>,
    ) -> Result<T, E>
    where
        E: From<DbError>,
    {
        self.conn
            .execute_batch("BEGIN IMMEDIATE")
            .map_err(DbError::from)
            .map_err(E::from)?;
        let result = (|| {
            if let Some(lease) = lease {
                self.runtime_writer_lease_cleanup_expired_tx(true)
                    .map_err(E::from)?;
                self.require_runtime_writer_lease_tx(lease, operation)
                    .map_err(E::from)?;
            }
            write()
        })();
        match result {
            Ok(value) => match self.conn.execute_batch("COMMIT") {
                Ok(()) => Ok(value),
                Err(commit_err) => {
                    // ROLLBACK on COMMIT failure so the connection does not
                    // return to the pool with an active transaction. Return
                    // the original COMMIT error.
                    let _ = self.conn.execute_batch("ROLLBACK");
                    Err(E::from(DbError::from(commit_err)))
                }
            },
            Err(error) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    pub(super) fn require_runtime_writer_lease_tx(
        &self,
        lease: &RuntimeWriterLease,
        operation: &'static str,
    ) -> Result<(), DbError> {
        let active = self.conn.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM runtime_writer_leases
                 WHERE name = ?1 AND owner = ?2 AND session_id = ?3 AND generation = ?4
             )",
            params![
                lease.name,
                lease.owner,
                lease.session_id,
                lease.generation as i64
            ],
            |row| row.get::<_, i64>(0),
        )?;
        if active != 0 {
            return Ok(());
        }
        Err(DbError::RuntimeWriterLeaseLost {
            lease_name: lease.name.clone(),
            owner: lease.owner.clone(),
            generation: lease.generation,
            operation,
        })
    }
}

#[cfg(test)]
#[path = "db_writer_lease_fence_tests.rs"]
mod tests;
