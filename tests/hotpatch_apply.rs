use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::thread;

use mempal::core::config::Config;
use mempal::hotpatch::manager::{
    ApplyOptions, DismissOptions, ReviewOptions, apply, dismiss, review,
};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

struct ApplyEnv {
    _tmp: TempDir,
    mempal_home: PathBuf,
    project_dir: PathBuf,
    drafts_dir: PathBuf,
}

impl ApplyEnv {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().join("home");
        let mempal_home = home.join(".mempal");
        let project_dir = tmp.path().join("workspace/project-alpha");
        let drafts_dir = home.join("drafts/project-alpha");
        fs::create_dir_all(mempal_home.join("hotpatch")).expect("create hotpatch dir");
        fs::create_dir_all(&project_dir).expect("create project dir");
        fs::create_dir_all(&drafts_dir).expect("create drafts dir");
        Self {
            _tmp: tmp,
            mempal_home,
            project_dir,
            drafts_dir,
        }
    }

    fn config(&self) -> Config {
        let config_text = format!(
            r#"
[hotpatch]
enabled = true
watch_files = ["CLAUDE.md", "AGENTS.md", "GEMINI.md"]
max_suggestion_length = 80
allowed_target_prefixes = ["{}", "{}"]
"#,
            self.project_dir.parent().expect("workspace").display(),
            self.drafts_dir.parent().expect("drafts root").display()
        );
        Config::parse(&config_text).expect("parse config")
    }

    fn suggestion_file(&self) -> PathBuf {
        let canonical = self
            .project_dir
            .canonicalize()
            .expect("canonical project dir");
        let mut hasher = Sha256::new();
        hasher.update(canonical.to_string_lossy().as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        self.mempal_home
            .join("hotpatch")
            .join(format!("CLAUDE-{}.md", &hash[..12]))
    }

    fn write_target_claude(&self, contents: &str) -> PathBuf {
        let target = self.project_dir.join("CLAUDE.md");
        fs::write(&target, contents).expect("write claude");
        target
    }

    fn write_suggestion_file(&self, dir: &Path, lines: &[&str]) {
        let content = format!(
            "# mempal hotpatch suggestions for {}\n\n<!-- managed by mempal -->\n\n{}\n",
            dir.canonicalize().expect("canonical dir").display(),
            lines.join("\n")
        );
        fs::write(self.suggestion_file(), content).expect("write suggestion file");
    }
}

#[test]
fn test_suggestion_pool_default_shows_pending_not_applied() {
    let env = ApplyEnv::new();
    env.write_target_claude("# Project\n");
    env.write_suggestion_file(
        &env.project_dir,
        &[
            "- ★★★★★ decision: pending one [drawer:pending1]",
            "- ★★★★ decision: already applied [drawer:applied1] <!-- applied 1713000000 -->",
            "- ★★★★ decision: already dismissed [drawer:dismiss1] <!-- dismissed 1713000001 -->",
        ],
    );

    let report = review(
        &env.config(),
        &env.mempal_home,
        ReviewOptions {
            dir: None,
            include_applied: false,
            include_dismissed: false,
        },
    )
    .expect("review");

    assert_eq!(report.pending_count, 1);
    assert_eq!(report.entries.len(), 1);
    assert!(report.stdout.contains("pending1"));
    assert!(!report.stdout.contains("applied1"));
    assert!(!report.stdout.contains("dismiss1"));
}

#[test]
fn test_claudemd_apply_requires_confirm_flag() {
    let env = ApplyEnv::new();
    let target = env.write_target_claude("# Project\n");
    env.write_suggestion_file(
        &env.project_dir,
        &["- ★★★★★ decision: use Arc<Mutex<>> [drawer:applyreq]"],
    );

    let outcome = apply(
        &env.config(),
        &env.mempal_home,
        ApplyOptions {
            dir: env.project_dir.clone(),
            confirm: false,
        },
    )
    .expect("apply dry run");

    assert!(outcome.stdout.contains("dry-run"));
    assert!(
        outcome
            .stdout
            .contains("+ - ★★★★★ decision: use Arc<Mutex<>> [drawer:applyreq]")
    );
    assert_eq!(
        fs::read_to_string(&target).expect("read target"),
        "# Project\n"
    );
    let suggestion = fs::read_to_string(env.suggestion_file()).expect("read suggestion");
    assert!(!suggestion.contains("<!-- applied"));
}

