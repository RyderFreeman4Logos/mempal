use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use mempal::core::db::Database;
use mempal::core::types::{BootstrapEvidenceArgs, Drawer, SourceType};
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

struct CompactionEnv {
    _tmp: TempDir,
    home: PathBuf,
    db_path: PathBuf,
}

impl CompactionEnv {
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

[consolidation]
similarity_threshold = 0.95
min_cluster_size = 3
max_clusters_per_run = 100
strategy = "richest_content"
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
}

fn drawer(id: &str, content: &str, importance: i32) -> Drawer {
    Drawer::new_bootstrap_evidence(BootstrapEvidenceArgs {
        id: id.to_string(),
        content: content.to_string(),
        wing: "memory".to_string(),
        room: Some("decision".to_string()),
        source_file: Some(format!("{id}.md")),
        source_type: SourceType::AgentInference,
        added_at: "2026-05-13T00:00:00Z".to_string(),
        chunk_index: Some(0),
        importance,
    })
}

fn seed_cluster(db_path: &Path) {
    let db = Database::open(db_path).expect("open db");
    for (drawer, vector) in [
        (
            drawer("cluster-a", "alpha decision", 1),
            vec![1.0_f32, 0.0, 0.0],
        ),
        (
            drawer("cluster-b", "beta decision with more detail", 5),
            vec![0.99_f32, 0.01, 0.0],
        ),
        (
            drawer("cluster-c", "gamma decision", 2),
            vec![0.98_f32, 0.02, 0.0],
        ),
        (drawer("distant", "unrelated", 5), vec![0.0_f32, 1.0, 0.0]),
    ] {
        db.insert_drawer_with_project(&drawer, Some("project-a"))
            .expect("insert drawer");
        db.insert_vector_with_project(&drawer.id, &vector, Some("project-a"))
            .expect("insert vector");
    }
}

#[test]
fn test_consolidate_dry_run_outputs_clusters_without_mutation() {
    let env = CompactionEnv::new();
    seed_cluster(&env.db_path);

    let output = Command::new(mempal_bin())
        .arg("consolidate")
        .arg("--dry-run")
        .arg("--wing")
        .arg("memory")
        .arg("--room")
        .arg("decision")
        .arg("--threshold")
        .arg("0.95")
        .arg("--min-cluster")
        .arg("3")
        .env("HOME", &env.home)
        .current_dir(&env.home)
        .output()
        .expect("run mempal consolidate");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout utf8");
    assert!(stdout.contains("clusters_found: 1"), "{stdout}");
    assert!(stdout.contains("cluster 1: size=3"), "{stdout}");
    assert!(stdout.contains("target=cluster-b"), "{stdout}");
    assert!(stdout.contains("cluster-a"), "{stdout}");
    assert!(
        stdout.contains("summary: clusters_found=1 processed=1 drawers_merged=0"),
        "{stdout}"
    );

    let db = Database::open(&env.db_path).expect("open db");
    assert_eq!(db.drawer_count().expect("drawer count"), 4);
    let log_count: i64 = db
        .conn()
        .query_row("SELECT COUNT(*) FROM consolidation_log", [], |row| {
            row.get(0)
        })
        .expect("log count");
    assert_eq!(log_count, 0);
}

#[test]
fn test_consolidate_real_run_updates_status_stats() {
    let env = CompactionEnv::new();
    seed_cluster(&env.db_path);

    let output = Command::new(mempal_bin())
        .arg("consolidate")
        .arg("--wing")
        .arg("memory")
        .arg("--room")
        .arg("decision")
        .arg("--threshold")
        .arg("0.95")
        .arg("--min-cluster")
        .arg("3")
        .env("HOME", &env.home)
        .current_dir(&env.home)
        .output()
        .expect("run mempal consolidate");
    assert!(output.status.success(), "{output:?}");

    let status = Command::new(mempal_bin())
        .arg("status")
        .env("HOME", &env.home)
        .current_dir(&env.home)
        .output()
        .expect("run mempal status");

    assert!(status.status.success(), "{status:?}");
    let stdout = String::from_utf8(status.stdout).expect("status stdout utf8");
    assert!(stdout.contains("Consolidation:"), "{stdout}");
    assert!(stdout.contains("total_compacted_drawers: 2"), "{stdout}");
    assert!(stdout.contains("consolidation_runs: 1"), "{stdout}");
    assert!(!stdout.contains("last_consolidation_at: none"), "{stdout}");
}
