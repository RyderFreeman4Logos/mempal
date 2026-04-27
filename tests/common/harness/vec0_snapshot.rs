//! Snapshot and restore helpers for the `drawer_vectors` vec0 virtual table.

use rusqlite::Connection;
use rusqlite::types::ValueRef;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vec0Row {
    pub drawer_id: String,
    pub dim: usize,
    pub raw_blob: Vec<u8>,
}

pub fn dump(conn: &Connection) -> rusqlite::Result<Vec<Vec0Row>> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'drawer_vectors'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(Vec::new());
    }

    let mut stmt = match conn.prepare("SELECT rowid, embedding FROM drawer_vectors ORDER BY rowid")
    {
        Ok(stmt) => stmt,
        Err(error) if error.to_string().contains("no such column: rowid") => {
            conn.prepare("SELECT id, embedding FROM drawer_vectors ORDER BY id")?
        }
        Err(error) => return Err(error),
    };
    stmt.query_map([], |row| {
        let drawer_id = match row.get_ref(0)? {
            ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned(),
            ValueRef::Integer(value) => value.to_string(),
            ValueRef::Real(value) => value.to_string(),
            ValueRef::Blob(blob) => String::from_utf8_lossy(blob).into_owned(),
            ValueRef::Null => String::new(),
        };
        let raw_blob: Vec<u8> = row.get(1)?;
        Ok(Vec0Row {
            drawer_id,
            dim: raw_blob.len() / std::mem::size_of::<f32>(),
            raw_blob,
        })
    })?
    .collect()
}

pub fn restore(conn: &Connection, snapshot: &[Vec0Row]) -> rusqlite::Result<()> {
    if snapshot.is_empty() {
        return Ok(());
    }

    let dim = snapshot[0].dim;
    assert!(
        snapshot.iter().all(|row| row.dim == dim),
        "vec0 snapshot dimensions must match"
    );

    conn.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS drawer_vectors USING vec0(id TEXT PRIMARY KEY, embedding FLOAT[{dim}]);"
    ))?;
    let mut stmt =
        match conn.prepare("INSERT INTO drawer_vectors (rowid, embedding) VALUES (?1, ?2)") {
            Ok(stmt) => stmt,
            Err(error)
                if error
                    .to_string()
                    .contains("table drawer_vectors has no column named rowid") =>
            {
                conn.prepare("INSERT INTO drawer_vectors (id, embedding) VALUES (?1, ?2)")?
            }
            Err(error) => return Err(error),
        };
    for row in snapshot {
        stmt.execute(rusqlite::params![row.drawer_id, row.raw_blob])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use mempal::core::db::Database;
    use mempal::core::types::{Drawer, SourceType};
    use tempfile::TempDir;

    #[test]
    fn smoke_round_trips_real_vec0_rows() {
        let tmp = TempDir::new().expect("tempdir");
        let source = Database::open(&tmp.path().join("source.db")).expect("open source db");
        let target = Database::open(&tmp.path().join("target.db")).expect("open target db");
        let drawer = Drawer {
            id: "drawer-1".to_string(),
            content: "content".to_string(),
            wing: "wing".to_string(),
            room: Some("room".to_string()),
            source_file: Some("source.txt".to_string()),
            source_type: SourceType::Manual,
            added_at: "2026-04-21T00:00:00Z".to_string(),
            chunk_index: Some(0),
            importance: 3,
            ..Drawer::default()
        };
        source.insert_drawer(&drawer).expect("insert drawer");
        source
            .insert_vector(&drawer.id, &[0.1, 0.2, 0.3, 0.4])
            .expect("insert vector");

        let snapshot = dump(source.conn()).expect("dump vec0");
        restore(target.conn(), &snapshot).expect("restore vec0");
        let restored = dump(target.conn()).expect("dump restored vec0");

        assert_eq!(restored, snapshot);
    }
}
