use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use mempal::core::config::Config;
use mempal::core::db::Database;
use mempal::core::types::{Drawer, SourceType};
use mempal::hotpatch::generator::{GenerationOptions, suggest_for_drawer};
use mempal::hotpatch::manager::{ApplyOptions, ReviewOptions, apply, review};
use sha2::{Digest, Sha256};
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
        let mempal_home = tmp.path().join("home/.mempal");
        let project_dir = tmp.path().join("project");
        fs::create_dir_all(&mempal_home).expect("create mempal home");
        fs::create_dir_all(project_dir.join("src")).expect("create src");
        fs::write(project_dir.join("CLAUDE.md"), "# Project\n").expect("write claude");
        let db_path = mempal_home.join("palace.db");
        Database::open(&db_path).expect("open db");
        Self {
            _tmp: tmp,
            mempal_home,
            db_path,
            project_dir,
        }
    }

    fn config(&self, extra: &str) -> Config {
        Config::parse(&format!(
            r#"
db_path = "{}"

[project]
id = "project-alpha"

[search]
strict_project_isolation = true

[hotpatch]
enabled = true
min_importance_stars = 4
watch_files = ["CLAUDE.md", "AGENTS.md", "GEMINI.md"]
max_suggestion_length = 80
allowed_target_prefixes = ["{}"]
{}
"#,
            self.db_path.display(),
            self.project_dir.display(),
            extra,
        ))
        .expect("parse config")
    }

    fn suggestion_file_for(&self, dir: &Path) -> PathBuf {
        let canonical = dir.canonicalize().expect("canonical dir");
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        self.mempal_home
            .join("hotpatch")
            .join(format!("CLAUDE-{}.md", &hash[..12]))
    }

    fn write_suggestion_file(&self, dir: &Path, lines: &[&str]) -> PathBuf {
        let suggestion_file = self.suggestion_file_for(dir);
        fs::create_dir_all(
            suggestion_file
                .parent()
                .expect("suggestion file parent must exist"),
        )
        .expect("create hotpatch dir");
        let content = format!(
            "# mempal hotpatch suggestions for {}\n\n<!-- managed by mempal -->\n\n{}\n",
            dir.canonicalize().expect("canonical dir").display(),
            lines.join("\n")
        );
        fs::write(&suggestion_file, content).expect("write suggestion file");
        suggestion_file
    }

    fn write_hook_payload(&self, file_path: &Path) -> PathBuf {
        let payload_path = self.mempal_home.join("hook.json");
        let payload = serde_json::json!({
            "tool_name": "Edit",
            "tool_input": {
                "file_path": file_path.display().to_string()
            }
        });
        fs::write(&payload_path, payload.to_string()).expect("write hook payload");
        payload_path
    }

    fn insert_drawer(&self, drawer_id: &str, content: &str, source_file: &Path) {
        let db = Database::open(&self.db_path).expect("open db");
        db.insert_drawer_with_project(
            &Drawer {
                id: drawer_id.to_string(),
                content: content.to_string(),
                wing: "hooks-raw".to_string(),
                room: Some("Edit".to_string()),
                source_file: Some(source_file.display().to_string()),
                source_type: SourceType::Manual,
                added_at: "1713000000".to_string(),
                chunk_index: Some(0),
                importance: 5,
            },
            Some("project-alpha"),
        )
        .expect("insert drawer");
    }
}

fn review_default(env: &HotpatchEnv) -> mempal::hotpatch::manager::ReviewReport {
    review(
        &env.config(""),
        &env.mempal_home,
        ReviewOptions {
            dir: None,
            include_applied: false,
            include_dismissed: false,
        },
    )
    .expect("review")
}

fn network_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("network guard")
}

#[test]
fn test_apply_confirm_merges_to_claudemd() {
    let env = HotpatchEnv::new();
    env.write_suggestion_file(
        &env.project_dir,
        &["- ★★★★★ decision: append the new rule [drawer:apply-confirm]"],
    );

    let outcome = apply(
        &env.config(""),
        &env.mempal_home,
        ApplyOptions {
            dir: env.project_dir.clone(),
            confirm: true,
        },
    )
    .expect("apply");

    let target = fs::read_to_string(env.project_dir.join("CLAUDE.md")).expect("read target");
    let review = review_default(&env);
    assert_eq!(outcome.applied_count, 1);
    assert!(target.contains("append the new rule"));
    assert_eq!(review.pending_count, 0);
    assert!(!review.stdout.contains("apply-confirm"));
}

#[test]
fn test_apply_preserves_existing_content() {
    let env = HotpatchEnv::new();
    let original = "EXISTING_CONTENT_MARKER\n## Section A\nKeep this block byte-identical.\n";
    fs::write(env.project_dir.join("CLAUDE.md"), original).expect("write original claude");
    env.write_suggestion_file(
        &env.project_dir,
        &["- ★★★★★ decision: additive only [drawer:preserve]"],
    );

    apply(
        &env.config(""),
        &env.mempal_home,
        ApplyOptions {
            dir: env.project_dir.clone(),
            confirm: true,
        },
    )
    .expect("apply");

    let updated = fs::read_to_string(env.project_dir.join("CLAUDE.md")).expect("read target");
    assert!(updated.starts_with(original));
    assert!(updated.contains("EXISTING_CONTENT_MARKER"));
    assert!(updated.contains("additive only"));
}

