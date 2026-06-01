use mempal::core::db::Database;
use mempal::xurl::model::{Provenance, RawTurn, Role, Tool};
use mempal::xurl::store::{self, TurnFilter};
use rusqlite::params;
use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};
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

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn open_home_db(home: &Path) -> Database {
    let mempal_home = home.join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&mempal_home.join("palace.db")).expect("open home db")
}

fn run_xurl_timeline(home: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .args(["xurl", "timeline"])
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run mempal xurl timeline")
}

fn run_xurl_stats(home: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .args(["xurl", "stats"])
        .args(args)
        .env("HOME", home)
        .output()
        .expect("run mempal xurl stats")
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
    insert_raw_turn_with_project_path(db.conn(), row, None);
}

fn insert_raw_turn_with_project_path(
    conn: &rusqlite::Connection,
    row: RawTurnRow<'_>,
    project_path: Option<&str>,
) {
    conn.execute(
        "INSERT INTO conversation_turns \
         (id, session_id, tool, turn_index, role, content, timestamp_epoch, \
          token_count, project_path, git_branch, is_csa_delegated, provenance) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,NULL,?8,NULL,0,'human')",
        params![
            row.id,
            row.session_id,
            row.tool,
            row.turn_index,
            row.role,
            row.content,
            row.timestamp_epoch,
            project_path,
        ],
    )
    .expect("insert raw turn");
}

fn insert_home_turn(db: &Database, row: RawTurnRow<'_>, project_path: Option<&str>) {
    insert_raw_turn_with_project_path(db.conn(), row, project_path);
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout utf8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr utf8")
}

fn json_turn_by_id<'a>(turns: &'a [Value], id: &str) -> &'a Value {
    turns
        .iter()
        .find(|turn| turn["id"] == id)
        .unwrap_or_else(|| panic!("missing turn {id}: {turns:?}"))
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_secs_f64()
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
fn timeline_json_includes_source_path_for_backfilled_and_null_turns() {
    let home = TempDir::new().expect("home");
    let db = open_home_db(home.path());
    insert_home_turn(
        &db,
        RawTurnRow {
            id: "with-source",
            session_id: "with-project",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "turn with project path",
            timestamp_epoch: 2_000.0,
        },
        Some("/repo/with-project"),
    );
    insert_home_turn(
        &db,
        RawTurnRow {
            id: "without-source",
            session_id: "without-project",
            tool: "cc",
            turn_index: 1,
            role: "assistant",
            content: "turn without project path",
            timestamp_epoch: 1_000.0,
        },
        None,
    );

    let output = run_xurl_timeline(home.path(), &["--format", "json", "--limit", "10"]);
    assert!(
        output.status.success(),
        "timeline json failed: {}",
        stderr(&output)
    );
    let turns: Vec<Value> = serde_json::from_str(&stdout(&output)).expect("timeline json");

    let with_source = json_turn_by_id(&turns, "with-source");
    assert_eq!(
        with_source["source_path"],
        Value::String("/repo/with-project".to_string())
    );
    let without_source = json_turn_by_id(&turns, "without-source");
    assert!(
        without_source["source_path"].is_null(),
        "source_path must be JSON null for unbackfilled turns: {without_source}"
    );
}

#[test]
fn timeline_markdown_shows_source_path_segment_only_when_present() {
    let home = TempDir::new().expect("home");
    let db = open_home_db(home.path());
    insert_home_turn(
        &db,
        RawTurnRow {
            id: "with-source",
            session_id: "with-project",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "turn with project path",
            timestamp_epoch: 2_000.0,
        },
        Some("/repo/with-project"),
    );
    insert_home_turn(
        &db,
        RawTurnRow {
            id: "without-source",
            session_id: "without-project",
            tool: "cc",
            turn_index: 1,
            role: "assistant",
            content: "turn without project path",
            timestamp_epoch: 1_000.0,
        },
        None,
    );

    let output = run_xurl_timeline(home.path(), &["--limit", "10"]);
    assert!(
        output.status.success(),
        "timeline markdown failed: {}",
        stderr(&output)
    );
    let markdown = stdout(&output);
    let with_header = markdown
        .lines()
        .find(|line| line.contains("with-project"))
        .expect("with-project header");
    assert!(
        with_header.contains(" · /repo/with-project"),
        "expected provenance segment in header: {with_header}"
    );

    let without_header = markdown
        .lines()
        .find(|line| line.contains("without-project"))
        .expect("without-project header");
    assert!(
        !without_header.contains("null"),
        "NULL project_path must not be printed: {without_header}"
    );
    assert!(
        !without_header.ends_with(" · "),
        "NULL project_path must not leave a dangling separator: {without_header}"
    );
    assert_eq!(
        without_header.matches(" · ").count(),
        2,
        "NULL header should only contain session/timestamp/role separators: {without_header}"
    );
}