#[test]
fn test_claudemd_apply_with_confirm_writes_and_locks() {
    let env = ApplyEnv::new();
    env.write_target_claude("# Project\n");
    env.write_suggestion_file(
        &env.project_dir,
        &["- ★★★★★ decision: one-time patch [drawer:locktest]"],
    );
    let config = env.config();
    let barrier = Arc::new(Barrier::new(2));
    let mempal_home = env.mempal_home.clone();
    let dir = env.project_dir.clone();

    let first_barrier = barrier.clone();
    let first_home = mempal_home.clone();
    let first_dir = dir.clone();
    let first_config = config.clone();
    let first = thread::spawn(move || {
        first_barrier.wait();
        apply(
            &first_config,
            &first_home,
            ApplyOptions {
                dir: first_dir,
                confirm: true,
            },
        )
        .expect("first apply")
    });

    let second_barrier = barrier.clone();
    let second_home = mempal_home.clone();
    let second_dir = dir.clone();
    let second_config = config.clone();
    let second = thread::spawn(move || {
        second_barrier.wait();
        apply(
            &second_config,
            &second_home,
            ApplyOptions {
                dir: second_dir,
                confirm: true,
            },
        )
        .expect("second apply")
    });

    let first = first.join().expect("join first");
    let second = second.join().expect("join second");
    let target = fs::read_to_string(env.project_dir.join("CLAUDE.md")).expect("read target");
    let suggestion = fs::read_to_string(env.suggestion_file()).expect("read suggestion");

    assert_eq!(
        target.matches("one-time patch").count(),
        1,
        "target must contain one patch"
    );
    assert_eq!(
        suggestion.matches("<!-- applied ").count(),
        1,
        "suggestion file must contain one applied marker"
    );
    assert_eq!(first.applied_count + second.applied_count, 1);
}

#[test]
fn test_claudemd_apply_preserves_existing_file_content() {
    let env = ApplyEnv::new();
    let original = "# Project\n## Rules\n- do X\n- do Y\n";
    let target = env.write_target_claude(original);
    env.write_suggestion_file(
        &env.project_dir,
        &["- ★★★★★ decision: append only [drawer:preserve]"],
    );

    apply(
        &env.config(),
        &env.mempal_home,
        ApplyOptions {
            dir: env.project_dir.clone(),
            confirm: true,
        },
    )
    .expect("apply");

    let after = fs::read_to_string(&target).expect("read target");
    assert!(after.starts_with(original));
    assert!(after.contains("- ★★★★★ decision: append only [drawer:preserve]"));
}

#[test]
fn test_claudemd_dismiss_removes_from_pool_without_writing() {
    let env = ApplyEnv::new();
    let target = env.write_target_claude("# Project\n");
    env.write_suggestion_file(
        &env.project_dir,
        &["- ★★★★★ decision: dismiss me [drawer:dismissme]"],
    );

    let dismissed = dismiss(
        &env.config(),
        &env.mempal_home,
        DismissOptions {
            dir: env.project_dir.clone(),
        },
    )
    .expect("dismiss");
    let review_report = review(
        &env.config(),
        &env.mempal_home,
        ReviewOptions {
            dir: Some(env.project_dir.clone()),
            include_applied: false,
            include_dismissed: false,
        },
    )
    .expect("review");
    let suggestion = fs::read_to_string(env.suggestion_file()).expect("read suggestion");

    assert_eq!(dismissed.dismissed_count, 1);
    assert_eq!(
        fs::read_to_string(target).expect("read target"),
        "# Project\n"
    );
    assert_eq!(review_report.pending_count, 0);
    assert!(suggestion.contains("<!-- dismissed "));
}

#[test]
fn test_apply_works_on_gitignored_claudemd() {
    let env = ApplyEnv::new();
    let target = env.drafts_dir.join("CLAUDE.md");
    fs::write(&target, "# Draft Rules\n").expect("write drafts target");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, env.project_dir.join("CLAUDE.md")).expect("symlink");
    #[cfg(not(unix))]
    fs::copy(&target, env.project_dir.join("CLAUDE.md")).expect("copy fallback");
    env.write_suggestion_file(
        &env.project_dir,
        &["- ★★★★★ decision: write symlink target [drawer:symlink1]"],
    );

    apply(
        &env.config(),
        &env.mempal_home,
        ApplyOptions {
            dir: env.project_dir.clone(),
            confirm: true,
        },
    )
    .expect("apply");

    let link_meta = fs::symlink_metadata(env.project_dir.join("CLAUDE.md")).expect("link metadata");
    let target_contents = fs::read_to_string(&target).expect("read target");
    assert!(target_contents.contains("write symlink target"));
    #[cfg(unix)]
    assert!(
        link_meta.file_type().is_symlink(),
        "CLAUDE link must stay a symlink"
    );
}
