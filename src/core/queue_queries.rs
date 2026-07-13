use rusqlite::{Connection, OptionalExtension};

use super::queue::{PendingOperationRecord, QueueError, Result};

pub(super) fn operation_status_from_completion(
    conn: &Connection,
    id: &str,
) -> Result<Option<PendingOperationRecord>> {
    conn.query_row(
        r#"
            SELECT message_id,
                   kind,
                   created_at,
                   claimed_at,
                   completed_at,
                   op_state,
                   result_drawer_id,
                   rejected_reason,
                   failure_detail,
                   result_json
            FROM pending_message_completions
            WHERE message_id = ?1
            "#,
        [id],
        |row| {
            Ok(PendingOperationRecord {
                id: row.get(0)?,
                kind: row.get(1)?,
                created_at: row.get(2)?,
                claimed_at: row.get(3)?,
                completed_at: row.get(4)?,
                op_state: row.get(5)?,
                result_drawer_id: row.get(6)?,
                rejected_reason: row.get(7)?,
                failure_detail: row.get(8)?,
                result_json: row.get(9)?,
            })
        },
    )
    .optional()
    .map_err(QueueError::from)
}
