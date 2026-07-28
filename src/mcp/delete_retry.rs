use crate::core::async_db::AsyncDb;
use crate::core::db::{DbError, db_error_is_sqlite_lock};
use crate::core::sqlite_retry::retry_content_mutation_sqlite_lock_async;

pub(super) async fn run(async_db: AsyncDb, drawer_id: String) -> Result<bool, DbError> {
    retry_content_mutation_sqlite_lock_async(
        move || {
            let async_db = async_db.clone();
            let drawer_id = drawer_id.clone();
            async move {
                async_db
                    .run_write(move |db| db.soft_delete_drawer(&drawer_id))
                    .await
            }
        },
        db_error_is_sqlite_lock,
    )
    .await
}
