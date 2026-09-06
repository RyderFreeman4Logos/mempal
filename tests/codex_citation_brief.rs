use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

use mempal::core::anchor;
use mempal::core::db::Database;
use mempal::core::types::{AnchorKind, Drawer, MemoryDomain, MemoryKind, Provenance, SourceType};
use mempal::hook_install::install_codex;
use serde_json::{Value, json};
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_home() -> (TempDir, Database) {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_dir = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_dir).expect("create .mempal");
    let db_path = mempal_dir.join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    fs::write(
        mempal_dir.join("config.toml"),
        format!(
            r#"
db_path = "{}"

[hooks]
enabled = true

[search]
bm25_fallback = true
exclude_raw_turns = true
"#,
            db_path.display()
        ),
    )
    .expect("write config");
    (tmp, db)
}

fn git_project(root: &std::path::Path, name: &str) -> std::path::PathBuf {
    let project = root.join(name);
    fs::create_dir_all(project.join(".git")).expect("create git project");
    project
}

fn evidence(
    id: &str,
    content: &str,
    importance: i32,
    wing: &str,
    room: Option<&str>,
    pinned: bool,
    pin_order: Option<i64>,
) -> Drawer {
    Drawer {
        id: id.to_string(),
        content: content.to_string(),
        wing: wing.to_string(),
        room: room.map(ToOwned::to_owned),
        source_file: Some(format!("tests://brief/{id}")),
        source_type: SourceType::UserExplicit,
        added_at: "1710000000".to_string(),
        chunk_index: Some(0),
        normalize_version: 1,
        importance,
        memory_kind: MemoryKind::Evidence,
        domain: MemoryDomain::Project,
        field: "general".to_string(),
        anchor_kind: AnchorKind::Repo,
        anchor_id: anchor::LEGACY_REPO_ANCHOR_ID.to_string(),
        parent_anchor_id: None,
        provenance: Some(Provenance::Human),
        statement: None,
        tier: None,
        status: None,
        supporting_refs: Vec::new(),
        counterexample_refs: Vec::new(),
        teaching_refs: Vec::new(),
        verification_refs: Vec::new(),
        scope_constraints: None,
        trigger_hints: None,
        is_pinned: pinned,
        pin_order,
        supersedes: None,
        effective_importance: f64::from(importance),
        compacted_into: None,
        confidence: 1.0,
    }
}

fn run_hook_brief(
    home: &TempDir,
    cwd: &std::path::Path,
    event: &str,
    prompt: &str,
) -> std::process::Output {
    let payload = json!({
        "session_id": "s1",
        "turn_id": "t1",
        "cwd": cwd,
        "hook_event_name": event,
        "prompt": prompt,
    })
    .to_string();
    let mut child = Command::new(mempal_bin())
        .args(["hook", "brief", "--cwd-source", "stdin-json"])
        .env("HOME", home.path())
        .env("MEMPAL_EMBED_BACKEND", "stub")
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn hook brief");
    child
        .stdin
        .as_mut()
        .expect("stdin")
        .write_all(payload.as_bytes())
        .expect("write payload");
    child.wait_with_output().expect("wait hook brief")
}

fn parse_hook_json(output: &std::process::Output) -> Value {
    assert_eq!(
        output.status.code(),
        Some(0),
        "hook brief failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("valid Codex hook JSON")
}

fn additional_context<'a>(parsed: &'a Value, event: &str) -> &'a str {
    assert_eq!(parsed["hookSpecificOutput"]["hookEventName"], event);
    parsed["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .expect("additionalContext")
}

