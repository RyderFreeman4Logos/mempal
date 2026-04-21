use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blake3::Hasher;
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::ingest::gating::GatingDecision;
use mempal::ingest::novelty::NoveltyAction;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

struct DashboardEnv {
    _tmp: TempDir,
    home: PathBuf,
    db_path: PathBuf,
}

impl DashboardEnv {
    fn new(project_id: Option<&str>, strict_project_isolation: bool) -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().join("home");
        let mempal_home = home.join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        write_config_atomic(
            &mempal_home.join("config.toml"),
            &config_text(&db_path, project_id, strict_project_isolation),
        );
        Self {
            _tmp: tmp,
            home,
            db_path,
        }
    }

    fn cwd(&self) -> &Path {
        &self.home
    }
}

fn config_text(db_path: &Path, project_id: Option<&str>, strict_project_isolation: bool) -> String {
    let project_section = project_id
        .map(|project_id| format!("\n[project]\nid = \"{project_id}\"\n"))
        .unwrap_or_default();
    format!(
        r#"
db_path = "{}"
{}
[embedder]
backend = "api"
base_url = "http://127.0.0.1:9/v1/"
api_model = "test-model"

[search]
strict_project_isolation = {}
"#,
        db_path.display(),
        project_section,
        strict_project_isolation
    )
}

fn write_config_atomic(path: &Path, contents: &str) {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, contents).expect("write temp config");
    fs::rename(&tmp, path).expect("rename config");
}

fn run_mempal(home: &Path, cwd: &Path, args: &[&str]) -> std::process::Output {
    Command::new(mempal_bin())
        .args(args)
        .env("HOME", home)
        .current_dir(cwd)
        .output()
        .expect("run mempal")
}

fn spawn_mempal(home: &Path, cwd: &Path, args: &[&str]) -> Child {
    Command::new(mempal_bin())
        .args(args)
        .env("HOME", home)
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn mempal")
}

struct DrawerSeed<'a> {
    id: &'a str,
    content: &'a str,
    wing: &'a str,
    room: Option<&'a str>,
    added_at: String,
    importance: i32,
    project_id: Option<&'a str>,
}

fn insert_drawer(db_path: &Path, seed: DrawerSeed<'_>) {
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer(&Drawer {
        id: seed.id.to_string(),
        content: seed.content.to_string(),
        wing: seed.wing.to_string(),
        room: seed.room.map(str::to_string),
        source_file: Some(format!("{}.md", seed.id)),
        source_type: SourceType::Manual,
        added_at: seed.added_at,
        chunk_index: Some(0),
        importance: seed.importance,
    })
    .expect("insert drawer");
    db.conn()
        .execute(
            "UPDATE drawers SET project_id = ?2 WHERE id = ?1",
            rusqlite::params![seed.id, seed.project_id],
        )
        .expect("update drawer project");
}

fn record_gating_audit(db_path: &Path, drawer_id: &str, accepted: bool, project_id: Option<&str>) {
    let db = Database::open(db_path).expect("open db");
    let decision = if accepted {
        GatingDecision::accepted(1, Some("keep".to_string()), Some(0.9))
    } else {
        GatingDecision::rejected(1, Some("drop".to_string()), None)
    };
    db.record_gating_audit(drawer_id, &decision, project_id)
        .expect("record gating audit");
}

fn record_novelty_audit(
    db_path: &Path,
    drawer_id: &str,
    action: NoveltyAction,
    project_id: Option<&str>,
) {
    let db = Database::open(db_path).expect("open db");
    db.record_novelty_audit(
        drawer_id,
        action,
        Some(drawer_id),
        Some(0.91),
        None,
        project_id,
    )
    .expect("record novelty audit");
}

