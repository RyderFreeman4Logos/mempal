use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};

use mempal::core::db::Database;
use mempal::core::types::Drawer;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

struct DeleteEnv {
    _tmp: TempDir,
    home: PathBuf,
    db_path: PathBuf,
}

impl DeleteEnv {
    fn new() -> Self {
        let tmp = TempDir::new_in("/tmp").expect("external tempdir");
        let home = tmp.path().join("home");
        let mempal_home = home.join(".mempal");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        fs::write(
            mempal_home.join("config.toml"),
            format!(
                r#"db_path = "{}"

[project]
id = "project-a"

[embed]
backend = "stub"

[search]
strict_project_isolation = true
"#,
                db_path.display()
            ),
        )
        .expect("write config");
        Self {
            _tmp: tmp,
            home,
            db_path,
        }
    }

    fn insert(&self, id: &str, project_id: &str) {
        let db = Database::open(&self.db_path).expect("open db");
        db.insert_drawer_with_project(
            &Drawer {
                id: id.to_string(),
                content: format!("synthetic content for {id}"),
                wing: "test/delete".to_string(),
                source_file: Some(format!("{id}.md")),
                ..Drawer::default()
            },
            Some(project_id),
        )
        .expect("insert drawer");
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(mempal_bin())
            .args(args)
            .env("HOME", &self.home)
            .env_remove("MEMPAL_PROJECT_ID")
            .current_dir(&self.home)
            .output()
            .expect("run mempal")
    }

    fn is_active(&self, drawer_id: &str) -> bool {
        Database::open(&self.db_path)
            .expect("open db")
            .get_drawer(drawer_id)
            .expect("load drawer")
            .is_some()
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(output.status.success(), "stderr={}", stderr(output));
}

#[test]
fn delete_exact_id_respects_project_scope_and_explicit_overrides() {
    let env = DeleteEnv::new();
    env.insert("current-project", "project-a");
    env.insert("other-project-denied", "project-b");
    env.insert("other-project-explicit", "project-b");
    env.insert("other-project-all", "project-b");

    let denied = env.run(&["delete", "other-project-denied"]);
    assert!(!denied.status.success(), "cross-project delete succeeded");
    assert!(
        stderr(&denied).contains("outside the current project scope"),
        "stderr={}",
        stderr(&denied)
    );
    assert!(env.is_active("other-project-denied"));

    let explicit = env.run(&["delete", "other-project-explicit", "--project", "project-b"]);
    assert_success(&explicit);
    assert!(!env.is_active("other-project-explicit"));

    let all_projects = env.run(&["delete", "other-project-all", "--all-projects"]);
    assert_success(&all_projects);
    assert!(!env.is_active("other-project-all"));

    let current = env.run(&["delete", "current-project"]);
    assert_success(&current);
    assert!(!env.is_active("current-project"));
}

#[test]
fn scoped_soft_delete_keeps_a_drawer_from_another_project() {
    let env = DeleteEnv::new();
    env.insert("scoped-drawer", "project-b");
    let db = Database::open(&env.db_path).expect("open db");

    let wrong_scope = db
        .soft_delete_drawer_in_project("scoped-drawer", Some("project-a"))
        .expect("attempt scoped delete");
    assert!(!wrong_scope);
    assert!(env.is_active("scoped-drawer"));

    let matching_scope = db
        .soft_delete_drawer_in_project("scoped-drawer", Some("project-b"))
        .expect("delete in matching scope");
    assert!(matching_scope);
    assert!(!env.is_active("scoped-drawer"));
}
