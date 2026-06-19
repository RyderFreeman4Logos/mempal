use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use mempal::core::db::Database;
use mempal::core::types::{BootstrapEvidenceArgs, Drawer, SourceType, Triple};
use mempal::core::utils::build_triple_id;
use serde_json::Value;
use tempfile::TempDir;

const NOW: u64 = 1_800_000_000;
const NOW_RFC3339: &str = "2027-01-15T08:00:00Z";

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

struct CliEnv {
    _tmp: TempDir,
    home: PathBuf,
    db_path: PathBuf,
}

impl CliEnv {
    fn new_with_remote_embed_config() -> Self {
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
backend = "openai_compat"

[embed.openai_compat]
base_url = "http://127.0.0.1:9/v1"
model = "test-embedder"
dim = 384
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

    fn db(&self) -> Database {
        Database::open(&self.db_path).expect("open db")
    }

    fn run(&self, args: &[&str]) -> std::process::Output {
        Command::new(mempal_bin())
            .args(args)
            .env("HOME", &self.home)
            .current_dir(&self.home)
            .output()
            .expect("run mempal")
    }
}

fn drawer(id: &str, content: &str, wing: &str, room: Option<&str>) -> Drawer {
    Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: content.to_string(),
        wing: wing.to_string(),
        room: room.map(str::to_string),
        source_file: Some(format!("{id}.md")),
        source_type: SourceType::AgentInference,
        added_at: NOW.to_string(),
        chunk_index: Some(0),
        importance: 3,
    })
}

fn seed_reflection_findings(db: &Database) {
    db.insert_drawer(&drawer("dup-a", "same memory", "alpha", Some("shared")))
        .expect("insert dup a");
    db.insert_drawer(&drawer("dup-b", "same memory", "alpha", Some("shared")))
        .expect("insert dup b");
    db.insert_drawer(&drawer(
        "tunnel-b",
        "different memory",
        "beta",
        Some("shared"),
    ))
    .expect("insert tunnel b");
    db.insert_drawer_with_project_validity(
        &drawer("expired", "old memory", "alpha", Some("facts")),
        None,
        None,
        None,
        Some("1790000000"),
    )
    .expect("insert expired drawer");
    db.insert_triple(&Triple {
        id: build_triple_id("Alice", "works_at", "Acme"),
        subject: "Alice".to_string(),
        predicate: "works_at".to_string(),
        object: "Acme".to_string(),
        valid_from: Some("1780000000".to_string()),
        valid_to: Some("1790000000".to_string()),
        confidence: 0.9,
        source_drawer: Some("expired".to_string()),
    })
    .expect("insert stale triple");
}

#[derive(Debug, PartialEq, Eq)]
struct DbSnapshot {
    drawer_count: i64,
    triple_count: i64,
    explicit_tunnel_count: usize,
}

fn snapshot(db_path: &Path) -> DbSnapshot {
    let db = Database::open(db_path).expect("open db for snapshot");
    DbSnapshot {
        drawer_count: db.drawer_count().expect("drawer count"),
        triple_count: db.triple_count().expect("triple count"),
        explicit_tunnel_count: db
            .list_explicit_tunnels(None)
            .expect("explicit tunnel count")
            .len(),
    }
}

#[test]
fn reflect_cli_deterministic_json_reports_source_backed_categories() {
    let env = CliEnv::new_with_remote_embed_config();
    seed_reflection_findings(&env.db());
    let before = snapshot(&env.db_path);

    let output = env.run(&[
        "reflect",
        "--mode",
        "deterministic",
        "--now",
        NOW_RFC3339,
        "--json",
    ]);

    assert!(
        output.status.success(),
        "reflect failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).expect("json report");
    assert_eq!(report["mode"], "deterministic");
    assert_eq!(report["dry_run"], true);
    assert_eq!(report["summary"]["duplicate_group_count"], 1);
    assert_eq!(report["summary"]["expired_drawer_count"], 1);
    assert_eq!(report["summary"]["stale_kg_fact_count"], 1);
    assert_eq!(report["summary"]["tunnel_candidate_count"], 1);
    assert_eq!(
        report["duplicate_candidates"][0]["samples"][0]["drawer_id"],
        "dup-a"
    );
    assert_eq!(
        report["stale_kg_facts"][0]["source"]["drawer_id"],
        "expired"
    );
    assert_eq!(snapshot(&env.db_path), before);
}

#[test]
fn reflect_help_documents_deterministic_mode() {
    let env = CliEnv::new_with_remote_embed_config();

    let output = env.run(&["reflect", "--help"]);

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--mode"));
    assert!(stdout.contains("deterministic"));
    assert!(stdout.contains("--json"));
}
