use mempal::core::db::Database;
use tempfile::TempDir;

struct TestDb {
    _dir: TempDir,
    inner: Database,
}

impl TestDb {
    fn conn(&self) -> &rusqlite::Connection {
        self.inner.conn()
    }
}

fn open_temp_db_at_fork_ext(_version: u32) -> TestDb {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("palace.db");
    let db = Database::open(&path).expect("open db");
    TestDb {
        _dir: dir,
        inner: db,
    }
}

#[test]
fn fork_ext_v16_creates_conversation_tables() {
    let db = open_temp_db_at_fork_ext(16);
    let tables: Vec<String> = db
        .conn()
        .prepare("SELECT name FROM sqlite_master WHERE type='table' AND name LIKE 'conversation%'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(tables.contains(&"conversation_turns".to_string()));
    assert!(tables.contains(&"conversation_turn_vectors".to_string()));
}

#[test]
fn fork_ext_v16_unique_constraint() {
    let db = open_temp_db_at_fork_ext(16);
    db.conn()
        .execute(
            "INSERT INTO conversation_turns VALUES ('id1','sess1','cc',0,'user','hello',1.0,5,NULL,NULL,0,'human')",
            [],
        )
        .unwrap();
    // Same (session_id, tool, turn_index) must fail
    let result = db.conn().execute(
        "INSERT INTO conversation_turns VALUES ('id2','sess1','cc',0,'user','world',1.0,5,NULL,NULL,0,'human')",
        [],
    );
    assert!(result.is_err());
}
