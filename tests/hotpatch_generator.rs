use std::fs;
use std::path::{Path, PathBuf};

use mempal::core::config::Config;
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::hotpatch::generator::{GenerationOptions, suggest_for_drawer};
use rusqlite::params;
use tempfile::TempDir;

struct HotpatchEnv {
    _tmp: TempDir,
    mempal_home: PathBuf,
    db_path: PathBuf,
    project_dir: PathBuf,
}

impl HotpatchEnv {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().join("home");
        let mempal_home = home.join(".mempal");
        let project_dir = tmp.path().join("workspace/project-alpha");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        fs::create_dir_all(project_dir.join("src")).expect("create project src");
        fs::write(project_dir.join("CLAUDE.md"), "# Project\n").expect("write CLAUDE");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        Self {
            _tmp: tmp,
            mempal_home,
            db_path,
            project_dir,
        }
    }

    fn config(&self, enabled: bool, extra: &str) -> Config {
        let config_text = format!(
            r#"
db_path = "{}"

[project]
id = "project-alpha"

[search]
strict_project_isolation = true

[hotpatch]
enabled = {}
min_importance_stars = 4
watch_files = ["CLAUDE.md", "AGENTS.md", "GEMINI.md"]
max_suggestion_length = 50
allowed_target_prefixes = ["{}"]
{}
"#,
            self.db_path.display(),
            enabled,
            self.project_dir.parent().expect("workspace").display(),
            extra
        );
        Config::parse(&config_text).expect("parse config")
    }

    fn insert_drawer(
        &self,
        drawer_id: &str,
        content: &str,
        importance: i32,
        source_file: &Path,
        project_id: Option<&str>,
    ) {
        let db = Database::open(&self.db_path).expect("open db");
        db.insert_drawer(&Drawer {
            id: drawer_id.to_string(),
            content: content.to_string(),
            wing: "hooks-raw".to_string(),
            room: Some("Edit".to_string()),
            source_file: Some(source_file.display().to_string()),
            source_type: SourceType::Manual,
            added_at: "1713000000".to_string(),
            chunk_index: Some(0),
            importance,
        })
        .expect("insert drawer");
        db.conn()
            .execute(
                "UPDATE drawers SET project_id = ?2 WHERE id = ?1",
                params![drawer_id, project_id],
            )
            .expect("update drawer project");
    }

    fn write_hook_payload(&self, name: &str, file_path: &Path) -> PathBuf {
        let payload_path = self.mempal_home.join(format!("{name}.json"));
        let payload = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {
                "file_path": file_path.display().to_string()
            }
        });
        fs::write(&payload_path, payload.to_string()).expect("write payload");
        payload_path
    }

    fn hotpatch_file(&self) -> PathBuf {
        self.mempal_home.join("hotpatch")
    }
}

fn expected_short_drawer_id(drawer_id: &str) -> String {
    drawer_id
        .rsplit_once('_')
        .map(|(_, suffix)| suffix.to_string())
        .unwrap_or_else(|| {
            drawer_id
                .chars()
                .rev()
                .take(8)
                .collect::<String>()
                .chars()
                .rev()
                .collect()
        })
}

fn drawer_hash(db_path: &Path, drawer_id: &str) -> String {
    let db = Database::open(db_path).expect("open db");
    let details = db
        .get_drawer_details(drawer_id)
        .expect("get drawer")
        .expect("drawer exists");
    blake3::hash(details.drawer.content.as_bytes())
        .to_hex()
        .to_string()
}

#[test]
fn test_disabled_no_suggestion_generated() {
    let env = HotpatchEnv::new();
    let file_path = env.project_dir.join("src/lib.rs");
    fs::write(&file_path, "pub fn demo() {}\n").expect("write source");
    let payload_path = env.write_hook_payload("disabled", &file_path);
    env.insert_drawer(
        "drawer-disabled-0001",
        "Decision: use Arc<Mutex<>>",
        5,
        &payload_path,
        Some("project-alpha"),
    );

    let config = env.config(false, "");
    let db = Database::open(&env.db_path).expect("open db");
    let outcome = suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer-disabled-0001",
        GenerationOptions::default(),
    )
    .expect("suggest");

    assert_eq!(outcome.appended, 0);
    assert!(
        !env.hotpatch_file().exists()
            || fs::read_dir(env.hotpatch_file())
                .expect("list")
                .next()
                .is_none()
    );
}

