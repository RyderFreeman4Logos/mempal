use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mempal::core::db::Database;
use mempal::core::types::{
    BootstrapEvidenceArgs, Drawer, KnowledgeStatus, KnowledgeTier, MemoryKind, SourceType,
};
use tempfile::TempDir;

#[cfg(unix)]
use std::os::unix::fs as unix_fs;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn setup_home() -> (TempDir, Database, PathBuf) {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_dir = home.join(".mempal");
    fs::create_dir_all(&mempal_dir).expect("create .mempal");
    let db_path = mempal_dir.join("palace.db");
    let db = Database::open(&db_path).expect("open db");
    (tmp, db, home)
}

fn run_mempal(home: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .env("HOME", home)
        .args(args)
        .output()
        .expect("run mempal")
}

fn markdown_drawer(id: &str, content: &str) -> Drawer {
    let mut drawer = Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: content.to_string(),
        wing: "mempal".to_string(),
        room: Some("markdown".to_string()),
        source_file: Some(format!("tests://markdown/{id}.md")),
        source_type: SourceType::AgentObservation,
        added_at: "2026-06-19T12:00:00Z".to_string(),
        chunk_index: Some(0),
        importance: 4,
    });
    drawer.memory_kind = MemoryKind::Knowledge;
    drawer.statement = Some("Markdown mirror keeps SQLite canonical.".to_string());
    drawer.tier = Some(KnowledgeTier::Shu);
    drawer.status = Some(KnowledgeStatus::Promoted);
    drawer.supporting_refs = vec!["drawer-source-ref".to_string()];
    drawer
}

fn db_path(home: &Path) -> PathBuf {
    home.join(".mempal").join("palace.db")
}

fn expected_exported_drawer_path(out_dir: &Path, drawer_id: &str) -> PathBuf {
    out_dir
        .join("wing-mempal")
        .join("room-markdown")
        .join(format!(
            "{}-{}.md",
            drawer_id.replace('/', "_"),
            &blake3::hash(drawer_id.as_bytes()).to_hex()[..8]
        ))
}

fn export_all_projects(home: &Path, out_dir: &Path) -> Output {
    run_mempal(
        home,
        &[
            "export",
            "md",
            out_dir.to_str().expect("utf8 path"),
            "--all-projects",
        ],
    )
}

#[test]
fn export_md_writes_stable_frontmatter_paths_and_redacts_by_default() {
    let (_tmp, db, home) = setup_home();
    let drawer_id = "drawer/markdown-cli";
    let secret = "sk-abcdefghijklmnopqrstuvwxyz1234567890";
    let drawer = markdown_drawer(drawer_id, &format!("keep this but redact {secret}"));
    db.insert_drawer_with_project(&drawer, Some("proj-md"))
        .expect("insert drawer");
    drop(db);

    let out_dir = home.join("mirror");
    let output = export_all_projects(&home, &out_dir);
    assert!(
        output.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("canonical_source: sqlite"), "{stdout}");
    assert!(
        stdout.contains("Markdown import/watch sync is not active"),
        "{stdout}"
    );

    let exported_path = expected_exported_drawer_path(&out_dir, drawer_id);
    let markdown = fs::read_to_string(exported_path).expect("read markdown export");
    assert!(markdown.contains("mempal_format: \"markdown_mirror_v1\""));
    assert!(markdown.contains("canonical_source: \"sqlite\""));
    assert!(markdown.contains("mirror_semantics: \"generated_read_only\""));
    assert!(markdown.contains("drawer_id: \"drawer/markdown-cli\""));
    assert!(markdown.contains("project_id: \"proj-md\""));
    assert!(markdown.contains("memory_kind: \"knowledge\""));
    assert!(markdown.contains("tier: \"shu\""));
    assert!(markdown.contains("status: \"promoted\""));
    assert!(markdown.contains("source_file: \"tests://markdown/drawer/markdown-cli.md\""));
    assert!(markdown.contains("supporting_refs:\n  - \"drawer-source-ref\""));
    assert!(!markdown.contains(secret), "{markdown}");
    assert!(markdown.contains("[REDACTED:openai_key]"), "{markdown}");

    let manifest =
        fs::read_to_string(out_dir.join(".mempal-markdown-mirror.toml")).expect("read manifest");
    assert!(manifest.contains("mempal_format = \"markdown_mirror_v1\""));
    assert!(manifest.contains("generated_files = ["));
    assert!(manifest.contains("README.md"));
    assert!(manifest.contains("index.md"));
    assert!(manifest.contains("wing-mempal/room-markdown/drawer_markdown-cli-"));
}

#[test]
fn export_md_refuses_unmanaged_non_empty_output_dir() {
    let (_tmp, db, home) = setup_home();
    db.insert_drawer_with_project(
        &markdown_drawer("drawer/refuse", "do not overwrite user files"),
        Some("proj-md"),
    )
    .expect("insert drawer");
    drop(db);

    let out_dir = home.join("mirror");
    fs::create_dir_all(&out_dir).expect("create mirror");
    let readme = out_dir.join("README.md");
    fs::write(&readme, "user-owned readme").expect("write user readme");

    let output = export_all_projects(&home, &out_dir);
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("refusing to export into non-empty unmanaged markdown mirror directory"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(&readme).expect("read user readme"),
        "user-owned readme"
    );
    assert!(!out_dir.join(".mempal-markdown-mirror.toml").exists());
}

