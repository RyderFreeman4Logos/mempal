use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use blake3::Hasher;
use mempal::core::db::Database;
use mempal::core::project::ProjectSearchScope;
use mempal::core::queue::PendingMessageStore;
use mempal::core::types::{Drawer, SourceType};
use mempal::ingest::gating::GatingDecision;
use mempal::ingest::novelty::NoveltyAction;
use mempal::observability::{self, TailFollowEvent, TailFollowFilters, TailFollowWake};
use mempal::session_review::append_hooks_raw_metadata;
use rusqlite::params;
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
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().join("home");
        let mempal_home = home.join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        write_dashboard_config(&mempal_home.join("config.toml"), &db_path, None, false);
        Self {
            _tmp: tmp,
            home,
            db_path,
        }
    }

    fn cwd(&self) -> &Path {
        &self.home
    }

    fn set_project_scope(&self, project_id: Option<&str>, strict_project_isolation: bool) {
        write_dashboard_config(
            &self.home.join(".mempal").join("config.toml"),
            &self.db_path,
            project_id,
            strict_project_isolation,
        );
    }
}

#[derive(Clone)]
struct DrawerSeed {
    id: String,
    content: String,
    wing: String,
    room: Option<String>,
    added_at: String,
    importance: i32,
}

fn drawer_seed(
    id: impl Into<String>,
    content: impl Into<String>,
    wing: impl Into<String>,
    room: Option<&str>,
    added_at: impl Into<String>,
    importance: i32,
) -> DrawerSeed {
    DrawerSeed {
        id: id.into(),
        content: content.into(),
        wing: wing.into(),
        room: room.map(str::to_string),
        added_at: added_at.into(),
        importance,
    }
}

fn insert_drawer(db_path: &Path, seed: &DrawerSeed) {
    insert_drawer_with_project(db_path, seed, None);
}

fn insert_drawer_with_project(db_path: &Path, seed: &DrawerSeed, project_id: Option<&str>) {
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer_with_project(
        &Drawer {
            id: seed.id.clone(),
            content: seed.content.clone(),
            wing: seed.wing.clone(),
            room: seed.room.clone(),
            source_file: Some(format!("{}.md", seed.id)),
            source_type: SourceType::Manual,
            added_at: seed.added_at.clone(),
            chunk_index: Some(0),
            importance: seed.importance,
        },
        project_id,
    )
    .expect("insert drawer");
}

fn write_dashboard_config(
    path: &Path,
    db_path: &Path,
    project_id: Option<&str>,
    strict_project_isolation: bool,
) {
    let project_section = project_id
        .map(|project_id| format!("\n[project]\nid = \"{project_id}\"\n"))
        .unwrap_or_default();
    write_config_atomic(
        path,
        &format!(
            r#"
db_path = "{}"
{}
[embed]
backend = "model2vec"

[search]
strict_project_isolation = {}
"#,
            db_path.display(),
            project_section,
            strict_project_isolation
        ),
    );
}

fn record_gating_audit(db_path: &Path, drawer_id: &str, accepted: bool) {
    let db = Database::open(db_path).expect("open db");
    let decision = if accepted {
        GatingDecision::accepted(1, Some("keep".to_string()), Some(0.9))
    } else {
        GatingDecision::rejected(1, Some("drop".to_string()), None, None)
    };
    db.record_gating_audit(drawer_id, &decision, None)
        .expect("record gating audit");
}

fn record_novelty_audit(db_path: &Path, drawer_id: &str, action: NoveltyAction, created_at: i64) {
    let db = Database::open(db_path).expect("open db");
    db.record_novelty_audit(drawer_id, action, Some(drawer_id), Some(0.91), None, None)
        .expect("record novelty audit");
    db.conn()
        .execute(
            "UPDATE novelty_audit SET created_at = ?2 WHERE candidate_hash = ?1",
            params![drawer_id, created_at],
        )
        .expect("set novelty created_at");
}

fn seed_queue_rows(db_path: &Path) {
    let store = PendingMessageStore::new(db_path).expect("queue store");
    let _pending = store
        .enqueue("hook", "pending payload")
        .expect("enqueue pending");
    let _claimed = store
        .enqueue("hook", "claimed payload")
        .expect("enqueue claimed");
    let failed = store
        .enqueue("hook", "failed payload")
        .expect("enqueue failed");
    let _claim = store
        .claim_next("dashboard-test", 120)
        .expect("claim next")
        .expect("claimed message");
    store.mark_failed(&failed, "boom").expect("mark failed");
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
        .prepare("SELECT id, content, wing, COALESCE(room, '') FROM drawers ORDER BY id")
        .expect("prepare drawers");
    let drawers = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
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

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_secs() as i64
}

