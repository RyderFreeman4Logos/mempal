use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use serde_json::Value;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time after unix epoch")
        .as_secs() as i64
}

struct PrimeEnv {
    _tmp: TempDir,
    home: PathBuf,
    db_path: PathBuf,
    foo_project: PathBuf,
}

impl PrimeEnv {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().join("home");
        let mempal_home = home.join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        fs::write(
            mempal_home.join("config.toml"),
            format!(
                r#"
db_path = "{}"

[embed]
backend = "model2vec"

[search]
strict_project_isolation = false
"#,
                db_path.display()
            ),
        )
        .expect("write config");

        let foo_project = home.join("foo");
        let bar_project = home.join("bar");
        fs::create_dir_all(&foo_project).expect("create foo project");
        fs::create_dir_all(&bar_project).expect("create bar project");

        Self {
            _tmp: tmp,
            home,
            db_path,
            foo_project,
        }
    }

    fn run(&self, cwd: &Path, args: &[&str]) -> std::process::Output {
        Command::new(mempal_bin())
            .args(args)
            .env("HOME", &self.home)
            .current_dir(cwd)
            .output()
            .expect("run mempal")
    }

    fn run_with_env(
        &self,
        cwd: &Path,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> std::process::Output {
        let mut command = Command::new(mempal_bin());
        command.args(args).env("HOME", &self.home).current_dir(cwd);
        for (key, value) in extra_env {
            command.env(key, value);
        }
        command.output().expect("run mempal with env")
    }
}

struct DrawerSeed {
    id: String,
    content: String,
    wing: String,
    room: Option<String>,
    added_at: i64,
    importance: i32,
    project_id: Option<String>,
}

fn insert_drawer(db_path: &Path, seed: DrawerSeed) {
    let db = Database::open(db_path).expect("open db");
    db.insert_drawer_with_project(
        &Drawer {
            id: seed.id.to_string(),
            content: seed.content,
            wing: seed.wing,
            room: seed.room,
            source_file: Some(format!("{}.md", seed.id)),
            source_type: SourceType::Manual,
            added_at: seed.added_at.to_string(),
            chunk_index: Some(0),
            importance: seed.importance,
            ..Drawer::default()
        },
        seed.project_id.as_deref(),
    )
    .expect("insert drawer");
}

fn utf8_char_count(value: &str) -> usize {
    value.chars().count()
}

#[test]
fn test_prime_default_output_has_three_blocks() {
    let env = PrimeEnv::new();
    let now = now_secs();
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "drawer-hi".to_string(),
            content: "Highest value project decision with enough detail to appear first."
                .to_string(),
            wing: "decisions".to_string(),
            room: Some("core".to_string()),
            added_at: now - 60,
            importance: 5,
            project_id: Some("foo".to_string()),
        },
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "drawer-mid".to_string(),
            content: "Secondary note about implementation sequencing.".to_string(),
            wing: "notes".to_string(),
            room: Some("impl".to_string()),
            added_at: now - 30,
            importance: 3,
            project_id: Some("foo".to_string()),
        },
    );
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "drawer-low".to_string(),
            content: "Low priority background context.".to_string(),
            wing: "misc".to_string(),
            room: Some("archive".to_string()),
            added_at: now - 10,
            importance: 1,
            project_id: Some("foo".to_string()),
        },
    );

    let output = env.run(&env.foo_project, &["prime"]);
    assert!(output.status.success(), "{output:?}");

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("Legend:"), "{stdout}");
    assert!(stdout.contains("\nTimeline:\n"), "{stdout}");
    assert!(stdout.contains("\nStats:\n"), "{stdout}");

    let timeline = stdout
        .split("Timeline:\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\nStats:\n").next())
        .expect("timeline block");
    let first_line = timeline.lines().next().expect("first timeline line");
    assert!(first_line.contains("drawer-hi"), "{stdout}");
}

#[test]
fn test_prime_empty_db_silent() {
    let env = PrimeEnv::new();

    let output = env.run(&env.foo_project, &["prime"]);
    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
}

