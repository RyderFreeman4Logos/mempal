use std::fs;
#[cfg(unix)]
use std::os::unix::fs::symlink;
use std::process::Command;

use mempal::hook_install::{
    McpInstallStatus, install_claude_code, install_codex, install_user_mcp,
};
use serde_json::{Value, json};
use tempfile::TempDir;

fn parse_json(path: &std::path::Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).expect("read json")).expect("parse json")
}

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn expected_hook_command(name: &str) -> String {
    let binary = fs::canonicalize(mempal_bin()).expect("canonical mempal bin");
    format!("{} hook {name}", binary.display())
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
    assert!(outcome.rendered.contains("PostToolUse"));
    assert_eq!(
        parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        expected_hook_command("PostToolUse")
    );
    assert_eq!(
        parsed["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        expected_hook_command("UserPromptSubmit")
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
        expected_hook_command("PostToolUse")
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
        expected_hook_command("PostToolUse")
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
    assert!(outcome.rendered.contains("PostToolUse"));
    assert_eq!(
        parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        expected_hook_command("PostToolUse")
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
    let expected = expected_hook_command("UserPromptSubmit");

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
            .contains(&expected_hook_command("PostToolUse"))
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
    assert_eq!(command, expected_hook_command("PostToolUse"));

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
        expected_hook_command("SessionEnd")
    );

    // Issue #129: the same install also seeds `~/.mcp.json` with the mempal
    // server entry so non-mempal projects expose mempal_* tools.
    let mcp = parse_json(&home.join(".mcp.json"));
    assert_eq!(
        mcp["mcpServers"]["mempal"]["command"], "mempal",
        "mempal MCP server entry must be installed in ~/.mcp.json"
    );
    assert_eq!(
        mcp["mcpServers"]["mempal"]["args"],
        json!(["serve", "--mcp"])
    );
}

#[test]
fn test_install_user_mcp_creates_file_when_missing() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home");

    let outcome = install_user_mcp(&home, false, false).expect("install mcp");
    assert_eq!(outcome.status, McpInstallStatus::Created);
    assert!(home.join(".mcp.json").exists(), "mcp file must be written");

    let parsed = parse_json(&home.join(".mcp.json"));
    assert_eq!(parsed["mcpServers"]["mempal"]["command"], "mempal");
    assert_eq!(
        parsed["mcpServers"]["mempal"]["args"],
        json!(["serve", "--mcp"])
    );
}

#[test]
fn test_install_user_mcp_idempotent_when_already_present() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home");

    let first = install_user_mcp(&home, false, false).expect("first install");
    assert_eq!(first.status, McpInstallStatus::Created);

    let second = install_user_mcp(&home, false, false).expect("second install");
    assert_eq!(second.status, McpInstallStatus::AlreadyPresent);
}

#[test]
fn test_install_user_mcp_merges_with_existing_servers() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    fs::write(
        home.join(".mcp.json"),
        r#"{
          "mcpServers": {
            "other-server": {
              "command": "other",
              "args": ["--bar"]
            }
          }
        }"#,
    )
    .expect("seed mcp.json with other server");

    let outcome = install_user_mcp(&home, false, false).expect("install mcp");
    assert_eq!(outcome.status, McpInstallStatus::Added);

    let parsed = parse_json(&home.join(".mcp.json"));
    assert_eq!(parsed["mcpServers"]["other-server"]["command"], "other");
    assert_eq!(parsed["mcpServers"]["mempal"]["command"], "mempal");
}

#[test]
fn test_install_user_mcp_reports_conflict_on_divergent_entry() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    fs::write(
        home.join(".mcp.json"),
        r#"{
          "mcpServers": {
            "mempal": {
              "command": "/custom/mempal-wrapper",
              "args": ["--special"]
            }
          }
        }"#,
    )
    .expect("seed mcp.json with custom mempal entry");

    let outcome = install_user_mcp(&home, false, false).expect("install mcp");
    assert_eq!(outcome.status, McpInstallStatus::Conflict);

    // Existing user customization must be preserved untouched.
    let parsed = parse_json(&home.join(".mcp.json"));
    assert_eq!(
        parsed["mcpServers"]["mempal"]["command"],
        "/custom/mempal-wrapper"
    );
}

#[test]
fn test_install_user_mcp_dry_run_does_not_write() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home");

    let outcome = install_user_mcp(&home, true, false).expect("dry-run install");
    assert!(!home.join(".mcp.json").exists());
    let rendered = outcome.rendered.expect("dry-run renders preview");
    assert!(rendered.contains("mempal"));
    assert!(rendered.contains("serve"));
}

#[test]
fn test_install_user_mcp_uninstall_removes_only_mempal_entry() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    fs::write(
        home.join(".mcp.json"),
        r#"{
          "mcpServers": {
            "mempal": {
              "command": "mempal",
              "args": ["serve", "--mcp"]
            },
            "other-server": {
              "command": "other",
              "args": ["--bar"]
            }
          }
        }"#,
    )
    .expect("seed mcp.json with mempal + other");

    let outcome = install_user_mcp(&home, false, true).expect("uninstall mcp");
    assert_eq!(outcome.status, McpInstallStatus::Removed);

    let parsed = parse_json(&home.join(".mcp.json"));
    assert!(parsed["mcpServers"].get("mempal").is_none());
    assert_eq!(parsed["mcpServers"]["other-server"]["command"], "other");
}

#[test]
fn test_install_user_mcp_uninstall_removes_empty_file() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir_all(&home).expect("create home");
    fs::write(
        home.join(".mcp.json"),
        r#"{
          "mcpServers": {
            "mempal": {
              "command": "mempal",
              "args": ["serve", "--mcp"]
            }
          }
        }"#,
    )
    .expect("seed mcp.json with only mempal");

    let outcome = install_user_mcp(&home, false, true).expect("uninstall mcp");
    assert_eq!(outcome.status, McpInstallStatus::Removed);
    assert!(
        !home.join(".mcp.json").exists(),
        "uninstall must drop empty .mcp.json instead of leaving a stub"
    );
}

