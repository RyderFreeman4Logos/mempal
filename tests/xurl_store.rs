use mempal::core::db::Database;
use mempal::xurl::model::{Provenance, RawTurn, Role, Tool, TurnMetadata};
use mempal::xurl::store::{self, TurnFilter};
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

fn make_raw_turn(session_id: &str, turn_index: u32, role: Role, content: &str) -> RawTurn {
    RawTurn {
        session_id: session_id.to_string(),
        tool: Tool::Cc,
        role,
        content: content.to_string(),
        timestamp_epoch: 1_000.0 + f64::from(turn_index),
        project_path: None,
        git_branch: None,
        is_csa_delegated: false,
        provenance: Provenance::Human,
        turn_index,
        metadata: TurnMetadata::default(),
    }
}

fn make_hermes_turn(session_id: &str, message_id: &str, content: &str) -> RawTurn {
    let mut turn = make_raw_turn(session_id, 0, Role::Assistant, content);
    turn.tool = Tool::Hermes;
    turn.timestamp_epoch = 2_000.0;
    turn.project_path = Some("/repo/old".to_string());
    turn.metadata = TurnMetadata {
        hermes_profile: Some("default".to_string()),
        session_title: Some("Old title".to_string()),
        session_source: Some("old-source".to_string()),
        message_id: Some(message_id.to_string()),
        tool_name: Some("old-tool".to_string()),
        tool_call_id: Some("old-call".to_string()),
        previous_message_id: None,
        next_message_id: None,
    };
    turn
}