fn read_lines_until(
    child: &mut Child,
    expected_lines: usize,
    timeout: Duration,
) -> (Vec<String>, Option<Instant>) {
    let stdout = child.stdout.take().expect("stdout");
    let (tx, rx) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let reader = BufReader::new(stdout);
        for line in reader.lines() {
            match line {
                Ok(line) => {
                    if tx.send(line).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let start = Instant::now();
    let mut first_at = None;
    let mut lines = Vec::new();
    while start.elapsed() < timeout && lines.len() < expected_lines {
        if let Ok(line) = rx.recv_timeout(Duration::from_millis(100)) {
            if first_at.is_none() && !line.trim().is_empty() {
                first_at = Some(Instant::now());
            }
            if !line.trim().is_empty() {
                lines.push(line);
            }
        }
    }
    (lines, first_at)
}

#[derive(Debug, PartialEq, Eq)]
struct DbSnapshot {
    drawer_count: i64,
    triple_count: i64,
    tunnel_count: usize,
    digest: String,
}

fn snapshot_db(db_path: &Path) -> DbSnapshot {
    let db = Database::open(db_path).expect("open db");
    let drawer_count = db.drawer_count().expect("drawer count");
    let triple_count = db.triple_count().expect("triple count");
    let tunnels = db.find_tunnels().expect("find tunnels");

    let mut hasher = Hasher::new();
    let mut stmt = db
        .conn()
        .prepare(
            "SELECT id, content, wing, COALESCE(room, ''), COALESCE(project_id, '') FROM drawers ORDER BY id",
        )
        .expect("prepare drawers");
    let drawers = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })
        .expect("query drawers")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect drawers");
    for drawer in drawers {
        hasher.update(drawer.0.as_bytes());
        hasher.update(drawer.1.as_bytes());
        hasher.update(drawer.2.as_bytes());
        hasher.update(drawer.3.as_bytes());
        hasher.update(drawer.4.as_bytes());
    }
    let mut stmt = db
        .conn()
        .prepare("SELECT id, subject, predicate, object FROM triples ORDER BY id")
        .expect("prepare triples");
    let triples = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .expect("query triples")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect triples");
    for triple in triples {
        hasher.update(triple.0.as_bytes());
        hasher.update(triple.1.as_bytes());
        hasher.update(triple.2.as_bytes());
        hasher.update(triple.3.as_bytes());
    }
    for (room, wings) in tunnels {
        hasher.update(room.as_bytes());
        for wing in wings {
            hasher.update(wing.as_bytes());
        }
    }

    DbSnapshot {
        drawer_count,
        triple_count,
        tunnel_count: db.find_tunnels().expect("find tunnels").len(),
        digest: hasher.finalize().to_hex().to_string(),
    }
}

fn unix_secs_days_ago(days: u64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_secs();
    (now - days * 86_400).to_string()
}

fn day_label_days_ago(days: u64) -> String {
    let secs = unix_secs_days_ago(days)
        .parse::<u64>()
        .expect("day timestamp as u64");
    mempal::cowork::peek::format_rfc3339(UNIX_EPOCH + Duration::from_secs(secs))[..10].to_string()
}

#[test]
fn test_mempal_tail_emits_most_recent_drawer_first() {
    let env = DashboardEnv::new(None, false);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "oldest",
            content: "old",
            wing: "code",
            room: Some("a"),
            added_at: "1713000000".to_string(),
            importance: 1,
            project_id: None,
        },
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "newer",
            content: "new",
            wing: "code",
            room: Some("a"),
            added_at: "1713000100".to_string(),
            importance: 1,
            project_id: None,
        },
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "newest",
            content: "latest",
            wing: "docs",
            room: Some("b"),
            added_at: "1713000200".to_string(),
            importance: 1,
            project_id: None,
        },
    );

    let output = run_mempal(&env.home, env.cwd(), &["tail", "--limit", "2"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout.lines().collect::<Vec<_>>();

    assert!(lines.first().is_some_and(|line| line.contains("newest")));
    assert!(lines.get(1).is_some_and(|line| line.contains("newer")));
}

#[test]
fn test_mempal_tail_follow_coalesces_event_storm() {
    let env = DashboardEnv::new(None, false);
    let mut child = spawn_mempal(&env.home, env.cwd(), &["tail", "--follow", "--limit", "0"]);
    std::thread::sleep(Duration::from_millis(300));

    let burst_started = Instant::now();
    for index in 0..20 {
        let drawer_id = format!("follow-{index:02}");
        let content = format!("burst drawer {index}");
        insert_drawer(
            &env.db_path,
            DrawerSeed {
                id: Box::leak(drawer_id.into_boxed_str()),
                content: Box::leak(content.into_boxed_str()),
                wing: "code",
                room: Some("follow"),
                added_at: format!("{}", 1713001000 + index),
                importance: 1,
                project_id: None,
            },
        );
    }

    let (lines, first_at) = read_lines_until(&mut child, 20, Duration::from_secs(5));
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(lines.len(), 20, "stdout lines: {lines:?}");
    let first_elapsed = first_at
        .map(|instant| instant.duration_since(burst_started))
        .expect("expected follow output");
    assert!(
        first_elapsed >= Duration::from_millis(200),
        "follow output arrived too early for debounce: {first_elapsed:?}"
    );
    for index in 0..20 {
        let needle = format!("follow-{index:02}");
        assert!(
            lines.iter().any(|line| line.contains(&needle)),
            "missing follow line for {needle}: {lines:?}"
        );
    }
}

#[test]
fn test_mempal_timeline_groups_by_day() {
    let env = DashboardEnv::new(None, false);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "d1",
            content: "day one",
            wing: "code",
            room: Some("a"),
            added_at: unix_secs_days_ago(2),
            importance: 1,
            project_id: None,
        },
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "d2",
            content: "day two",
            wing: "code",
            room: Some("a"),
            added_at: unix_secs_days_ago(1),
            importance: 1,
            project_id: None,
        },
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "d3",
            content: "day three",
            wing: "code",
            room: Some("a"),
            added_at: unix_secs_days_ago(0),
            importance: 1,
            project_id: None,
        },
    );

    let output = run_mempal(&env.home, env.cwd(), &["timeline", "--since", "7d"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains(&format!("=== {} ===", day_label_days_ago(2))));
    assert!(stdout.contains(&format!("=== {} ===", day_label_days_ago(1))));
    assert!(stdout.contains(&format!("=== {} ===", day_label_days_ago(0))));
}