#[test]
fn test_audit_novelty_lists_decisions() {
    let env = DashboardEnv::new();
    let decisions = [
        ("novelty-09", NoveltyAction::Insert),
        ("novelty-08", NoveltyAction::Insert),
        ("novelty-07", NoveltyAction::Merge),
        ("novelty-06", NoveltyAction::Drop),
        ("novelty-05", NoveltyAction::Insert),
        ("novelty-04", NoveltyAction::Drop),
        ("novelty-03", NoveltyAction::Merge),
        ("novelty-02", NoveltyAction::Insert),
        ("novelty-01", NoveltyAction::Drop),
        ("novelty-00", NoveltyAction::Insert),
    ];
    for (index, (drawer_id, action)) in decisions.iter().enumerate() {
        insert_drawer(
            &env.db_path,
            &drawer_seed(
                *drawer_id,
                format!("novelty drawer {index}"),
                "default",
                Some("audit"),
                format!("{}", 1_713_000_000 + index),
                2,
            ),
        );
        record_novelty_audit(
            &env.db_path,
            drawer_id,
            *action,
            now_unix_secs() - 60 + (decisions.len() - index) as i64,
        );
    }

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
    let detail_lines = stdout
        .lines()
        .filter(|line| line.starts_with("  "))
        .collect::<Vec<_>>();
    assert_eq!(detail_lines.len(), 10, "{stdout}");
    assert!(
        detail_lines
            .iter()
            .any(|line| line.contains("decision=drop"))
    );
    assert!(
        detail_lines
            .iter()
            .any(|line| line.contains("decision=merge"))
    );
    assert!(
        detail_lines
            .iter()
            .any(|line| line.contains("decision=insert"))
    );
    assert!(detail_lines[0].contains("novelty-09"), "{stdout}");
    assert!(
        detail_lines[0].contains("similarity_score=0.910"),
        "{stdout}"
    );
}

#[test]
fn test_missing_palace_db_friendly_error() {
    let tmp = TempDir::new().expect("tempdir");
    let output = run_mempal(tmp.path(), tmp.path(), &["tail"]);
    assert!(!output.status.success(), "command unexpectedly succeeded");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no palace.db found at ~/.mempal/palace.db; run `mempal init` first"),
        "{stderr}"
    );
    assert!(!stderr.to_lowercase().contains("panic"), "{stderr}");
}

#[cfg(target_os = "linux")]
#[test]
fn test_no_http_port_bound() {
    let env = DashboardEnv::new();
    let mut child = spawn_mempal(&env.home, env.cwd(), &["tail", "--follow", "--limit", "0"]);
    std::thread::sleep(Duration::from_millis(400));

    let output = Command::new("sh")
        .args([
            "-c",
            "if command -v ss >/dev/null 2>&1; then ss -ltnp; elif command -v netstat >/dev/null 2>&1; then netstat -tlnp; else exit 1; fi",
        ])
        .output()
        .expect("inspect listening sockets");

    let _ = child.kill();
    let _ = child.wait();

    assert!(
        output.status.success(),
        "socket inspection failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let listing = String::from_utf8_lossy(&output.stdout);
    assert!(
        !listing.contains(&format!("pid={},", child.id()))
            && !listing.contains(&format!("pid={}", child.id())),
        "dashboard process bound a listening socket:\n{listing}"
    );
}

#[test]
fn test_observability_subcommands_readonly() {
    let env = DashboardEnv::new();
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "readonly-1",
            "drawer one",
            "default",
            Some("scope"),
            "1713000000",
            1,
        ),
    );
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "readonly-2",
            "drawer two",
            "agent-diary",
            Some("scope"),
            "1713000010",
            2,
        ),
    );
    record_novelty_audit(
        &env.db_path,
        "readonly-1",
        NoveltyAction::Insert,
        1_713_000_100,
    );
    seed_queue_rows(&env.db_path);

    let before = snapshot_db(&env.db_path);
    for args in [
        vec!["tail"],
        vec!["timeline", "--since", "7d"],
        vec!["timeline", "--format", "json"],
        vec!["stats"],
        vec!["view", "readonly-1"],
        vec!["audit", "--kind", "novelty", "--since", "7d"],
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
    assert!(
        !env.home.join(".mempal/audit.jsonl").exists(),
        "dashboard subcommands must not emit audit log side effects"
    );
}