#[tokio::test]
async fn insert_turns_idempotent_same_content() {
    let db = open_temp_db_at_fork_ext(16);
    let turn = make_raw_turn("sess1", 0, Role::User, "Hello");
    store::insert_turns(db.conn(), std::slice::from_ref(&turn)).unwrap();
    store::insert_turns(db.conn(), std::slice::from_ref(&turn)).unwrap(); // second call — should skip
    let count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM conversation_turns", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[tokio::test]
async fn insert_turns_updates_changed_content() {
    let db = open_temp_db_at_fork_ext(16);
    let turn = make_raw_turn("sess1", 0, Role::User, "Hello");
    store::insert_turns(db.conn(), &[turn]).unwrap();
    let updated = make_raw_turn("sess1", 0, Role::User, "Hello v2");
    store::insert_turns(db.conn(), &[updated]).unwrap();
    let content: String = db
        .conn()
        .query_row(
            "SELECT content FROM conversation_turns WHERE session_id='sess1' AND turn_index=0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(content, "Hello v2");
}

#[test]
fn insert_turns_updates_changed_metadata_without_deleting_vectors() {
    let db = open_temp_db_at_fork_ext(16);
    let turn = make_hermes_turn("sess1", "msg1", "Hello");
    store::insert_turns(db.conn(), std::slice::from_ref(&turn)).unwrap();
    let turn_id = store::turn_id_for(&turn);
    db.conn()
        .execute(
            "INSERT INTO conversation_turn_vectors (turn_id, chunk_index, vector) VALUES (?1, 0, ?2)",
            rusqlite::params![&turn_id, vec![1_u8, 2, 3, 4]],
        )
        .unwrap();

    let mut updated = make_hermes_turn("sess1", "msg1", "Hello");
    updated.timestamp_epoch = 3_000.0;
    updated.project_path = Some("/repo/new".to_string());
    updated.metadata.session_title = Some("New title".to_string());
    updated.metadata.session_source = Some("new-source".to_string());
    updated.metadata.tool_name = Some("new-tool".to_string());
    updated.metadata.previous_message_id = Some("msg0".to_string());
    updated.metadata.next_message_id = Some("msg2".to_string());

    let stats = store::insert_turns(db.conn(), std::slice::from_ref(&updated)).unwrap();

    assert_eq!(stats.inserted, 0);
    assert_eq!(stats.updated, 1);
    assert_eq!(stats.skipped, 0);
    let stored = store::get_turns(
        db.conn(),
        TurnFilter {
            tool: Some(Tool::Hermes),
            session_id: Some("sess1".to_string()),
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0].id, turn_id);
    assert_eq!(stored[0].content, "Hello");
    assert_eq!(stored[0].timestamp_epoch, 3_000.0);
    assert_eq!(stored[0].project_path.as_deref(), Some("/repo/new"));
    assert_eq!(
        stored[0].metadata.session_title.as_deref(),
        Some("New title")
    );
    assert_eq!(
        stored[0].metadata.session_source.as_deref(),
        Some("new-source")
    );
    assert_eq!(stored[0].metadata.tool_name.as_deref(), Some("new-tool"));
    assert_eq!(
        stored[0].metadata.previous_message_id.as_deref(),
        Some("msg0")
    );
    assert_eq!(stored[0].metadata.next_message_id.as_deref(), Some("msg2"));

    let vector_count: i64 = db
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM conversation_turn_vectors WHERE turn_id = ?1",
            rusqlite::params![&turn_id],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(vector_count, 1);
}

#[test]
fn insert_turns_returns_correct_stats() {
    let db = open_temp_db_at_fork_ext(16);

    // Insert two new turns
    let turns = vec![
        make_raw_turn("sess1", 0, Role::User, "Hello"),
        make_raw_turn("sess1", 1, Role::Assistant, "World"),
    ];
    let stats = store::insert_turns(db.conn(), &turns).unwrap();
    assert_eq!(stats.inserted, 2);
    assert_eq!(stats.skipped, 0);
    assert_eq!(stats.updated, 0);

    // Re-insert same turns — should be skipped
    let turns2 = vec![
        make_raw_turn("sess1", 0, Role::User, "Hello"),
        make_raw_turn("sess1", 1, Role::Assistant, "World"),
    ];
    let stats2 = store::insert_turns(db.conn(), &turns2).unwrap();
    assert_eq!(stats2.inserted, 0);
    assert_eq!(stats2.skipped, 2);
    assert_eq!(stats2.updated, 0);

    // Insert one changed, one new
    let turns3 = vec![
        make_raw_turn("sess1", 0, Role::User, "Hello changed"),
        make_raw_turn("sess1", 2, Role::User, "Brand new"),
    ];
    let stats3 = store::insert_turns(db.conn(), &turns3).unwrap();
    assert_eq!(stats3.updated, 1);
    assert_eq!(stats3.inserted, 1);
    assert_eq!(stats3.skipped, 0);
}

#[test]
fn get_turns_ordered_newest_first() {
    let db = open_temp_db_at_fork_ext(16);
    let mut t0 = make_raw_turn("sess1", 0, Role::User, "first");
    t0.timestamp_epoch = 1000.0;
    let mut t1 = make_raw_turn("sess1", 1, Role::User, "second");
    t1.timestamp_epoch = 2000.0;
    let mut t2 = make_raw_turn("sess1", 2, Role::User, "third");
    t2.timestamp_epoch = 3000.0;
    store::insert_turns(db.conn(), &[t0, t1, t2]).unwrap();

    let turns = store::get_turns(
        db.conn(),
        TurnFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(turns[0].timestamp_epoch, 3000.0);
    assert_eq!(turns[2].timestamp_epoch, 1000.0);
}

#[test]
fn get_stats_shows_per_tool_counts() {
    let db = open_temp_db_at_fork_ext(16);

    let mut cc_turn = make_raw_turn("s1", 0, Role::User, "hi");
    cc_turn.tool = Tool::Cc;
    cc_turn.timestamp_epoch = 1.0;

    let mut codex_turn0 = make_raw_turn("s2", 0, Role::User, "hi");
    codex_turn0.tool = Tool::Codex;
    codex_turn0.timestamp_epoch = 2.0;

    let mut codex_turn1 = make_raw_turn("s2", 1, Role::Assistant, "bye");
    codex_turn1.tool = Tool::Codex;
    codex_turn1.timestamp_epoch = 3.0;

    store::insert_turns(db.conn(), &[cc_turn]).unwrap();
    store::insert_turns(db.conn(), &[codex_turn0, codex_turn1]).unwrap();

    let stats = store::get_stats(db.conn()).unwrap();
    let cc_stat = stats.iter().find(|s| s.tool == "cc").unwrap();
    let codex_stat = stats.iter().find(|s| s.tool == "codex").unwrap();
    assert_eq!(cc_stat.count, 1);
    assert_eq!(codex_stat.count, 2);
}

#[test]
fn get_turns_tool_filter() {
    let db = open_temp_db_at_fork_ext(16);

    let mut cc = make_raw_turn("s1", 0, Role::User, "cc turn");
    cc.tool = Tool::Cc;
    let mut codex = make_raw_turn("s2", 0, Role::User, "codex turn");
    codex.tool = Tool::Codex;

    store::insert_turns(db.conn(), &[cc, codex]).unwrap();

    let turns = store::get_turns(
        db.conn(),
        TurnFilter {
            tool: Some(Tool::Cc),
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].content, "cc turn");
}