#[test]
fn test_mempal_stats_counts_drawers_by_wing_room() {
    let env = DashboardEnv::new(None, false);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "s1",
            content: "one",
            wing: "code",
            room: Some("core"),
            added_at: "1713000000".to_string(),
            importance: 3,
            project_id: None,
        },
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "s2",
            content: "two",
            wing: "code",
            room: Some("core"),
            added_at: "1713000010".to_string(),
            importance: 2,
            project_id: None,
        },
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "s3",
            content: "three",
            wing: "docs",
            room: Some("notes"),
            added_at: "1713000020".to_string(),
            importance: 1,
            project_id: None,
        },
    );
    record_gating_audit(&env.db_path, "s1", true, None);
    record_novelty_audit(&env.db_path, "s2", NoveltyAction::Merge, None);

    let output = run_mempal(&env.home, env.cwd(), &["stats"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("drawers total: 3"));
    assert!(stdout.contains("code/core: 2"));
    assert!(stdout.contains("docs/notes: 1"));
}

#[test]
fn test_mempal_view_drawer_id_prints_raw_verbatim() {
    let env = DashboardEnv::new(None, false);
    let content = "Decision: use raw output here.\nSecond line stays verbatim.";
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "view-1",
            content,
            wing: "code",
            room: Some("view"),
            added_at: "1713000000".to_string(),
            importance: 4,
            project_id: None,
        },
    );

    let output = run_mempal(&env.home, env.cwd(), &["view", "view-1", "--raw"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("view-1"));
    assert!(stdout.contains(content));
    assert!(!stdout.contains("\u{1b}["));
}

#[test]
fn test_mempal_audit_respects_project_isolation() {
    let env = DashboardEnv::new(Some("proj-A"), true);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "audit-a",
            content: "project A drawer",
            wing: "code",
            room: Some("audit"),
            added_at: "1713000000".to_string(),
            importance: 1,
            project_id: Some("proj-A"),
        },
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "audit-b",
            content: "project B drawer",
            wing: "code",
            room: Some("audit"),
            added_at: "1713000001".to_string(),
            importance: 1,
            project_id: Some("proj-B"),
        },
    );
    record_novelty_audit(
        &env.db_path,
        "audit-a",
        NoveltyAction::Insert,
        Some("proj-A"),
    );
    record_novelty_audit(&env.db_path, "audit-b", NoveltyAction::Drop, Some("proj-B"));

    let output = run_mempal(
        &env.home,
        env.cwd(),
        &["audit", "--kind", "novelty", "--since", "7d"],
    );
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("audit-a"));
    assert!(!stdout.contains("audit-b"));
}

#[test]
fn test_dashboard_commands_do_not_mutate_db() {
    let env = DashboardEnv::new(None, false);
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "d1",
            content: "drawer one",
            wing: "code",
            room: Some("same"),
            added_at: "1713000000".to_string(),
            importance: 1,
            project_id: None,
        },
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "d2",
            content: "drawer two",
            wing: "docs",
            room: Some("same"),
            added_at: "1713000001".to_string(),
            importance: 1,
            project_id: None,
        },
    );
    record_gating_audit(&env.db_path, "d1", true, None);
    record_novelty_audit(&env.db_path, "d2", NoveltyAction::Merge, None);

    let before = snapshot_db(&env.db_path);

    for args in [
        vec!["tail", "--limit", "2"],
        vec!["timeline", "--since", "7d"],
        vec!["stats"],
        vec!["view", "d1", "--raw"],
        vec!["audit", "--since", "7d"],
    ] {
        let output = run_mempal(&env.home, env.cwd(), &args);
        assert!(
            output.status.success(),
            "args={args:?} stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let after = snapshot_db(&env.db_path);
    assert_eq!(before, after);
}

#[test]
fn test_mempal_audit_keeps_project_scoped_rejects_without_drawer_rows() {
    let env = DashboardEnv::new(Some("proj-A"), true);
    record_gating_audit(&env.db_path, "candidate-a", false, Some("proj-A"));
    record_gating_audit(&env.db_path, "candidate-b", false, Some("proj-B"));

    let output = run_mempal(
        &env.home,
        env.cwd(),
        &["audit", "--kind", "gating", "--since", "7d"],
    );
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("candidate-a"));
    assert!(!stdout.contains("candidate-b"));
}

#[test]
fn test_mempal_stats_counts_project_scoped_audit_rows_without_drawer_rows() {
    let env = DashboardEnv::new(Some("proj-A"), true);
    record_gating_audit(&env.db_path, "candidate-a", false, Some("proj-A"));
    record_gating_audit(&env.db_path, "candidate-b", false, Some("proj-B"));

    let output = run_mempal(&env.home, env.cwd(), &["stats"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(stdout.contains("gating:"));
    assert!(stdout.contains("  rejected: 1"));
}