#[test]
fn test_stats_shows_all_sections() {
    let env = DashboardEnv::new();
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "stats-1",
            "drawer one",
            "default",
            Some("core"),
            "1713000000",
            3,
        ),
    );
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "stats-2",
            "drawer two",
            "agent-diary",
            Some("codex"),
            "1713000010",
            4,
        ),
    );
    seed_queue_rows(&env.db_path);
    record_gating_audit(&env.db_path, "stats-1", true);
    record_novelty_audit(&env.db_path, "stats-2", NoveltyAction::Merge, 1_713_000_200);

    let output = run_mempal(&env.home, env.cwd(), &["stats"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("schema_version:"), "{stdout}");
    assert!(stdout.contains("fork_ext_version:"), "{stdout}");
    assert!(stdout.contains("queue:"), "{stdout}");
    assert!(stdout.contains("  heartbeat:"), "{stdout}");
    assert!(stdout.contains("gating:"), "{stdout}");
    assert!(stdout.contains("novelty:"), "{stdout}");
    assert!(stdout.contains("privacy scrub:"), "{stdout}");
    assert!(stdout.contains("drawers total: 2"), "{stdout}");
}

#[test]
fn test_tail_default_prints_recent_20() {
    let env = DashboardEnv::new();
    for index in 0..25 {
        let content = if index == 24 {
            "latest \u{1b}[31mred\u{1b}[0m decision".to_string()
        } else {
            format!("tail drawer {index}")
        };
        insert_drawer(
            &env.db_path,
            &drawer_seed(
                format!("tail-{index:02}"),
                content,
                "default",
                Some("tail"),
                format!("{}", 1_713_000_000 + index),
                1,
            ),
        );
    }

    let output = run_mempal(&env.home, env.cwd(), &["tail"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 20, "{stdout}");
    assert!(lines[0].contains("tail-24"), "{stdout}");
    assert!(lines[19].contains("tail-05"), "{stdout}");
    assert!(
        !stdout.contains('\u{1b}'),
        "tail rendered raw ANSI:\n{stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_tail_follow_coalesces_event_storm() {
    let env = DashboardEnv::new();
    for index in 0..20 {
        insert_drawer(
            &env.db_path,
            &drawer_seed(
                format!("storm-{index:02}"),
                format!("storm drawer {index}"),
                "default",
                Some("follow"),
                format!("{}", 1_713_001_000 + index),
                1,
            ),
        );
    }

    let db = Database::open_read_only(&env.db_path).expect("open readonly db");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    for _ in 0..120 {
        tx.send(TailFollowEvent::Notify).expect("send notify");
    }

    let started = Instant::now();
    let batch = observability::collect_tail_follow_batch(
        &db,
        &ProjectSearchScope::all_projects(),
        TailFollowFilters {
            wing: None,
            room: None,
            since_cutoff: None,
            raw: false,
        },
        0,
        &mut rx,
    )
    .await
    .expect("collect follow batch");

    assert_eq!(batch.wake, TailFollowWake::Notify);
    assert_eq!(batch.lines.len(), 20, "{batch:?}");
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "event storm was not debounced: {:?}",
        started.elapsed()
    );
    assert!(
        batch
            .lines
            .first()
            .is_some_and(|line| line.contains("storm-00"))
    );
    assert!(
        batch
            .lines
            .last()
            .is_some_and(|line| line.contains("storm-19"))
    );
}

#[test]
fn test_tail_follow_sees_new_drawers() {
    let env = DashboardEnv::new();
    let mut child = spawn_mempal(&env.home, env.cwd(), &["tail", "--follow", "--limit", "0"]);
    std::thread::sleep(Duration::from_millis(300));

    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "follow-new",
            "fresh follow drawer",
            "default",
            Some("follow"),
            "1713002000",
            2,
        ),
    );

    let (lines, _) = read_lines_until(&mut child, 1, Duration::from_secs(5));
    let _ = child.kill();
    let _ = child.wait();

    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("follow-new"), "{lines:?}");
}

#[test]
fn test_tail_wing_filter() {
    let env = DashboardEnv::new();
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "wing-agent-1",
            "agent diary entry",
            "agent-diary",
            Some("claude"),
            "1713000000",
            2,
        ),
    );
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "wing-default-1",
            "default wing entry",
            "default",
            Some("room"),
            "1713000010",
            2,
        ),
    );

    let output = run_mempal(&env.home, env.cwd(), &["tail", "--wing", "agent-diary"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("wing-agent-1"), "{stdout}");
    assert!(!stdout.contains("wing-default-1"), "{stdout}");
}

