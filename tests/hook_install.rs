use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::sync::{Mutex, OnceLock};

use mempal::hook_install::{HookInstallTarget, install, install_claude_code};
use serde_json::Value;
use tempfile::TempDir;

fn parse_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read json")).expect("parse json")
}

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: OnceLock<Mutex<()>> = OnceLock::new();
    GUARD
        .get_or_init(|| Mutex::new(()))
        .lock()
        .expect("env mutex")
}

#[test]
fn test_hook_install_writes_claude_code_settings() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&home).expect("create home");

    let outcome = install_claude_code(&cwd, &home, false, false).expect("install");
    let parsed = parse_json(&outcome.write_path);

    assert!(outcome.display_path.ends_with(".claude/settings.json"));
    assert!(outcome.changed);
    assert_eq!(outcome.removed_commands, 0);
    assert!(outcome.rendered.contains("hook_post_tool"));
    assert_eq!(
        parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        "mempal hook hook_post_tool"
    );
    assert_eq!(
        parsed["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        "mempal hook hook_user_prompt"
    );
}

#[test]
fn test_hook_install_respects_project_local_settings() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(cwd.join(".claude")).expect("create local .claude");
    fs::create_dir_all(&home).expect("create home");
    fs::write(cwd.join(".claude/settings.json"), r#"{ "theme": "dark" }"#)
        .expect("write local settings");

    let outcome = install_claude_code(&cwd, &home, false, false).expect("install");
    let parsed = parse_json(&outcome.write_path);

    assert!(outcome.display_path.ends_with(".claude/settings.json"));
    assert!(outcome.rendered.contains("\"theme\": \"dark\""));
    assert_eq!(parsed["theme"], "dark");
    assert!(
        !home.join(".claude/settings.json").exists(),
        "global settings must remain untouched"
    );
}

#[cfg(unix)]
#[test]
fn test_hook_install_follows_symlink_target() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    let target_dir = tmp.path().join("target-settings");
    fs::create_dir_all(cwd.join(".claude")).expect("create local .claude");
    fs::create_dir_all(&home).expect("create home");
    fs::create_dir_all(&target_dir).expect("create target dir");

    let real_target = target_dir.join("claude-settings.json");
    fs::write(&real_target, r#"{ "theme": "dark" }"#).expect("write target settings");
    let local_link = cwd.join(".claude/settings.json");
    symlink(&real_target, &local_link).expect("create symlink");

    let outcome = install_claude_code(&cwd, &home, false, false).expect("install");
    let parsed = parse_json(&real_target);

    assert!(
        fs::symlink_metadata(&local_link)
            .expect("symlink metadata")
            .file_type()
            .is_symlink()
    );
    assert_eq!(parsed["theme"], "dark");
    assert_eq!(outcome.write_path, real_target);
    assert!(outcome.rendered.contains("hook_post_tool"));
    assert_eq!(
        parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        "mempal hook hook_post_tool"
    );
}

#[test]
fn test_hook_install_coexists_with_upstream_cowork_entry() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(cwd.join(".claude")).expect("create local .claude");
    fs::create_dir_all(&home).expect("create home");
    fs::write(
        cwd.join(".claude/settings.json"),
        r#"{
          "hooks": {
            "UserPromptSubmit": [{
              "hooks": [{
                "type": "command",
                "command": ".claude/hooks/user-prompt-submit.sh"
              }]
            }]
          }
        }"#,
    )
    .expect("write seed settings");

    let outcome = install_claude_code(&cwd, &home, false, false).expect("install");
    let parsed = parse_json(&outcome.write_path);
    let entries = parsed["hooks"]["UserPromptSubmit"]
        .as_array()
        .expect("prompt array");
    let commands: Vec<&str> = entries
        .iter()
        .flat_map(|entry| entry["hooks"].as_array().expect("hook array").iter())
        .filter_map(|hook| hook["command"].as_str())
        .collect();

    assert!(commands.contains(&".claude/hooks/user-prompt-submit.sh"));
    assert!(commands.contains(&"mempal hook hook_user_prompt"));
    assert_eq!(
        commands
            .iter()
            .filter(|command| **command == "mempal hook hook_user_prompt")
            .count(),
        1
    );

    let second = install_claude_code(&cwd, &home, false, false).expect("reinstall");
    let parsed_second = parse_json(&second.write_path);
    let second_commands: Vec<&str> = parsed_second["hooks"]["UserPromptSubmit"]
        .as_array()
        .expect("prompt array")
        .iter()
        .flat_map(|entry| entry["hooks"].as_array().expect("hook array").iter())
        .filter_map(|hook| hook["command"].as_str())
        .collect();
    assert_eq!(
        second_commands
            .iter()
            .filter(|command| **command == "mempal hook hook_user_prompt")
            .count(),
        1
    );
    assert!(!second.changed, "second install should be idempotent");
}

#[cfg(unix)]
#[test]
fn test_hook_install_refuses_agent_instruction_targets() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    let forbidden = tmp.path().join("AGENTS.md");
    fs::create_dir_all(cwd.join(".claude")).expect("create local .claude");
    fs::create_dir_all(&home).expect("create home");
    fs::write(&forbidden, "instructions").expect("write forbidden target");

    let local_link = cwd.join(".claude/settings.json");
    symlink(&forbidden, &local_link).expect("create symlink");

    let error = install_claude_code(&cwd, &home, false, false).expect_err("must refuse");
    assert!(
        error.to_string().contains("agent-instruction"),
        "unexpected error: {error}"
    );
}

#[test]
fn test_hook_install_public_wrapper_uses_home_env() {
    let _guard = env_guard();
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&home).expect("create home");

    let original_cwd = std::env::current_dir().expect("current dir");
    let original_home = std::env::var_os("HOME");
    std::env::set_current_dir(&cwd).expect("set current dir");
    // SAFETY: the mutex above serializes process-global env mutation for this test.
    unsafe { std::env::set_var("HOME", &home) };
    install(HookInstallTarget::ClaudeCode, false, false).expect("wrapper install");
    std::env::set_current_dir(original_cwd).expect("restore current dir");
    if let Some(home) = original_home {
        // SAFETY: guarded by the mutex above.
        unsafe { std::env::set_var("HOME", home) };
    }

    let parsed = parse_json(&home.join(".claude/settings.json"));
    assert_eq!(
        parsed["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
        "mempal hook hook_session_end"
    );
}
