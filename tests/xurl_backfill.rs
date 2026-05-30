use std::path::{Path, PathBuf};
use std::process::Command;

use mempal::core::db::Database;
use mempal::xurl::backfill::{self, BackfillOptions, BackfillSourceConfig};
use mempal::xurl::model::{Provenance, RawTurn, Role, Tool};
use mempal::xurl::store::{self, TurnFilter};
use serde_json::Value;
use tempfile::TempDir;

struct TestDb {
    _dir: TempDir,
    path: PathBuf,
    inner: Database,
}

impl TestDb {
    fn conn(&self) -> &rusqlite::Connection {
        self.inner.conn()
    }
}

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn open_temp_db() -> TestDb {
    let dir = TempDir::new().expect("tempdir");
    let path = dir.path().join("palace.db");
    let inner = Database::open(&path).expect("open db");
    TestDb {
        _dir: dir,
        path,
        inner,
    }
}

fn make_turn(session_id: &str, turn_index: u32, project_path: Option<&str>) -> RawTurn {
    RawTurn {
        session_id: session_id.to_string(),
        tool: Tool::Cc,
        role: Role::User,
        content: format!("turn {turn_index}"),
        timestamp_epoch: 1_000.0 + f64::from(turn_index),
        project_path: project_path.map(str::to_string),
        git_branch: None,
        is_csa_delegated: false,
        provenance: Provenance::Human,
        turn_index,
    }
}

fn make_cc_line(session_id: &str, text: &str) -> String {
    serde_json::json!({
        "type": "user",
        "timestamp": "2026-05-27T12:00:00Z",
        "sessionId": session_id,
        "userType": "external",
        "message": {
            "role": "user",
            "content": [{"type": "text", "text": text}]
        }
    })
    .to_string()
}

fn write_cc_source(home: &Path, session_id: &str, cwd: &str) -> PathBuf {
    let encoded = cwd.replace('/', "-");
    let session_dir = home.join(".claude/projects").join(encoded);
    std::fs::create_dir_all(&session_dir).expect("create source dir");
    let path = session_dir.join(format!("{session_id}.jsonl"));
    std::fs::write(&path, make_cc_line(session_id, "historical xurl turn")).expect("write source");
    path
}

fn sources(home: &Path) -> BackfillSourceConfig {
    BackfillSourceConfig {
        cc_root: home.join(".claude/projects"),
        codex_root: home.join(".codex/sessions"),
        hermes_db: None,
    }
}

fn project_paths_for_session(db: &TestDb, session_id: &str) -> Vec<Option<String>> {
    store::get_turns(
        db.conn(),
        TurnFilter {
            session_id: Some(session_id.to_string()),
            limit: 10,
            ..Default::default()
        },
    )
    .expect("get turns")
    .into_iter()
    .map(|turn| turn.project_path)
    .collect()
}

#[test]
fn backfill_cc_null_project_path_from_encoded_dir_and_is_idempotent() {
    let home = TempDir::new().expect("home");
    let db = open_temp_db();
    let cwd = "/repo/encoded";
    write_cc_source(home.path(), "cc-historical", cwd);
    let turns = vec![
        make_turn("cc-historical", 0, None),
        make_turn("cc-historical", 1, None),
    ];
    store::insert_turns(db.conn(), &turns).expect("insert historical turns");

    let stats = backfill::backfill_project_paths(
        db.conn(),
        &sources(home.path()),
        BackfillOptions::execute(),
    )
    .expect("backfill");
    assert_eq!(stats.sessions_scanned, 1);
    assert_eq!(stats.turns_filled, 2);
    assert_eq!(stats.turns_skipped_no_source, 0);
    assert_eq!(stats.batches, 1);
    assert_eq!(stats.by_project_path[cwd].turns, 2);
    assert_eq!(
        project_paths_for_session(&db, "cc-historical"),
        vec![Some(cwd.to_string()), Some(cwd.to_string())]
    );

    let second = backfill::backfill_project_paths(
        db.conn(),
        &sources(home.path()),
        BackfillOptions::execute(),
    )
    .expect("second backfill");
    assert_eq!(second.sessions_scanned, 0);
    assert_eq!(second.turns_filled, 0);
}

#[test]
fn backfill_leaves_existing_project_path_unmodified() {
    let home = TempDir::new().expect("home");
    let db = open_temp_db();
    write_cc_source(home.path(), "already-set", "/repo/from-source");
    let turn = make_turn("already-set", 0, Some("/repo/existing"));
    store::insert_turns(db.conn(), &[turn]).expect("insert turn");

    let stats = backfill::backfill_project_paths(
        db.conn(),
        &sources(home.path()),
        BackfillOptions::execute(),
    )
    .expect("backfill");
    assert_eq!(stats.sessions_scanned, 0);
    assert_eq!(stats.turns_filled, 0);
    assert_eq!(
        project_paths_for_session(&db, "already-set"),
        vec![Some("/repo/existing".to_string())]
    );
}

#[test]
fn backfill_missing_source_stays_null_and_is_counted() {
    let home = TempDir::new().expect("home");
    let db = open_temp_db();
    let turn = make_turn("missing-source", 0, None);
    store::insert_turns(db.conn(), &[turn]).expect("insert turn");

    let stats = backfill::backfill_project_paths(
        db.conn(),
        &sources(home.path()),
        BackfillOptions::execute(),
    )
    .expect("backfill");
    assert_eq!(stats.sessions_scanned, 1);
    assert_eq!(stats.turns_filled, 0);
    assert_eq!(stats.turns_skipped_no_source, 1);
    assert_eq!(project_paths_for_session(&db, "missing-source"), vec![None]);
}

#[test]
fn backfill_dry_run_writes_nothing_and_reports_would_fill_counts() {
    let home = TempDir::new().expect("home");
    let db = open_temp_db();
    let cwd = "/repo/dryrun";
    write_cc_source(home.path(), "dry-run-session", cwd);
    let turn = make_turn("dry-run-session", 0, None);
    store::insert_turns(db.conn(), &[turn]).expect("insert turn");
    db.conn()
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .expect("checkpoint");
    let before = std::fs::read(&db.path).expect("read db before dry-run");

    let stats = backfill::backfill_project_paths(
        db.conn(),
        &sources(home.path()),
        BackfillOptions::dry_run(),
    )
    .expect("dry-run");
    let after = std::fs::read(&db.path).expect("read db after dry-run");

    assert_eq!(stats.sessions_scanned, 1);
    assert_eq!(stats.turns_filled, 1);
    assert_eq!(stats.by_project_path[cwd].turns, 1);
    assert_eq!(
        project_paths_for_session(&db, "dry-run-session"),
        vec![None]
    );
    assert_eq!(before, after);
}

#[test]
fn xurl_backfill_cli_prints_json_summary() {
    let home = TempDir::new().expect("home");
    let mempal_home = home.path().join(".mempal");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    let cwd = "/repo/clijson";
    write_cc_source(home.path(), "cli-json-session", cwd);
    let turn = make_turn("cli-json-session", 0, None);
    store::insert_turns(db.conn(), &[turn]).expect("insert turn");

    let output = Command::new(mempal_bin())
        .args(["xurl", "backfill", "--dry-run", "--json"])
        .env("HOME", home.path())
        .output()
        .expect("run xurl backfill");
    assert!(
        output.status.success(),
        "backfill command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let summary: Value = serde_json::from_slice(&output.stdout).expect("parse json");
    assert_eq!(summary["sessions_scanned"], 1);
    assert_eq!(summary["turns_filled"], 1);
    assert_eq!(summary["by_project_path"][cwd]["turns"], 1);
}
