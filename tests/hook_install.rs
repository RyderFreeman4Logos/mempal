use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::process::Command;

use mempal::hook_install::install_claude_code;
use serde_json::Value;
use tempfile::TempDir;

fn parse_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read json")).expect("parse json")
}

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn expected_hook_command(alias: &str) -> String {
    let binary = fs::canonicalize(mempal_bin()).expect("canonical mempal bin");
    format!("{} hook {alias}", binary.display())
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
        expected_hook_command("hook_post_tool")
    );
    assert_eq!(
        parsed["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        expected_hook_command("hook_user_prompt")
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
    assert_eq!(
        parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        expected_hook_command("hook_post_tool")
    );
    assert!(
        !home.join(".claude/settings.json").exists(),
        "global settings must remain untouched"
    );
}

#[test]
fn test_hook_install_merges_existing_settings() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(cwd.join(".claude")).expect("create local .claude");
    fs::create_dir_all(&home).expect("create home");
    fs::write(
        cwd.join(".claude/settings.json"),
        r#"{
          "theme": "dark",
          "hooks": {
            "Stop": [{
              "hooks": [{
                "type": "command",
                "command": "existing stop hook"
              }]
            }]
          }
        }"#,
    )
    .expect("write seed settings");

    let outcome = install_claude_code(&cwd, &home, false, false).expect("install");
    let parsed = parse_json(&outcome.write_path);

    assert_eq!(parsed["theme"], "dark");
    assert_eq!(
        parsed["hooks"]["Stop"][0]["hooks"][0]["command"],
        "existing stop hook"
    );
    assert_eq!(
        parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        expected_hook_command("hook_post_tool")
    );
    assert!(outcome.changed);
}

#[cfg(unix)]
#[test]
fn test_hook_install_follows_symlink_target() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    let target_dir = cwd.join("shared");
    fs::create_dir_all(cwd.join(".claude")).expect("create local .claude");
    fs::create_dir_all(&home).expect("create home");
    fs::create_dir_all(target_dir.join(".claude")).expect("create target dir");

    let real_target = target_dir.join(".claude/settings.json");
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
        expected_hook_command("hook_post_tool")
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
    let expected = expected_hook_command("hook_user_prompt");

    assert!(commands.contains(&".claude/hooks/user-prompt-submit.sh"));
    assert!(commands.iter().any(|command| *command == expected));
    assert_eq!(
        commands
            .iter()
            .filter(|command| **command == expected)
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
            .filter(|command| **command == expected)
            .count(),
        1
    );
    assert!(!second.changed, "second install should be idempotent");
}

#[test]
fn test_hook_install_dry_run_does_not_write() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&home).expect("create home");

    let outcome = install_claude_code(&cwd, &home, true, false).expect("dry-run install");
    assert!(
        outcome
            .rendered
            .contains(&expected_hook_command("hook_post_tool"))
    );
    assert!(
        !home.join(".claude/settings.json").exists(),
        "dry-run must not write global settings"
    );
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

#[cfg(unix)]
#[test]
fn test_hook_install_absolute_path_binary() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&home).expect("create home");

    let outcome = install_claude_code(&cwd, &home, false, false).expect("install");
    let parsed = parse_json(&outcome.write_path);
    let command = parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"]
        .as_str()
        .expect("post-tool command");
    assert!(
        command.starts_with('/'),
        "hook command must use absolute binary path, got {command}"
    );
    assert_eq!(command, expected_hook_command("hook_post_tool"));

    let outside = TempDir::new().expect("outside tempdir");
    let outside_target = outside.path().join(".claude/settings.json");
    fs::create_dir_all(outside_target.parent().expect("outside parent"))
        .expect("create outside parent");
    fs::write(&outside_target, "{}").expect("write outside settings");

    fs::create_dir_all(cwd.join(".claude")).expect("create local .claude");
    let local_link = cwd.join(".claude/settings.json");
    symlink(&outside_target, &local_link).expect("create external symlink");

    let error = install_claude_code(&cwd, &home, false, false).expect_err("must reject");
    assert!(
        error.to_string().contains("outside allowed roots"),
        "unexpected error: {error}"
    );
}

#[test]
fn test_hook_install_public_wrapper_uses_home_env() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&home).expect("create home");

    let output = Command::new(mempal_bin())
        .args(["hook", "install", "--target", "claude-code"])
        .current_dir(&cwd)
        .env("HOME", &home)
        .output()
        .expect("wrapper install");
    assert!(
        output.status.success(),
        "install command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed = parse_json(&home.join(".claude/settings.json"));
    assert_eq!(
        parsed["hooks"]["SessionEnd"][0]["hooks"][0]["command"],
        expected_hook_command("hook_session_end")
    );
}
