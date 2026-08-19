use crate::core::async_db::AsyncDb;
use crate::core::db::{DbError, db_error_is_sqlite_lock};
use crate::core::sqlite_retry::retry_content_mutation_sqlite_lock_async;

pub(super) async fn run(async_db: AsyncDb, drawer_id: String) -> Result<bool, DbError> {
    retry_content_mutation_sqlite_lock_async(
        move |retry_deadline| {
            let async_db = async_db.clone();
            let drawer_id = drawer_id.clone();
            async move {
                async_db
                    .run_write_until(retry_deadline, move |db| delete_receipt(db, &drawer_id))
                    .await
            }
        },
        db_error_is_sqlite_lock,
    )
    .await
}

/// MCP delete receipt: true when the drawer is soft-deleted after the attempt
/// (whether flipped by this call or already soft-deleted by an earlier
/// operation), false only when the drawer does not exist as a row at all.
fn delete_receipt(db: &crate::core::db::Database, drawer_id: &str) -> Result<bool, DbError> {
    if db.soft_delete_drawer(drawer_id)? {
        return Ok(true);
    }
    db.drawer_is_soft_deleted(drawer_id)
}