#[test]
fn test_timeline_groups_by_day() {
    let env = DashboardEnv::new();
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "timeline-1",
            "day one",
            "default",
            Some("room"),
            unix_secs_days_ago(2),
            1,
        ),
    );
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "timeline-2",
            "day two",
            "default",
            Some("room"),
            unix_secs_days_ago(1),
            3,
        ),
    );
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "timeline-3",
            "day three",
            "default",
            Some("room"),
            unix_secs_days_ago(0),
            2,
        ),
    );

    let output = run_mempal(&env.home, env.cwd(), &["timeline", "--since", "7d"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("=== {} ===", day_label_days_ago(2))),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("=== {} ===", day_label_days_ago(1))),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!("=== {} ===", day_label_days_ago(0))),
        "{stdout}"
    );
}

#[test]
fn test_timeline_json_format_is_valid_ndjson() {
    let env = DashboardEnv::new();
    for index in 0..5 {
        insert_drawer(
            &env.db_path,
            &drawer_seed(
                format!("json-{index}"),
                format!("Decision {index} with control \u{1b}[31mred\u{1b}[0m"),
                "default",
                Some("timeline"),
                format!("{}", 1_713_000_000 + index),
                index + 1,
            ),
        );
    }

    let output = run_mempal(&env.home, env.cwd(), &["timeline", "--format", "json"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines = stdout
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect::<Vec<_>>();
    assert_eq!(lines.len(), 5, "{stdout}");
    for line in lines {
        let value = serde_json::from_str::<serde_json::Value>(line).expect("valid ndjson row");
        assert!(value.get("timestamp").is_some(), "{value}");
        assert!(value.get("drawer_id").is_some(), "{value}");
        assert!(value.get("wing").is_some(), "{value}");
        assert!(value.get("room").is_some(), "{value}");
        assert!(value.get("importance_stars").is_some(), "{value}");
    }
}

#[test]
fn test_view_prints_full_drawer() {
    let env = DashboardEnv::new();
    let content = "Decision: use CLI dashboard \u{1b}[31mRED\u{1b}[0m";
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "view-full",
            content,
            "default",
            Some("view"),
            "1713000000",
            4,
        ),
    );

    let output = run_mempal(&env.home, env.cwd(), &["view", "view-full"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("drawer_id: view-full"), "{stdout}");
    assert!(stdout.contains("scope: default/view"), "{stdout}");
    assert!(stdout.contains("created_at:"), "{stdout}");
    assert!(stdout.contains("content_truncated: false"), "{stdout}");
    assert!(stdout.contains("original_content_bytes:"), "{stdout}");
    assert!(stdout.contains("Decision: use CLI dashboard"), "{stdout}");
    assert!(
        !stdout.contains('\u{1b}'),
        "view rendered raw ANSI:\n{stdout}"
    );
}

#[test]
fn test_view_raw_is_verbatim() {
    let env = DashboardEnv::new();
    let content = "Decision: keep raw \u{1b}[31mRED\u{1b}[0m bytes";
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "view-raw",
            content,
            "default",
            Some("view"),
            "1713000000",
            4,
        ),
    );

    let output = run_mempal(&env.home, env.cwd(), &["view", "view-raw", "--raw"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains(content), "{stdout}");
    assert!(
        stdout.contains('\u{1b}'),
        "raw mode lost ANSI bytes:\n{stdout}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn test_tail_follow_fallback_on_inotify_silence() {
    let env = DashboardEnv::new();
    let db = Database::open_read_only(&env.db_path).expect("open readonly db");
    let (_tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let db_path = env.db_path.clone();

    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(200));
        insert_drawer(
            &db_path,
            &drawer_seed(
                "fallback-new",
                "drawer visible after timeout tick",
                "default",
                Some("follow"),
                "1713004000",
                3,
            ),
        );
    });

    let started = Instant::now();
    let batch = observability::collect_tail_follow_batch(
        &db,
        &ProjectSearchScope::all_projects(),
        TailFollowFilters {
            wing: None,
            room: None,
            since_cutoff: None,
            raw: false,
        },
        0,
        &mut rx,
    )
    .await
    .expect("collect fallback batch");

    assert_eq!(batch.wake, TailFollowWake::Tick, "{batch:?}");
    assert!(
        started.elapsed() >= Duration::from_millis(3000),
        "timeout fallback fired too early: {:?}",
        started.elapsed()
    );
    assert_eq!(batch.lines.len(), 1, "{batch:?}");
    assert!(batch.lines[0].contains("fallback-new"), "{batch:?}");
}