#[test]
fn test_high_importance_drawer_generates_suggestion() {
    let env = HotpatchEnv::new();
    let file_path = env.project_dir.join("src/lib.rs");
    fs::write(&file_path, "pub fn demo() {}\n").expect("write source");
    let payload_path = env.write_hook_payload("high", &file_path);
    env.insert_drawer(
        "drawer-high-0001",
        "Decision: use Arc<Mutex<>> over RwLock for low-write path",
        5,
        &payload_path,
        Some("project-alpha"),
    );

    let config = env.config(true, "");
    let db = Database::open(&env.db_path).expect("open db");
    let outcome = suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer-high-0001",
        GenerationOptions::default(),
    )
    .expect("suggest");

    assert_eq!(outcome.appended, 1);
    let entries = fs::read_dir(env.hotpatch_file())
        .expect("list hotpatch")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect entries");
    assert_eq!(entries.len(), 1);
    let content = fs::read_to_string(entries[0].path()).expect("read suggestion file");
    assert!(content.contains("mempal hotpatch suggestions for"));
    assert!(content.contains(&format!(
        "[drawer:{}]",
        expected_short_drawer_id("drawer-high-0001")
    )));
    assert!(content.contains("decision: use Arc<Mutex<>>"));
}

#[test]
fn test_low_importance_skipped() {
    let env = HotpatchEnv::new();
    let file_path = env.project_dir.join("src/lib.rs");
    fs::write(&file_path, "pub fn demo() {}\n").expect("write source");
    let payload_path = env.write_hook_payload("low", &file_path);
    env.insert_drawer(
        "drawer-low-0001",
        "Routine note: adjusted whitespace",
        2,
        &payload_path,
        Some("project-alpha"),
    );

    let config = env.config(true, "");
    let db = Database::open(&env.db_path).expect("open db");
    let outcome = suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer-low-0001",
        GenerationOptions::default(),
    )
    .expect("suggest");

    assert_eq!(outcome.appended, 0);
    assert!(
        !env.hotpatch_file().exists()
            || fs::read_dir(env.hotpatch_file())
                .expect("list")
                .next()
                .is_none()
    );
}

#[test]
fn test_duplicate_drawer_id_not_re_appended() {
    let env = HotpatchEnv::new();
    let file_path = env.project_dir.join("src/lib.rs");
    fs::write(&file_path, "pub fn demo() {}\n").expect("write source");
    let payload_path = env.write_hook_payload("dup", &file_path);
    env.insert_drawer(
        "drawer-dup-0001",
        "Decision: keep dedup stable",
        5,
        &payload_path,
        Some("project-alpha"),
    );

    let config = env.config(true, "");
    let db = Database::open(&env.db_path).expect("open db");
    suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer-dup-0001",
        GenerationOptions::default(),
    )
    .expect("first suggest");
    let outcome = suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer-dup-0001",
        GenerationOptions::default(),
    )
    .expect("second suggest");

    let entries = fs::read_dir(env.hotpatch_file())
        .expect("list hotpatch")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect entries");
    let content = fs::read_to_string(entries[0].path()).expect("read suggestion file");
    assert_eq!(outcome.appended, 0);
    assert_eq!(
        content
            .matches(&format!(
                "[drawer:{}]",
                expected_short_drawer_id("drawer-dup-0001")
            ))
            .count(),
        1
    );
}

#[test]
fn test_distinct_drawers_with_shared_prefix_do_not_collide() {
    let env = HotpatchEnv::new();
    let file_path = env.project_dir.join("src/lib.rs");
    fs::write(&file_path, "pub fn demo() {}\n").expect("write source");
    let payload_path = env.write_hook_payload("prefix", &file_path);
    env.insert_drawer(
        "drawer_hooks_raw_edit_deadbeef",
        "Decision: first suggestion survives",
        5,
        &payload_path,
        Some("project-alpha"),
    );
    env.insert_drawer(
        "drawer_hooks_raw_edit_cafebabe",
        "Decision: second suggestion must also survive",
        5,
        &payload_path,
        Some("project-alpha"),
    );

    let config = env.config(true, "");
    let db = Database::open(&env.db_path).expect("open db");
    let first = suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer_hooks_raw_edit_deadbeef",
        GenerationOptions::default(),
    )
    .expect("first suggest");
    let second = suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer_hooks_raw_edit_cafebabe",
        GenerationOptions::default(),
    )
    .expect("second suggest");

    let entries = fs::read_dir(env.hotpatch_file())
        .expect("list hotpatch")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect entries");
    let content = fs::read_to_string(entries[0].path()).expect("read suggestion file");

    assert_eq!(first.appended, 1);
    assert_eq!(second.appended, 1);
    assert!(content.contains("[drawer:deadbeef]"));
    assert!(content.contains("[drawer:cafebabe]"));
}