#[test]
fn test_apply_without_confirm_is_dry_run() {
    let env = HotpatchEnv::new();
    let target_path = env.project_dir.join("CLAUDE.md");
    let before = fs::read_to_string(&target_path).expect("read target before");
    env.write_suggestion_file(
        &env.project_dir,
        &["- ★★★★★ decision: keep \u{1b}[31mRED\u{1b}[0m preview [drawer:dry-run]"],
    );

    let outcome = apply(
        &env.config(""),
        &env.mempal_home,
        ApplyOptions {
            dir: env.project_dir.clone(),
            confirm: false,
        },
    )
    .expect("apply dry run");
    let review = review_default(&env);
    let after = fs::read_to_string(&target_path).expect("read target after");

    assert_eq!(before, after);
    assert_eq!(review.pending_count, 1);
    assert!(outcome.stdout.contains("dry-run"));
    assert!(outcome.stdout.contains("\\u{1b}[31mRED\\u{1b}[0m"));
    assert!(!outcome.stdout.contains('\u{1b}'));
}

#[test]
fn test_hotpatch_no_llm_api_calls() {
    let _guard = network_guard();
    let env = HotpatchEnv::new();
    let file_path = env.project_dir.join("src/lib.rs");
    fs::write(&file_path, "pub fn demo() {}\n").expect("write source");
    let payload_path = env.write_hook_payload(&file_path);
    env.insert_drawer(
        "drawer-network-audit",
        "Decision: local hotpatch generation must stay offline.",
        &payload_path,
    );

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sentinel listener");
    listener
        .set_nonblocking(true)
        .expect("set nonblocking listener");
    let addr = listener.local_addr().expect("listener addr");
    let accepts = Arc::new(AtomicUsize::new(0));
    let running = Arc::new(AtomicBool::new(true));
    let accept_count = Arc::clone(&accepts);
    let accept_running = Arc::clone(&running);
    let join = thread::spawn(move || {
        while accept_running.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((_stream, _addr)) => {
                    accept_count.fetch_add(1, Ordering::SeqCst);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    let proxy = format!("http://{addr}");
    // SAFETY: guarded by a process-global mutex in this test module.
    unsafe {
        std::env::set_var("HTTP_PROXY", &proxy);
        std::env::set_var("HTTPS_PROXY", &proxy);
        std::env::set_var("ALL_PROXY", &proxy);
    }

    let db = Database::open(&env.db_path).expect("open db");
    let outcome = suggest_for_drawer(
        &db,
        &env.config(""),
        &env.mempal_home,
        "drawer-network-audit",
        GenerationOptions::default(),
    )
    .expect("generate suggestion");

    running.store(false, Ordering::SeqCst);
    join.join().expect("join listener thread");
    // SAFETY: guarded by a process-global mutex in this test module.
    unsafe {
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("HTTPS_PROXY");
        std::env::remove_var("ALL_PROXY");
    }

    assert_eq!(outcome.appended, 1);
    assert_eq!(accepts.load(Ordering::SeqCst), 0);
}

#[test]
fn test_review_lists_pending_suggestions() {
    let env = HotpatchEnv::new();
    let second_dir = env.project_dir.join("subdir");
    fs::create_dir_all(&second_dir).expect("create second dir");
    fs::write(second_dir.join("CLAUDE.md"), "# Subdir\n").expect("write subdir claude");

    env.write_suggestion_file(
        &env.project_dir,
        &[
            "- ★★★★★ decision: alpha [drawer:pending-a]",
            "- ★★★★ decision: beta \u{1b}[31mwarn\u{1b}[0m [drawer:pending-b]",
            "- ★★★ decision: already applied [drawer:applied-x] <!-- applied 1713000000 -->",
        ],
    );
    env.write_suggestion_file(&second_dir, &["- ★★★★★ note: gamma [drawer:pending-c]"]);

    let report = review_default(&env);

    assert_eq!(report.pending_count, 3);
    assert!(report.stdout.contains("pending-a"));
    assert!(report.stdout.contains("pending-b"));
    assert!(report.stdout.contains("pending-c"));
    assert!(report.stdout.contains("\\u{1b}[31mwarn\\u{1b}[0m"));
    assert!(!report.stdout.contains('\u{1b}'));
    assert!(!report.stdout.contains("applied-x"));
}

#[cfg(unix)]
#[test]
fn test_hotpatch_symlink_escape_rejected() {
    let env = HotpatchEnv::new();
    let outside_dir = env
        .project_dir
        .parent()
        .expect("project parent")
        .join("outside-root");
    fs::create_dir_all(outside_dir.join("src")).expect("create outside dir");
    fs::write(outside_dir.join("CLAUDE.md"), "# Outside\n").expect("write outside claude");
    fs::write(outside_dir.join("src/lib.rs"), "pub fn escaped() {}\n")
        .expect("write outside source");
    std::os::unix::fs::symlink(&outside_dir, env.project_dir.join("escaped"))
        .expect("create escape symlink");

    let escaped_file = env.project_dir.join("escaped/src/lib.rs");
    let payload_path = env.write_hook_payload(&escaped_file);
    env.insert_drawer(
        "drawer-symlink-escape",
        "Decision: reject escaped watched directories.",
        &payload_path,
    );

    let db = Database::open(&env.db_path).expect("open db");
    let error = suggest_for_drawer(
        &db,
        &env.config(""),
        &env.mempal_home,
        "drawer-symlink-escape",
        GenerationOptions::default(),
    )
    .expect_err("symlink escape must be rejected");

    let message = error.to_string();
    assert!(
        message.contains("symlink escape") || message.contains("outside project root"),
        "unexpected error: {message}"
    );
    let hotpatch_dir = env.mempal_home.join("hotpatch");
    assert!(
        !hotpatch_dir.exists()
            || fs::read_dir(&hotpatch_dir)
                .expect("read hotpatch dir")
                .next()
                .is_none()
    );
    assert!(
        !env.project_dir.join("escaped/CLAUDE.md").exists()
            || outside_dir.join("CLAUDE.md").exists()
    );
}