#[test]
fn test_tail_strips_hooks_raw_sentinel_from_preview() {
    let env = DashboardEnv::new();
    let content = append_hooks_raw_metadata(
        "hook envelope body remains visible",
        Some("sess-hook-tail"),
        Some("1713000100"),
    );
    insert_drawer(
        &env.db_path,
        &drawer_seed(
            "hooks-raw-preview",
            content,
            "hooks-raw",
            Some("Bash"),
            "1713005000",
            2,
        ),
    );

    let output = run_mempal(&env.home, env.cwd(), &["tail"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("<!-- mempal:hooks-raw -->"),
        "non-raw tail leaked hooks metadata:\n{stdout}"
    );
    assert!(
        !stdout.contains("session_id:"),
        "non-raw tail leaked session metadata:\n{stdout}"
    );
    assert!(
        stdout.contains("hook envelope body remains visible"),
        "{stdout}"
    );

    let raw_output = run_mempal(&env.home, env.cwd(), &["tail", "--raw"]);
    assert!(
        raw_output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&raw_output.stderr)
    );
    let raw_stdout = String::from_utf8_lossy(&raw_output.stdout);
    assert!(
        raw_stdout.contains("<!-- mempal:hooks-raw -->"),
        "{raw_stdout}"
    );
}

#[test]
fn test_tail_respects_strict_project_isolation() {
    let env = DashboardEnv::new();
    env.set_project_scope(Some("proj-A"), true);
    insert_drawer_with_project(
        &env.db_path,
        &drawer_seed(
            "tail-proj-a",
            "project A visible",
            "default",
            Some("tail"),
            "1713006002",
            2,
        ),
        Some("proj-A"),
    );
    insert_drawer_with_project(
        &env.db_path,
        &drawer_seed(
            "tail-proj-b",
            "project B hidden",
            "default",
            Some("tail"),
            "1713006001",
            2,
        ),
        Some("proj-B"),
    );
    insert_drawer_with_project(
        &env.db_path,
        &drawer_seed(
            "tail-global",
            "global hidden",
            "default",
            Some("tail"),
            "1713006000",
            2,
        ),
        None,
    );

    let output = run_mempal(&env.home, env.cwd(), &["tail"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("tail-proj-a"), "{stdout}");
    assert!(!stdout.contains("tail-proj-b"), "{stdout}");
    assert!(!stdout.contains("tail-global"), "{stdout}");
}

#[test]
fn test_stats_per_project_breakdown() {
    let env = DashboardEnv::new();
    insert_drawer_with_project(
        &env.db_path,
        &drawer_seed(
            "stats-proj-a-1",
            "stats project a one",
            "default",
            Some("stats"),
            "1713007000",
            1,
        ),
        Some("proj-A"),
    );
    insert_drawer_with_project(
        &env.db_path,
        &drawer_seed(
            "stats-proj-a-2",
            "stats project a two",
            "default",
            Some("stats"),
            "1713007001",
            1,
        ),
        Some("proj-A"),
    );
    insert_drawer_with_project(
        &env.db_path,
        &drawer_seed(
            "stats-proj-b-1",
            "stats project b one",
            "default",
            Some("stats"),
            "1713007002",
            1,
        ),
        Some("proj-B"),
    );
    insert_drawer_with_project(
        &env.db_path,
        &drawer_seed(
            "stats-global-1",
            "stats global one",
            "default",
            Some("stats"),
            "1713007003",
            1,
        ),
        None,
    );

    let output = run_mempal(&env.home, env.cwd(), &["stats"]);
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("drawers by project:"), "{stdout}");
    assert!(stdout.contains("  proj-A: 2"), "{stdout}");
    assert!(stdout.contains("  proj-B: 1"), "{stdout}");
    assert!(stdout.contains("  NULL: 1"), "{stdout}");
}

#[test]
fn test_audit_filters_by_project_when_isolation_strict() {
    let env = DashboardEnv::new();
    env.set_project_scope(Some("proj-A"), true);
    insert_drawer_with_project(
        &env.db_path,
        &drawer_seed(
            "audit-proj-a",
            "audit project a",
            "default",
            Some("audit"),
            "1713008000",
            2,
        ),
        Some("proj-A"),
    );
    insert_drawer_with_project(
        &env.db_path,
        &drawer_seed(
            "audit-proj-b",
            "audit project b",
            "default",
            Some("audit"),
            "1713008001",
            2,
        ),
        Some("proj-B"),
    );
    record_novelty_audit(
        &env.db_path,
        "audit-proj-a",
        NoveltyAction::Insert,
        now_unix_secs(),
    );
    record_novelty_audit(
        &env.db_path,
        "audit-proj-b",
        NoveltyAction::Drop,
        now_unix_secs(),
    );

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
    assert!(stdout.contains("audit-proj-a"), "{stdout}");
    assert!(!stdout.contains("audit-proj-b"), "{stdout}");
}
