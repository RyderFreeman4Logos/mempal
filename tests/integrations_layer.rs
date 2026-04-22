use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

struct IntegrationsEnv {
    _tmp: TempDir,
    home: PathBuf,
    repo: PathBuf,
}

impl IntegrationsEnv {
    fn new() -> Self {
        let tmp = TempDir::new().expect("tempdir");
        let home = tmp.path().join("home");
        let repo = tmp.path().join("repo");
        fs::create_dir_all(&home).expect("create home");
        fs::create_dir_all(&repo).expect("create repo");
        Self {
            _tmp: tmp,
            home,
            repo,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(mempal_bin())
            .args(args)
            .env("HOME", &self.home)
            .current_dir(&self.repo)
            .output()
            .expect("run mempal")
    }

    fn integrations_root(&self) -> PathBuf {
        self.home.join(".mempal").join("integrations")
    }

    fn manifest_path(&self) -> PathBuf {
        self.integrations_root().join("manifest.toml")
    }

    fn claude_settings_path(&self) -> PathBuf {
        self.home.join(".claude").join("settings.json")
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout utf8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr utf8")
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read json")).expect("parse json")
}

fn count_backups(dir: &Path, prefix: &str) -> usize {
    fs::read_dir(dir)
        .expect("read backup dir")
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.starts_with(prefix))
        })
        .count()
}

#[test]
fn test_bootstrap_creates_integrations_tree() {
    let env = IntegrationsEnv::new();

    let output = env.run(&["integrations", "bootstrap"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    let hooks_dir = env.integrations_root().join("claude-code").join("hooks");
    assert!(
        hooks_dir.is_dir(),
        "hooks dir missing: {}",
        hooks_dir.display()
    );
    let entries: Vec<_> = fs::read_dir(&hooks_dir)
        .expect("read hooks dir")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect hooks entries");
    assert!(
        !entries.is_empty(),
        "claude-code hooks dir must be non-empty"
    );

    let manifest = fs::read_to_string(env.manifest_path()).expect("read manifest");
    assert!(
        manifest.contains("claude-code/hooks/session-start.sh"),
        "manifest missing session-start asset:\n{manifest}"
    );
}

#[test]
fn test_bootstrap_idempotent_when_hash_matches() {
    let env = IntegrationsEnv::new();

    let first = env.run(&["integrations", "bootstrap"]);
    assert_eq!(first.status.code(), Some(0), "stderr: {}", stderr(&first));

    let hook_path = env
        .integrations_root()
        .join("claude-code")
        .join("hooks")
        .join("session-start.sh");
    let manifest_path = env.manifest_path();
    let hook_mtime = fs::metadata(&hook_path)
        .expect("hook metadata")
        .modified()
        .expect("hook mtime");
    let manifest_mtime = fs::metadata(&manifest_path)
        .expect("manifest metadata")
        .modified()
        .expect("manifest mtime");

    thread::sleep(Duration::from_secs(1));

    let second = env.run(&["integrations", "bootstrap"]);
    assert_eq!(second.status.code(), Some(0), "stderr: {}", stderr(&second));
    assert!(
        stdout(&second).contains("up-to-date"),
        "expected up-to-date stdout, got:\n{}",
        stdout(&second)
    );

    let hook_mtime_after = fs::metadata(&hook_path)
        .expect("hook metadata after")
        .modified()
        .expect("hook mtime after");
    let manifest_mtime_after = fs::metadata(&manifest_path)
        .expect("manifest metadata after")
        .modified()
        .expect("manifest mtime after");

    assert_eq!(
        hook_mtime_after, hook_mtime,
        "hook file should not be rewritten"
    );
    assert_eq!(
        manifest_mtime_after, manifest_mtime,
        "manifest file should not be rewritten"
    );
}

#[test]
fn test_install_appends_to_existing_settings() {
    let env = IntegrationsEnv::new();
    let claude_dir = env.home.join(".claude");
    fs::create_dir_all(&claude_dir).expect("create claude dir");

    let seed = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {
                    "hooks": [{
                        "type": "command",
                        "command": "echo unrelated session start"
                    }]
                }
            ]
        }
    });
    fs::write(
        env.claude_settings_path(),
        serde_json::to_string_pretty(&seed).expect("serialize seed"),
    )
    .expect("write seed settings");

    let output = env.run(&["integrations", "install", "--tool", "claude-code"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    let backups = count_backups(&claude_dir, "settings.json.bak.");
    assert_eq!(backups, 1, "install should create exactly one backup");

    let parsed = read_json(&env.claude_settings_path());
    let arr = parsed["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart array");
    assert_eq!(arr.len(), 2, "expected unrelated + mempal entries");
    assert_eq!(
        arr[0]["hooks"][0]["command"], "echo unrelated session start",
        "unrelated hook must remain first"
    );
    assert_eq!(arr[1]["mempal_source"], true);
    let mempal_command = arr[1]["hooks"][0]["command"]
        .as_str()
        .expect("mempal command");
    assert!(
        mempal_command.contains(".mempal/integrations/claude-code/hooks/session-start.sh"),
        "unexpected mempal command: {mempal_command}"
    );
}

#[test]
fn test_install_refuses_local_claude_dir() {
    let env = IntegrationsEnv::new();
    let local_claude_dir = env.repo.join(".claude");
    fs::create_dir_all(&local_claude_dir).expect("create local .claude");
    let local_settings = local_claude_dir.join("settings.json");
    fs::write(&local_settings, "{\n  \"hooks\": {}\n}\n").expect("write local settings");

    let output = env.run(&[
        "integrations",
        "install",
        "--tool",
        "claude-code",
        "--profile",
        "project",
    ]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "project profile must be rejected"
    );
    assert!(
        stderr(&output).contains("project profile disabled by P11 spec; use --profile user"),
        "unexpected stderr: {}",
        stderr(&output)
    );
    assert_eq!(
        fs::read_to_string(&local_settings).expect("read local settings"),
        "{\n  \"hooks\": {}\n}\n"
    );
}

#[test]
fn test_uninstall_preserves_unrelated_entries() {
    let env = IntegrationsEnv::new();
    let claude_dir = env.home.join(".claude");
    fs::create_dir_all(&claude_dir).expect("create claude dir");

    let seed = serde_json::json!({
        "hooks": {
            "SessionStart": [
                {
                    "hooks": [{
                        "type": "command",
                        "command": "echo unrelated session start"
                    }]
                },
                {
                    "mempal_source": true,
                    "hooks": [{
                        "type": "command",
                        "command": "bash /tmp/fake-mempal-session-start.sh"
                    }]
                }
            ]
        }
    });
    fs::write(
        env.claude_settings_path(),
        serde_json::to_string_pretty(&seed).expect("serialize seed"),
    )
    .expect("write seed settings");

    let output = env.run(&["integrations", "uninstall", "--tool", "claude-code"]);
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    let content = fs::read_to_string(env.claude_settings_path()).expect("read settings");
    let parsed: Value = serde_json::from_str(&content).expect("parse settings");
    let arr = parsed["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart array");
    assert_eq!(arr.len(), 1, "only unrelated entry should remain");
    assert_eq!(
        arr[0]["hooks"][0]["command"],
        "echo unrelated session start"
    );
    assert!(
        !content.contains("\"mempal_source\": true"),
        "mempal marker must be removed"
    );
}

#[test]
fn test_concurrent_install_flock_serialized() {
    let env = IntegrationsEnv::new();
    let claude_dir = env.home.join(".claude");
    fs::create_dir_all(&claude_dir).expect("create claude dir");
    fs::write(
        env.claude_settings_path(),
        serde_json::to_string_pretty(&serde_json::json!({
            "hooks": {
                "SessionStart": [{
                    "hooks": [{
                        "type": "command",
                        "command": "echo unrelated session start"
                    }]
                }]
            }
        }))
        .expect("serialize seed"),
    )
    .expect("write seed settings");

    let bootstrap = env.run(&["integrations", "bootstrap"]);
    assert_eq!(
        bootstrap.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&bootstrap)
    );

    let mut child_a = Command::new(mempal_bin())
        .args(["integrations", "install", "--tool", "claude-code"])
        .env("HOME", &env.home)
        .current_dir(&env.repo)
        .spawn()
        .expect("spawn install A");
    let mut child_b = Command::new(mempal_bin())
        .args(["integrations", "install", "--tool", "claude-code"])
        .env("HOME", &env.home)
        .current_dir(&env.repo)
        .spawn()
        .expect("spawn install B");

    let status_a = child_a.wait().expect("wait install A");
    let status_b = child_b.wait().expect("wait install B");
    assert!(status_a.success(), "install A failed: {status_a:?}");
    assert!(status_b.success(), "install B failed: {status_b:?}");

    let backup_count = count_backups(&claude_dir, "settings.json.bak.");
    assert_eq!(backup_count, 1, "only one install should create a backup");

    let parsed = read_json(&env.claude_settings_path());
    let arr = parsed["hooks"]["SessionStart"]
        .as_array()
        .expect("SessionStart array");
    let mempal_entries = arr
        .iter()
        .filter(|entry| entry.get("mempal_source").and_then(Value::as_bool) == Some(true))
        .count();
    assert_eq!(mempal_entries, 1, "expected exactly one mempal entry");
}