#[test]
fn test_hook_install_skip_mcp_does_not_touch_user_mcp() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&home).expect("create home");

    let output = Command::new(mempal_bin())
        .args(["hook", "install", "--target", "claude-code", "--skip-mcp"])
        .current_dir(&cwd)
        .env("HOME", &home)
        .output()
        .expect("wrapper install with --skip-mcp");
    assert!(
        output.status.success(),
        "install --skip-mcp failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        home.join(".claude/settings.json").exists(),
        "claude settings still written"
    );
    assert!(
        !home.join(".mcp.json").exists(),
        "--skip-mcp must not touch ~/.mcp.json"
    );
}

#[test]
fn test_hook_install_writes_codex_hooks() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&home).expect("create home");

    let outcome = install_codex(&cwd, &home, false, false).expect("install codex");
    let parsed = parse_json(&outcome.write_path);

    assert!(outcome.display_path.ends_with(".codex/hooks.json"));
    assert!(outcome.changed);
    assert_eq!(
        parsed["hooks"]["PostToolUse"][0]["hooks"][0]["command"],
        expected_hook_command("PostToolUse")
    );
    assert_eq!(
        parsed["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"],
        expected_hook_command("UserPromptSubmit")
    );
}

#[test]
fn test_hook_install_codex_merges_existing() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
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
    .expect("write seed codex hooks");

    let outcome = install_codex(&cwd, &home, false, false).expect("install codex");
    let parsed = parse_json(&outcome.write_path);
    let entries = parsed["hooks"]["UserPromptSubmit"]
        .as_array()
        .expect("prompt array");

    // Should have 2 entries in UserPromptSubmit array
    assert_eq!(entries.len(), 2);
    let commands: Vec<&str> = entries
        .iter()
        .flat_map(|entry| entry["hooks"].as_array().expect("hook array").iter())
        .filter_map(|hook| hook["command"].as_str())
        .collect();

    assert!(commands.contains(
        &"mempal cowork-drain --target codex --format codex-hook-json --cwd-source stdin-json"
    ));
    assert!(commands.contains(&expected_hook_command("UserPromptSubmit").as_str()));
}

#[test]
fn test_hook_install_all_target() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&home).expect("create home");

    let output = Command::new(mempal_bin())
        .args(["hook", "install", "--target", "all"])
        .current_dir(&cwd)
        .env("HOME", &home)
        .output()
        .expect("install all");

    assert!(output.status.success());
    assert!(home.join(".claude/settings.json").exists());
    assert!(home.join(".codex/hooks.json").exists());
    assert!(home.join(".mcp.json").exists());
}

#[test]
fn test_hook_install_codex_warns_on_missing_feature_flag() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(&home).expect("create home");

    let output = Command::new(mempal_bin())
        .args(["hook", "install", "--target", "codex"])
        .current_dir(&cwd)
        .env("HOME", &home)
        .output()
        .expect("install codex");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("warning: Codex hook runtime (codex_hooks) is currently disabled"));
    assert!(stdout.contains("codex features enable codex_hooks"));
}

#[test]
fn test_hook_install_codex_no_warning_when_feature_enabled() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(home.join(".codex")).expect("create .codex");
    fs::write(home.join(".codex/config.toml"), "codex_hooks = true").expect("enable hooks");

    let output = Command::new(mempal_bin())
        .args(["hook", "install", "--target", "codex"])
        .current_dir(&cwd)
        .env("HOME", &home)
        .output()
        .expect("install codex");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("warning: Codex hook runtime (codex_hooks) is currently disabled"));
}

#[test]
fn test_hook_install_removes_legacy_aliases() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(cwd.join(".claude")).expect("create local .claude");
    fs::create_dir_all(&home).expect("create home");

    // Seed with a legacy hook command
    let legacy_cmd = "/usr/local/bin/mempal hook hook_post_tool";
    fs::write(
        cwd.join(".claude/settings.json"),
        json!({
          "hooks": {
            "PostToolUse": [{
              "hooks": [{
                "type": "command",
                "command": legacy_cmd
              }]
            }]
          }
        })
        .to_string(),
    )
    .expect("write seed settings");

    let outcome = install_claude_code(&cwd, &home, false, false).expect("install");
    let parsed = parse_json(&outcome.write_path);

    let entries = parsed["hooks"]["PostToolUse"].as_array().expect("array");
    // Should have only 1 entry (the new one), legacy should be gone
    assert_eq!(entries.len(), 1);
    let command = entries[0]["hooks"][0]["command"].as_str().expect("command");
    assert_ne!(command, legacy_cmd);
    assert_eq!(command, expected_hook_command("PostToolUse"));
    assert_eq!(outcome.removed_commands, 1);
}

#[test]
fn test_hook_install_codex_no_warning_when_feature_enabled_in_section() {
    let tmp = TempDir::new().expect("tempdir");
    let cwd = tmp.path().join("repo");
    let home = tmp.path().join("home");
    fs::create_dir_all(&cwd).expect("create cwd");
    fs::create_dir_all(home.join(".codex")).expect("create .codex");
    fs::write(
        home.join(".codex/config.toml"),
        "[features]\ncodex_hooks = true",
    )
    .expect("enable hooks");

    let output = Command::new(mempal_bin())
        .args(["hook", "install", "--target", "codex"])
        .current_dir(&cwd)
        .env("HOME", &home)
        .output()
        .expect("install codex");

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("warning: Codex hook runtime (codex_hooks) is currently disabled"));
}