#[test]
fn test_codex_user_prompt_submit_injects_citation_first_brief() {
    let (home, db) = setup_home();
    let project = git_project(home.path(), "alpha-app");
    db.insert_drawer_with_project(
        &evidence(
            "local-decision",
            "Ship citation-first Codex brief on submit.",
            4,
            "mempal",
            Some("decision"),
            false,
            None,
        ),
        Some("alpha-app"),
    )
    .expect("insert local");

    let parsed = parse_hook_json(&run_hook_brief(
        &home,
        &project,
        "UserPromptSubmit",
        "citation-first Codex brief",
    ));
    let context = additional_context(&parsed, "UserPromptSubmit");
    assert!(context.contains("local-decision"), "{context}");
    assert!(
        context.contains("tests://brief/local-decision"),
        "{context}"
    );
    assert!(
        context.contains("Ship citation-first Codex brief on submit."),
        "{context}"
    );
}

#[test]
fn test_codex_session_start_injects_citation_first_brief() {
    let (home, db) = setup_home();
    let project = git_project(home.path(), "alpha-app");
    db.insert_drawer_with_project(
        &evidence(
            "resume-decision",
            "Resume uses the same project brief as submit.",
            4,
            "mempal",
            Some("decision"),
            false,
            None,
        ),
        Some("alpha-app"),
    )
    .expect("insert local");

    let parsed = parse_hook_json(&run_hook_brief(&home, &project, "SessionStart", "resume"));
    let context = additional_context(&parsed, "SessionStart");
    assert!(context.contains("resume-decision"), "{context}");
    assert!(
        context.contains("Resume uses the same project brief as submit."),
        "{context}"
    );
}

#[test]
fn test_codex_brief_excludes_similar_foreign_project() {
    let (home, db) = setup_home();
    let project = git_project(home.path(), "alpha-app");
    db.insert_drawer_with_project(
        &evidence(
            "local-pricing",
            "Local pricing decision stays in alpha-app.",
            4,
            "mempal",
            Some("decision"),
            false,
            None,
        ),
        Some("alpha-app"),
    )
    .expect("insert local");
    db.insert_drawer_with_project(
        &evidence(
            "foreign-pricing",
            "Foreign pricing decision belongs to alpha-app-other.",
            5,
            "mempal",
            Some("decision"),
            false,
            None,
        ),
        Some("alpha-app-other"),
    )
    .expect("insert foreign");

    let parsed = parse_hook_json(&run_hook_brief(
        &home,
        &project,
        "UserPromptSubmit",
        "pricing decision",
    ));
    let context = additional_context(&parsed, "UserPromptSubmit");
    assert!(context.contains("local-pricing"), "{context}");
    assert!(
        context.contains("Local pricing decision stays in alpha-app."),
        "{context}"
    );
    assert!(!context.contains("foreign-pricing"), "{context}");
    assert!(!context.contains("alpha-app-other"), "{context}");
}

#[test]
fn test_codex_brief_pinned_precedes_ranked_and_excludes_raw() {
    let (home, db) = setup_home();
    let project = git_project(home.path(), "alpha-app");
    db.insert_drawer_with_project(
        &evidence(
            "pinned-canonical",
            "Canonical pinned fact: never invent citations.",
            5,
            "mempal",
            Some("rule"),
            true,
            Some(0),
        ),
        Some("alpha-app"),
    )
    .expect("insert pinned");
    db.insert_drawer_with_project(
        &evidence(
            "ranked-evidence",
            "Ranked evidence about citations.",
            3,
            "mempal",
            Some("evidence"),
            false,
            None,
        ),
        Some("alpha-app"),
    )
    .expect("insert ranked");
    db.insert_drawer_with_project(
        &evidence(
            "raw-noise",
            "Low-importance raw transcript should stay excluded.",
            0,
            "hooks-raw",
            Some("user-prompt"),
            false,
            None,
        ),
        Some("alpha-app"),
    )
    .expect("insert raw");

    let parsed = parse_hook_json(&run_hook_brief(
        &home,
        &project,
        "UserPromptSubmit",
        "citations",
    ));
    let context = additional_context(&parsed, "UserPromptSubmit");
    let pinned_at = context
        .find("pinned-canonical")
        .expect("pinned citation present");
    let ranked_at = context
        .find("ranked-evidence")
        .expect("ranked citation present");
    assert!(
        pinned_at < ranked_at,
        "pinned facts must precede ranked evidence: {context}"
    );
    assert!(!context.contains("raw-noise"), "{context}");
    assert!(
        !context.contains("Low-importance raw transcript"),
        "{context}"
    );
}