#[test]
fn test_long_summary_truncated() {
    let env = HotpatchEnv::new();
    let file_path = env.project_dir.join("src/lib.rs");
    fs::write(&file_path, "pub fn demo() {}\n").expect("write source");
    let payload_path = env.write_hook_payload("long", &file_path);
    let long_line = "Decision: this summary is intentionally much longer than fifty characters so it must truncate safely.";
    env.insert_drawer(
        "drawer-long-0001",
        long_line,
        5,
        &payload_path,
        Some("project-alpha"),
    );

    let config = env.config(true, "");
    let db = Database::open(&env.db_path).expect("open db");
    suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer-long-0001",
        GenerationOptions::default(),
    )
    .expect("suggest");

    let entries = fs::read_dir(env.hotpatch_file())
        .expect("list hotpatch")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect entries");
    let content = fs::read_to_string(entries[0].path()).expect("read suggestion file");
    let line = content
        .lines()
        .find(|line| line.starts_with("- "))
        .expect("suggestion line");
    let summary = line
        .split(": ")
        .nth(1)
        .expect("topic separator")
        .split(" [drawer:")
        .next()
        .expect("summary");
    assert!(summary.chars().count() <= 51, "summary too long: {summary}");
    assert!(
        summary.ends_with('…'),
        "summary should end with ellipsis: {summary}"
    );
}

#[test]
fn test_suggestion_generation_uses_local_signals_only() {
    let generator_src = include_str!("../src/hotpatch/generator.rs");
    let manager_src = include_str!("../src/hotpatch/manager.rs");

    for forbidden in [
        "api.openai",
        "api.anthropic",
        "generativelanguage",
        "reqwest",
        "openai_compat",
    ] {
        assert!(
            !generator_src.contains(forbidden),
            "generator should stay local-only, found forbidden token {forbidden}"
        );
        assert!(
            !manager_src.contains(forbidden),
            "manager should stay local-only, found forbidden token {forbidden}"
        );
    }
}

#[test]
fn test_suggestion_respects_project_isolation() {
    let env = HotpatchEnv::new();
    let file_path = env.project_dir.join("src/lib.rs");
    fs::write(&file_path, "pub fn demo() {}\n").expect("write source");
    let payload_path = env.write_hook_payload("scoped", &file_path);
    env.insert_drawer(
        "drawer-scoped-0001",
        "Decision: scoped suggestion",
        5,
        &payload_path,
        Some("project-beta"),
    );

    let config = env.config(true, "");
    let db = Database::open(&env.db_path).expect("open db");
    let skipped = suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer-scoped-0001",
        GenerationOptions::default(),
    )
    .expect("suggest");
    assert_eq!(skipped.appended, 0);

    let allowed = suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer-scoped-0001",
        GenerationOptions { all_projects: true },
    )
    .expect("suggest all projects");
    assert_eq!(allowed.appended, 1);
}

#[test]
fn test_suggestion_source_drawer_ids_remain_raw() {
    let env = HotpatchEnv::new();
    let file_path = env.project_dir.join("src/lib.rs");
    fs::write(&file_path, "pub fn demo() {}\n").expect("write source");
    let payload_path = env.write_hook_payload("raw", &file_path);
    env.insert_drawer(
        "drawer-raw-0001",
        "Decision: keep drawers raw verbatim",
        5,
        &payload_path,
        Some("project-alpha"),
    );

    let before = drawer_hash(&env.db_path, "drawer-raw-0001");
    let config = env.config(true, "");
    let db = Database::open(&env.db_path).expect("open db");
    suggest_for_drawer(
        &db,
        &config,
        &env.mempal_home,
        "drawer-raw-0001",
        GenerationOptions::default(),
    )
    .expect("suggest");
    let after = drawer_hash(&env.db_path, "drawer-raw-0001");

    assert_eq!(before, after, "hotpatch generation must not mutate drawers");
}