#[test]
fn timeline_rejects_invalid_format_at_parse_time() {
    let home = TempDir::new().expect("home");
    let output = run_xurl_timeline(home.path(), &["--format", "bogus"]);

    assert!(!output.status.success(), "timeline format should fail");
    let err = stderr(&output);
    assert!(
        err.contains("invalid value") && err.contains("markdown") && err.contains("json"),
        "clap error should list valid formats, got: {err}"
    );
}

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
fn stats_cli_json_reports_tools_and_unindexed_remaining() {
    let home = TempDir::new().expect("home");
    let db = open_home_db(home.path());
    insert_home_turn(
        &db,
        RawTurnRow {
            id: "cc-stat",
            session_id: "s1",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "hi",
            timestamp_epoch: 1_000.0,
        },
        None,
    );
    insert_home_turn(
        &db,
        RawTurnRow {
            id: "codex-stat",
            session_id: "s2",
            tool: "codex",
            turn_index: 0,
            role: "assistant",
            content: "bye",
            timestamp_epoch: 2_000.0,
        },
        None,
    );

    let json_output = run_xurl_stats(home.path(), &["--json"]);
    assert!(
        json_output.status.success(),
        "stats json failed: {}",
        stderr(&json_output)
    );
    let report: Value = serde_json::from_str(&stdout(&json_output)).expect("stats json");
    assert_eq!(report["unindexed_remaining"], Value::from(2));
    let tools = report["tools"].as_array().expect("tools array");
    let cc = tools
        .iter()
        .find(|tool| tool["tool"] == "cc")
        .expect("cc stats");
    assert_eq!(cc["count"], Value::from(1));
    assert_eq!(cc["min_timestamp"], Value::from(1_000.0));
    assert_eq!(cc["max_timestamp"], Value::from(1_000.0));
    assert!(cc["first"].as_str().expect("first").contains('T'));
    assert!(cc["last"].as_str().expect("last").contains('T'));

    let human_output = run_xurl_stats(home.path(), &[]);
    assert!(
        human_output.status.success(),
        "stats human failed: {}",
        stderr(&human_output)
    );
    let human = stdout(&human_output);
    assert!(human.contains("| tool"));
    assert!(human.contains("unindexed_remaining: 2"));
}

#[test]
fn stats_cli_filters_tool_session_and_since_counts() {
    let home = TempDir::new().expect("home");
    let db = open_home_db(home.path());
    let now = now_epoch();
    let recent = now - 60.0 * 60.0;
    let old = now - 10.0 * 24.0 * 60.0 * 60.0;

    insert_home_turn(
        &db,
        RawTurnRow {
            id: "cc-recent-keep",
            session_id: "keep",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "recent cc keep",
            timestamp_epoch: recent,
        },
        None,
    );
    insert_home_turn(
        &db,
        RawTurnRow {
            id: "cc-old-keep",
            session_id: "keep",
            tool: "cc",
            turn_index: 1,
            role: "user",
            content: "old cc keep",
            timestamp_epoch: old,
        },
        None,
    );
    insert_home_turn(
        &db,
        RawTurnRow {
            id: "cc-recent-other-session",
            session_id: "other",
            tool: "cc",
            turn_index: 0,
            role: "user",
            content: "recent cc other",
            timestamp_epoch: recent,
        },
        None,
    );
    insert_home_turn(
        &db,
        RawTurnRow {
            id: "codex-recent-keep",
            session_id: "keep",
            tool: "codex",
            turn_index: 0,
            role: "assistant",
            content: "recent codex keep",
            timestamp_epoch: recent,
        },
        None,
    );

    let global_output = run_xurl_stats(home.path(), &["--json"]);
    assert!(
        global_output.status.success(),
        "global stats failed: {}",
        stderr(&global_output)
    );
    let global: Value = serde_json::from_str(&stdout(&global_output)).expect("global stats json");
    assert_eq!(global["unindexed_remaining"], Value::from(4));
    let global_tools = global["tools"].as_array().expect("global tools");
    let global_cc = global_tools
        .iter()
        .find(|tool| tool["tool"] == "cc")
        .expect("global cc stat");
    assert_eq!(global_cc["count"], Value::from(3));

    let filtered_output = run_xurl_stats(
        home.path(),
        &[
            "--tool",
            "cc",
            "--session",
            "keep",
            "--since",
            "2d",
            "--json",
        ],
    );
    assert!(
        filtered_output.status.success(),
        "filtered stats failed: {}",
        stderr(&filtered_output)
    );
    let filtered: Value =
        serde_json::from_str(&stdout(&filtered_output)).expect("filtered stats json");
    assert_eq!(filtered["unindexed_remaining"], Value::from(1));
    let filtered_tools = filtered["tools"].as_array().expect("filtered tools");
    assert_eq!(filtered_tools.len(), 1);
    assert_eq!(filtered_tools[0]["tool"], Value::from("cc"));
    assert_eq!(filtered_tools[0]["count"], Value::from(1));
    let min_timestamp = filtered_tools[0]["min_timestamp"]
        .as_f64()
        .expect("min timestamp");
    let max_timestamp = filtered_tools[0]["max_timestamp"]
        .as_f64()
        .expect("max timestamp");
    assert!(
        (min_timestamp - recent).abs() < 0.001,
        "unexpected min timestamp: {min_timestamp}"
    );
    assert!(
        (max_timestamp - recent).abs() < 0.001,
        "unexpected max timestamp: {max_timestamp}"
    );
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