#[test]
fn test_codex_brief_empty_memory_warns_without_inventing() {
    let (home, _db) = setup_home();
    let project = git_project(home.path(), "empty-app");
    let parsed = parse_hook_json(&run_hook_brief(
        &home,
        &project,
        "UserPromptSubmit",
        "what is next",
    ));
    let context = additional_context(&parsed, "UserPromptSubmit");
    assert!(
        context.to_lowercase().contains("no cited")
            || context.to_lowercase().contains("no evidence")
            || context.to_lowercase().contains("absent"),
        "empty memory must warn: {context}"
    );
    assert!(
        !context.contains("drawer: invented") && !context.contains("I remember"),
        "{context}"
    );
}

#[test]
fn test_codex_brief_transport_failure_fails_open() {
    let tmp = TempDir::new().expect("tempdir");
    let mempal_dir = tmp.path().join(".mempal");
    fs::create_dir_all(&mempal_dir).expect("create .mempal");
    let db_path = mempal_dir.join("palace.db");
    fs::write(&db_path, b"not-a-sqlite-database").expect("write corrupt db");
    fs::write(
        mempal_dir.join("config.toml"),
        format!("db_path = \"{}\"\n", db_path.display()),
    )
    .expect("write config");
    let project = git_project(tmp.path(), "broken-app");

    let output = run_hook_brief(&tmp, &project, "UserPromptSubmit", "hello");
    assert_eq!(output.status.code(), Some(0));
    assert!(
        output.stdout.is_empty(),
        "fail-open must not write diagnostics to stdout, got {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn test_codex_install_registers_brief_hook_without_replacing_drain() {
    let tmp = TempDir::new().expect("tempdir");
    let root = tmp.path().canonicalize().expect("canonical fixture root");
    let cwd = root.join("repo");
    let home = root.join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(home.join(".codex")).expect("create .codex");
    fs::write(
        home.join(".codex/hooks.json"),
        r#"{
          "hooks": {
            "UserPromptSubmit": [{
              "hooks": [{
                "type": "command",
                "command": "mempal cowork-drain --target codex --format codex-hook-json --cwd-source stdin-json"
              }]
            }]
          }
        }"#,
    )
    .expect("seed drain hook");

    let first = install_codex(&cwd, &home, false, false).expect("install");
    let second = install_codex(&cwd, &home, false, false).expect("reinstall");
    assert!(first.changed);
    assert!(!second.changed);

    let parsed: Value =
        serde_json::from_str(&fs::read_to_string(home.join(".codex/hooks.json")).unwrap())
            .expect("parse hooks");
    let commands = |event: &str| -> Vec<String> {
        parsed["hooks"][event]
            .as_array()
            .expect("event array")
            .iter()
            .flat_map(|entry| entry["hooks"].as_array().expect("hooks").iter())
            .filter_map(|hook| hook["command"].as_str().map(ToOwned::to_owned))
            .collect()
    };
    let prompt_cmds = commands("UserPromptSubmit");
    let start_cmds = commands("SessionStart");
    assert!(
        prompt_cmds
            .iter()
            .any(|cmd| cmd.contains("hook brief") && cmd.contains("--cwd-source stdin-json")),
        "{prompt_cmds:?}"
    );
    assert!(
        start_cmds
            .iter()
            .any(|cmd| cmd.contains("hook brief") && cmd.contains("--cwd-source stdin-json")),
        "{start_cmds:?}"
    );
    assert!(prompt_cmds.iter().any(|cmd| cmd.contains("cowork-drain")));
}