#[test]
fn export_md_refuses_unmanaged_collision_inside_existing_mirror() {
    let (_tmp, db, home) = setup_home();
    db.insert_drawer_with_project(
        &markdown_drawer("drawer/kept", "kept generated file"),
        Some("proj-md"),
    )
    .expect("insert kept drawer");
    drop(db);

    let out_dir = home.join("mirror");
    let first = export_all_projects(&home, &out_dir);
    assert!(
        first.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let collision_id = "drawer/collision";
    let collision_path = expected_exported_drawer_path(&out_dir, collision_id);
    fs::create_dir_all(collision_path.parent().expect("collision parent"))
        .expect("create collision parent");
    fs::write(&collision_path, "user-owned collision").expect("write collision");

    let db = Database::open(&db_path(&home)).expect("reopen db");
    db.insert_drawer_with_project(
        &markdown_drawer(collision_id, "new drawer should not overwrite user file"),
        Some("proj-md"),
    )
    .expect("insert collision drawer");
    drop(db);

    let second = export_all_projects(&home, &out_dir);
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("refusing to overwrite unmanaged markdown export file"),
        "{stderr}"
    );
    assert_eq!(
        fs::read_to_string(&collision_path).expect("read collision file"),
        "user-owned collision"
    );
}

#[test]
fn export_md_rerun_removes_stale_generated_files_without_deleting_user_files() {
    let (_tmp, db, home) = setup_home();
    let stale_id = "drawer/stale";
    let kept_id = "drawer/kept";
    db.insert_drawer_with_project(&markdown_drawer(stale_id, "stale body"), Some("proj-md"))
        .expect("insert stale drawer");
    db.insert_drawer_with_project(&markdown_drawer(kept_id, "kept body"), Some("proj-md"))
        .expect("insert kept drawer");
    drop(db);

    let out_dir = home.join("mirror");
    let first = export_all_projects(&home, &out_dir);
    assert!(
        first.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let stale_path = expected_exported_drawer_path(&out_dir, stale_id);
    let kept_path = expected_exported_drawer_path(&out_dir, kept_id);
    assert!(stale_path.exists());
    assert!(kept_path.exists());

    let user_file = out_dir
        .join("wing-mempal")
        .join("room-markdown")
        .join("user-note.md");
    fs::write(&user_file, "user-owned note").expect("write user note");

    let db = Database::open(&db_path(&home)).expect("reopen db");
    assert!(db.soft_delete_drawer(stale_id).expect("soft delete stale"));
    drop(db);

    let second = export_all_projects(&home, &out_dir);
    assert!(
        second.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&second.stdout),
        String::from_utf8_lossy(&second.stderr)
    );

    assert!(!stale_path.exists(), "stale generated file remained");
    assert!(kept_path.exists(), "current generated file missing");
    assert_eq!(
        fs::read_to_string(&user_file).expect("read user note"),
        "user-owned note"
    );

    let stale_relative = stale_path
        .strip_prefix(&out_dir)
        .expect("relative stale path")
        .to_string_lossy()
        .replace('\\', "/");
    let kept_relative = kept_path
        .strip_prefix(&out_dir)
        .expect("relative kept path")
        .to_string_lossy()
        .replace('\\', "/");
    let manifest =
        fs::read_to_string(out_dir.join(".mempal-markdown-mirror.toml")).expect("read manifest");
    assert!(!manifest.contains(&stale_relative), "{manifest}");
    assert!(manifest.contains(&kept_relative), "{manifest}");
}

#[cfg(unix)]
#[test]
fn export_md_refuses_symlinked_generated_parent() {
    let (_tmp, db, home) = setup_home();
    let drawer_id = "drawer/symlink-parent";
    db.insert_drawer_with_project(
        &markdown_drawer(drawer_id, "must stay inside mirror"),
        Some("proj-md"),
    )
    .expect("insert drawer");
    drop(db);

    let out_dir = home.join("mirror");
    let first = export_all_projects(&home, &out_dir);
    assert!(
        first.status.success(),
        "stdout={}\nstderr={}",
        String::from_utf8_lossy(&first.stdout),
        String::from_utf8_lossy(&first.stderr)
    );

    let generated_parent = out_dir.join("wing-mempal");
    fs::remove_dir_all(&generated_parent).expect("remove generated parent");
    let outside = home.join("outside");
    fs::create_dir_all(&outside).expect("create outside dir");
    unix_fs::symlink(&outside, &generated_parent).expect("symlink generated parent");

    let second = export_all_projects(&home, &out_dir);
    assert!(!second.status.success());
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("refusing to use symlinked markdown export directory"),
        "{stderr}"
    );
    assert!(
        !outside.join("room-markdown").exists(),
        "export escaped through symlinked parent"
    );
}

#[test]
fn export_md_help_and_docs_explain_sqlite_canonical_semantics() {
    let (_tmp, _db, home) = setup_home();
    let output = run_mempal(&home, &["export", "md", "--help"]);
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("SQLite remains the source of truth"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Import/watch sync is intentionally not active"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Non-empty unmanaged directories are refused"),
        "{stdout}"
    );

    let docs = fs::read_to_string("docs/markdown-mirror.md").expect("read docs");
    assert!(docs.contains("SQLite remains the canonical memory store"));
    assert!(docs.contains("non-empty directory"));
    assert!(docs.contains("mempal mirror manifest"));
    assert!(docs.contains("SQLite wins by default"));
    assert!(docs.contains("Future import or watch behavior must be opt-in"));
}
