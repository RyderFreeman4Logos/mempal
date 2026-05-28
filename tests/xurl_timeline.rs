use mempal::core::db::Database;
use mempal::xurl::model::{Provenance, RawTurn, Role, Tool};
use mempal::xurl::store::{self, TurnFilter};
use rusqlite::params;
use tempfile::TempDir;

// ── test DB helper ────────────────────────────────────────────────────────────

struct TestDb {
    _dir: TempDir,
    inner: Database,
}

impl TestDb {
    fn conn(&self) -> &rusqlite::Connection {
        self.inner.conn()
    }
}

fn open_temp_db() -> TestDb {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("palace.db");
    let db = Database::open(&path).expect("open db");
    TestDb {
        _dir: dir,
        inner: db,
    }
}

// ── fixture helpers ───────────────────────────────────────────────────────────

struct RawTurnRow<'a> {
    id: &'a str,
    session_id: &'a str,
    tool: &'a str,
    turn_index: i64,
    role: &'a str,
    content: &'a str,
    timestamp_epoch: f64,
}

/// Insert a raw turn row directly into conversation_turns (bypasses store logic).
fn insert_raw_turn(db: &TestDb, row: RawTurnRow<'_>) {
    db.conn()
        .execute(
            "INSERT INTO conversation_turns \
             (id, session_id, tool, turn_index, role, content, timestamp_epoch, \
              token_count, project_path, git_branch, is_csa_delegated, provenance) \
             VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,NULL,NULL,0,'human')",
            params![
                row.id,
                row.session_id,
                row.tool,
                row.turn_index,
                row.role,
                row.content,
                row.timestamp_epoch
            ],
        )
        .expect("insert raw turn");
}

fn make_raw_turn(
    session_id: &str,
    tool: Tool,
    turn_index: u32,
    role: Role,
    content: &str,
    ts: f64,
) -> RawTurn {
    RawTurn {
        session_id: session_id.to_string(),
        tool,
        role,
        content: content.to_string(),
        timestamp_epoch: ts,
        project_path: None,
        git_branch: None,
        is_csa_delegated: false,
        provenance: Provenance::Human,
        turn_index,
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[test]
fn timeline_returns_newest_first() {
    let db = open_temp_db();
    insert_raw_turn(
        &db,
        RawTurnRow {
            id: "t1",
            session_id: "sess1",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "first",
            timestamp_epoch: 1000.0,
        },
    );
    insert_raw_turn(
        &db,
        RawTurnRow {
            id: "t2",
            session_id: "sess1",
            tool: "cc",
            turn_index: 1,
            role: "user",
            content: "second",
            timestamp_epoch: 2000.0,
        },
    );
    insert_raw_turn(
        &db,
        RawTurnRow {
            id: "t3",
            session_id: "sess1",
            tool: "cc",
            turn_index: 2,
            role: "user",
            content: "third",
            timestamp_epoch: 3000.0,
        },
    );

    let turns = store::get_turns(
        db.conn(),
        TurnFilter {
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(turns.len(), 3);
    assert_eq!(turns[0].timestamp_epoch, 3000.0, "newest should be first");
    assert_eq!(turns[1].timestamp_epoch, 2000.0);
    assert_eq!(turns[2].timestamp_epoch, 1000.0, "oldest should be last");
}

#[test]
fn timeline_pagination_works() {
    let db = open_temp_db();
    for i in 0..10i64 {
        let id = format!("t{i}");
        let content = format!("msg {i}");
        insert_raw_turn(
            &db,
            RawTurnRow {
                id: &id,
                session_id: "sess1",
                tool: "cc",
                turn_index: i,
                role: "user",
                content: &content,
                timestamp_epoch: i as f64 * 100.0,
            },
        );
    }

    // Page 0: first 3 items (newest first: ts=900,800,700)
    let page0 = store::get_turns(
        db.conn(),
        TurnFilter {
            limit: 3,
            offset: 0,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(page0.len(), 3);
    assert_eq!(page0[0].timestamp_epoch, 900.0);

    // Page 1: next 3 items (ts=600,500,400)
    let page1 = store::get_turns(
        db.conn(),
        TurnFilter {
            limit: 3,
            offset: 3,
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(page1.len(), 3);
    assert_eq!(page1[0].timestamp_epoch, 600.0);
}

#[test]
fn timeline_since_filter() {
    let db = open_temp_db();
    insert_raw_turn(
        &db,
        RawTurnRow {
            id: "t1",
            session_id: "sess1",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "old",
            timestamp_epoch: 100.0,
        },
    );
    insert_raw_turn(
        &db,
        RawTurnRow {
            id: "t2",
            session_id: "sess1",
            tool: "cc",
            turn_index: 1,
            role: "user",
            content: "recent",
            timestamp_epoch: 5000.0,
        },
    );

    let turns = store::get_turns(
        db.conn(),
        TurnFilter {
            since_epoch: Some(1000.0),
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(turns.len(), 1);
    assert_eq!(turns[0].content, "recent");
}

#[test]
fn stats_shows_per_tool_counts() {
    let db = open_temp_db();
    insert_raw_turn(
        &db,
        RawTurnRow {
            id: "t1",
            session_id: "s1",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "hi",
            timestamp_epoch: 1.0,
        },
    );
    insert_raw_turn(
        &db,
        RawTurnRow {
            id: "t2",
            session_id: "s2",
            tool: "codex",
            turn_index: 0,
            role: "user",
            content: "hi",
            timestamp_epoch: 2.0,
        },
    );
    insert_raw_turn(
        &db,
        RawTurnRow {
            id: "t3",
            session_id: "s2",
            tool: "codex",
            turn_index: 1,
            role: "assistant",
            content: "bye",
            timestamp_epoch: 3.0,
        },
    );

    let stats = store::get_stats(db.conn()).unwrap();

    let cc_stat = stats.iter().find(|s| s.tool == "cc").expect("cc stat");
    let codex_stat = stats
        .iter()
        .find(|s| s.tool == "codex")
        .expect("codex stat");
    assert_eq!(cc_stat.count, 1);
    assert_eq!(codex_stat.count, 2);
}

#[test]
fn stats_includes_date_range() {
    let db = open_temp_db();
    insert_raw_turn(
        &db,
        RawTurnRow {
            id: "t1",
            session_id: "s1",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "a",
            timestamp_epoch: 1000.0,
        },
    );
    insert_raw_turn(
        &db,
        RawTurnRow {
            id: "t2",
            session_id: "s1",
            tool: "cc",
            turn_index: 1,
            role: "user",
            content: "b",
            timestamp_epoch: 9000.0,
        },
    );

    let stats = store::get_stats(db.conn()).unwrap();
    let cc = stats.iter().find(|s| s.tool == "cc").unwrap();
    assert_eq!(cc.min_timestamp, 1000.0);
    assert_eq!(cc.max_timestamp, 9000.0);
}

#[test]
fn timeline_tool_filter() {
    let db = open_temp_db();

    let turns = vec![
        make_raw_turn("s1", Tool::Cc, 0, Role::User, "cc turn", 1.0),
        make_raw_turn("s2", Tool::Codex, 0, Role::User, "codex turn", 2.0),
    ];
    store::insert_turns(db.conn(), &turns).unwrap();

    let results = store::get_turns(
        db.conn(),
        TurnFilter {
            tool: Some(Tool::Cc),
            limit: 10,
            ..Default::default()
        },
    )
    .unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].content, "cc turn");
}

#[test]
fn stats_empty_db_returns_empty() {
    let db = open_temp_db();
    let stats = store::get_stats(db.conn()).unwrap();
    assert!(stats.is_empty());
}