#[test]
fn test_prime_missing_db_exits_zero() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let cwd = tmp.path().join("project");
    fs::create_dir_all(&cwd).expect("create cwd");

    let output = Command::new(mempal_bin())
        .arg("prime")
        .env("HOME", &home)
        .current_dir(&cwd)
        .output()
        .expect("run mempal prime");

    assert!(output.status.success(), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    let stderr = String::from_utf8(output.stderr).expect("stderr utf8");
    assert!(
        stderr.contains("mempal: palace.db not found; skipping priming"),
        "{stderr}"
    );
}

#[test]
fn test_prime_json_format_valid_schema() {
    let env = PrimeEnv::new();
    let now = now_secs();
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "drawer-json".to_string(),
            content: "多语言 UTF-8 内容需要安全裁剪。This sentence is intentionally long so the preview path is exercised without involving ANSI output.".to_string(),
            wing: "notes".to_string(),
            room: Some("json".to_string()),
            added_at: now - 30,
            importance: 4,
            project_id: Some("foo".to_string()),
        },
    );

    let output = env.run(&env.foo_project, &["prime", "--format", "json"]);
    assert!(output.status.success(), "{output:?}");

    let value: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    for key in [
        "project_id",
        "generated_at",
        "legend",
        "drawers",
        "stats",
        "budget_used_tokens",
        "truncated",
    ] {
        assert!(value.get(key).is_some(), "missing key {key}: {value}");
    }
    let previews = value["drawers"].as_array().expect("drawers array");
    assert_eq!(previews.len(), 1);
    let preview = previews[0]["preview"].as_str().expect("preview string");
    assert!(utf8_char_count(preview) <= 120, "{preview}");
}

#[test]
fn test_prime_token_budget_truncates_by_importance() {
    let env = PrimeEnv::new();
    let now = now_secs();
    let long_tail =
        "Important context repeated to consume budget without ANSI escapes. ".repeat(10);

    for index in 0..8 {
        insert_drawer(
            &env.db_path,
            DrawerSeed {
                id: format!("p5-{index}"),
                content: format!("P5 {index}: {long_tail}"),
                wing: "decisions".to_string(),
                room: Some("top".to_string()),
                added_at: now - index,
                importance: 5,
                project_id: Some("foo".to_string()),
            },
        );
    }
    for index in 0..8 {
        insert_drawer(
            &env.db_path,
            DrawerSeed {
                id: format!("p4-{index}"),
                content: format!("P4 {index}: {long_tail}"),
                wing: "notes".to_string(),
                room: Some("mid".to_string()),
                added_at: now - 100 - index,
                importance: 4,
                project_id: Some("foo".to_string()),
            },
        );
    }
    for index in 0..30 {
        insert_drawer(
            &env.db_path,
            DrawerSeed {
                id: format!("p3-{index}"),
                content: format!("P3 {index}: {long_tail}"),
                wing: "archive".to_string(),
                room: Some("low".to_string()),
                added_at: now - 200 - index,
                importance: 3,
                project_id: Some("foo".to_string()),
            },
        );
    }

    let output = env.run(
        &env.foo_project,
        &["prime", "--format", "json", "--token-budget", "512"],
    );
    assert!(output.status.success(), "{output:?}");

    let value: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let drawers = value["drawers"].as_array().expect("drawers array");
    assert!(!drawers.is_empty(), "{value}");
    assert!(value["truncated"].as_bool().unwrap_or(false), "{value}");
    assert!(
        value["budget_used_tokens"]
            .as_u64()
            .is_some_and(|tokens| tokens <= 512),
        "{value}"
    );
    assert!(
        drawers.iter().all(|drawer| {
            drawer["importance_stars"]
                .as_i64()
                .is_some_and(|importance| importance >= 4)
        }),
        "{value}"
    );
}