#[test]
fn test_codex_install_returns_not_yet_implemented() {
    let env = IntegrationsEnv::new();

    let output = env.run(&["integrations", "install", "--tool", "codex"]);
    assert_ne!(
        output.status.code(),
        Some(0),
        "codex install should fail with stub error"
    );
    assert!(
        stderr(&output).contains(
            "not_yet_implemented: codex integration is spec-reserved, see specs/fork-ext/p11-integrations-layer.spec.md"
        ),
        "unexpected stderr: {}",
        stderr(&output)
    );
}

#[test]
fn test_status_reports_drift_on_manual_edit() {
    let env = IntegrationsEnv::new();

    let bootstrap = env.run(&["integrations", "bootstrap"]);
    assert_eq!(
        bootstrap.status.code(),
        Some(0),
        "stderr: {}",
        stderr(&bootstrap)
    );

    let hook_path = env
        .integrations_root()
        .join("claude-code")
        .join("hooks")
        .join("session-start.sh");
    fs::write(&hook_path, "#!/bin/sh\necho drifted\n").expect("mutate hook");

    let status = env.run(&["integrations", "status"]);
    assert_ne!(
        status.status.code(),
        Some(0),
        "asset drift must produce non-zero exit"
    );
    let combined = format!("{}\n{}", stdout(&status), stderr(&status));
    assert!(
        combined.contains("drifted"),
        "status must report drift, got:\n{combined}"
    );
}
