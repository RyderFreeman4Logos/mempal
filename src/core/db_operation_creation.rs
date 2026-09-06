use rusqlite::params;

use super::db::{Database, DbError};
use super::types::Drawer;

impl Database {
    pub(crate) fn insert_drawer_with_project_validity_and_operation(
        &self,
        drawer: &Drawer,
        project_id: Option<&str>,
        source_root: Option<&str>,
        valid_from: Option<&str>,
        valid_until: Option<&str>,
        creation_operation_id: Option<&str>,
    ) -> Result<(), DbError> {
        self.with_write_reserve_retry("insert drawer", || {
            self.insert_drawer_with_project_validity_once(
                drawer,
                project_id,
                source_root,
                valid_from,
                valid_until,
                creation_operation_id,
            )
        })
    }

    pub(crate) fn drawer_ids_created_by_operation(
        &self,
        operation_id: &str,
        drawer_ids: &[String],
    ) -> Result<Vec<String>, DbError> {
        let mut created = Vec::new();
        for drawer_id in drawer_ids {
            let owned = self.conn().query_row(
                "SELECT EXISTS(SELECT 1 FROM drawers WHERE id = ?1 AND creation_operation_id = ?2)",
                params![drawer_id, operation_id],
                |row| row.get::<_, bool>(0),
            )?;
            if owned {
                created.push(drawer_id.clone());
            }
        }
        Ok(created)
    }
}