#[test]
fn test_prime_runs_when_embedder_degraded() {
    let env = PrimeEnv::new();
    let now = now_secs();
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "drawer-degraded".to_string(),
            content: "Priming must keep working when the embedder is degraded.".to_string(),
            wing: "ops".to_string(),
            room: Some("recovery".to_string()),
            added_at: now - 10,
            importance: 4,
            project_id: Some("foo".to_string()),
        },
    );

    let output = env.run_with_env(
        &env.foo_project,
        &["prime", "--format", "json"],
        &[("MEMPAL_TEST_EMBED_DEGRADED", "1")],
    );
    assert!(output.status.success(), "{output:?}");

    let value: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    assert_eq!(value["stats"]["embedder_status"], "degraded");
    assert_eq!(value["drawers"].as_array().map(Vec::len), Some(1));
}

#[test]
fn test_prime_project_id_overrides_cwd() {
    let env = PrimeEnv::new();
    let now = now_secs();
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "foo-only".to_string(),
            content: "Only for foo project.".to_string(),
            wing: "notes".to_string(),
            room: Some("foo".to_string()),
            added_at: now - 30,
            importance: 5,
            project_id: Some("foo".to_string()),
        },
    );
    for index in 0..3 {
        insert_drawer(
            &env.db_path,
            DrawerSeed {
                id: format!("bar-{index}"),
                content: format!("Bar project drawer {index}"),
                wing: "notes".to_string(),
                room: Some("bar".to_string()),
                added_at: now - index,
                importance: 4,
                project_id: Some("bar".to_string()),
            },
        );
    }

    let output = env.run(
        &env.foo_project,
        &["prime", "--format", "json", "--project-id", "bar"],
    );
    assert!(output.status.success(), "{output:?}");

    let value: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let drawers = value["drawers"].as_array().expect("drawers array");
    assert_eq!(drawers.len(), 3, "{value}");
    assert!(drawers.iter().all(|drawer| {
        drawer["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("bar-"))
    }));
}

#[test]
fn test_prime_since_filter() {
    let env = PrimeEnv::new();
    let now = now_secs();
    for index in 0..5 {
        insert_drawer(
            &env.db_path,
            DrawerSeed {
                id: format!("recent-{index}"),
                content: format!("Recent drawer {index}"),
                wing: "notes".to_string(),
                room: Some("recent".to_string()),
                added_at: now - (index * 60),
                importance: 4,
                project_id: Some("foo".to_string()),
            },
        );
    }
    for index in 0..5 {
        insert_drawer(
            &env.db_path,
            DrawerSeed {
                id: format!("old-{index}"),
                content: format!("Old drawer {index}"),
                wing: "archive".to_string(),
                room: Some("old".to_string()),
                added_at: now - (30 * 24 * 60 * 60) - index,
                importance: 2,
                project_id: Some("foo".to_string()),
            },
        );
    }

    let output = env.run(
        &env.foo_project,
        &["prime", "--format", "json", "--since", "7d"],
    );
    assert!(output.status.success(), "{output:?}");

    let value: Value = serde_json::from_slice(&output.stdout).expect("valid json");
    let drawers = value["drawers"].as_array().expect("drawers array");
    assert_eq!(drawers.len(), 5, "{value}");
    assert_eq!(value["stats"]["recent_7d"], 5);
    assert!(drawers.iter().all(|drawer| {
        drawer["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("recent-"))
    }));
}

#[test]
fn test_prime_output_no_ansi_escapes() {
    let env = PrimeEnv::new();
    let now = now_secs();
    insert_drawer(
        &env.db_path,
        DrawerSeed {
            id: "drawer-plain".to_string(),
            content: "Output must stay plain text without terminal escape sequences.".to_string(),
            wing: "notes".to_string(),
            room: Some("plain".to_string()),
            added_at: now - 1,
            importance: 5,
            project_id: Some("foo".to_string()),
        },
    );

    let output = env.run(&env.foo_project, &["prime"]);
    assert!(output.status.success(), "{output:?}");

    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(!stdout.contains("\u{1b}["), "{stdout}");
}
