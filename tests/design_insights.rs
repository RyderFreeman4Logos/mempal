use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use mempal::core::db::Database;
use serde_json::Value;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn palace_db_path(home: &TempDir) -> PathBuf {
    home.path().join(".mempal/palace.db")
}

fn setup_home() -> TempDir {
    let home = TempDir::new().expect("home");
    Database::open(&palace_db_path(&home)).expect("open db");
    home
}

fn run_mempal(home: &Path, args: &[&str]) -> Output {
    Command::new(mempal_bin())
        .env("HOME", home)
        .env("MEMPAL_EMBED_BACKEND", "stub")
        .args(args)
        .output()
        .expect("run mempal")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

#[test]
fn test_cli_design_insight_record_list_resolve_and_redact() {
    let home = setup_home();
    let secret = "supersecret12345";
    let provider_secret = "providerfixture12345";
    let secret_key_fixture = "secretkeyfixture12345";
    let json_provider_secret = "jsonproviderfixture54321";
    let json_github_token = "jsongithubfixture54321";
    let json_jwt_secret = "jsonjwtfixture54321";
    let json_prompt = "jsonpromptfixture54321";
    let json_user_prompt = "jsonuserpromptfixture54321";
    let evidence = format!("https://user:pass@example.test/session?token={secret}");
    let summary = format!(
        r#"Endpoint outage should produce a reusable retry rule without storing Bearer {secret}; provider body {{"OPENAI_API_KEY":"{json_provider_secret}","prompt":"{json_prompt} with spaces"}}."#
    );
    let rule = format!(
        r#"password={secret}
raw prompt: copy the user's full request
provider body {{"JWT_SECRET_KEY":"{json_jwt_secret} with spaces","user_prompt":"{json_user_prompt} with spaces"}}"#
    );
    let project = format!(
        r#"OPENAI_API_KEY={provider_secret} DJANGO_SECRET_KEY={secret_key_fixture} {{"GITHUB_TOKEN":"{json_github_token}"}}"#
    );

    let record = run_mempal(
        home.path(),
        &[
            "insight",
            "record",
            "--source",
            "user-idea",
            "--scope",
            "issue",
            "--target",
            "github-issue",
            "--evidence",
            &evidence,
            "--summary",
            &summary,
            "--rule",
            &rule,
            "--project",
            &project,
            "--priority",
            "5",
            "--json",
        ],
    );
    assert_success(&record);
    let record_stdout = stdout(&record);
    assert!(!record_stdout.contains(secret), "{record_stdout}");
    assert!(!record_stdout.contains(provider_secret), "{record_stdout}");
    assert!(
        !record_stdout.contains(secret_key_fixture),
        "{record_stdout}"
    );
    for leaked in [
        json_provider_secret,
        json_github_token,
        json_jwt_secret,
        json_prompt,
        json_user_prompt,
    ] {
        assert!(!record_stdout.contains(leaked));
    }
    assert!(!record_stdout.contains("user:pass"), "{record_stdout}");
    assert!(
        !record_stdout.contains("copy the user's full request"),
        "{record_stdout}"
    );
    let record_value: Value = serde_json::from_str(&record_stdout).expect("record json");
    let insight_id = record_value["id"].as_str().expect("insight id").to_string();
    assert!(insight_id.starts_with("insight_"));
    assert_eq!(record_value["status"], "open");
    assert_eq!(record_value["priority"], 5);
    assert_eq!(
        record_value["project_id"],
        r#"OPENAI_API_KEY=<redacted> DJANGO_SECRET_KEY=<redacted> {"GITHUB_TOKEN":"<redacted>"}"#
    );
    assert!(record_value["redaction_count"].as_u64().unwrap_or(0) >= 12);

    let db = Database::open(&palace_db_path(&home)).expect("reopen db");
    let stored: (String, String, Option<String>, Option<String>) = db
        .conn()
        .query_row(
            "SELECT evidence_ref, summary, rule_text, project_id FROM design_insights WHERE id = ?1",
            [insight_id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("stored insight");
    let stored_text = format!(
        "{} {} {} {}",
        stored.0,
        stored.1,
        stored.2.as_deref().unwrap_or_default(),
        stored.3.as_deref().unwrap_or_default()
    );
    for leaked in [
        secret,
        provider_secret,
        secret_key_fixture,
        json_provider_secret,
        json_github_token,
        json_jwt_secret,
        json_prompt,
        json_user_prompt,
    ] {
        assert!(!stored_text.contains(leaked));
    }

    let list = run_mempal(
        home.path(),
        &["insight", "list", "--status", "open", "--json"],
    );
    assert_success(&list);
    let list_stdout = stdout(&list);
    assert!(!list_stdout.contains(secret), "{list_stdout}");
    assert!(!list_stdout.contains(provider_secret), "{list_stdout}");
    assert!(!list_stdout.contains(secret_key_fixture), "{list_stdout}");
    for leaked in [
        json_provider_secret,
        json_github_token,
        json_jwt_secret,
        json_prompt,
        json_user_prompt,
    ] {
        assert!(!list_stdout.contains(leaked));
    }
    let rows: Value = serde_json::from_str(&list_stdout).expect("list json");
    assert_eq!(rows.as_array().expect("rows").len(), 1);
    assert_eq!(rows[0]["id"], insight_id);

    let status = run_mempal(home.path(), &["status"]);
    assert_success(&status);
    let status_stdout = stdout(&status);
    assert!(
        status_stdout.contains("Design Insights:") && status_stdout.contains("high_value_open: 1"),
        "{status_stdout}"
    );

    let doctor = run_mempal(home.path(), &["doctor", "--format", "json"]);
    assert_success(&doctor);
    let doctor_value: Value = serde_json::from_str(&stdout(&doctor)).expect("doctor json");
    assert_eq!(doctor_value["design_insights"]["open_total"], 1);
    assert_eq!(doctor_value["design_insights"]["high_value_open"], 1);
    assert!(
        doctor_value["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning
                .as_str()
                .unwrap_or_default()
                .contains("design insight")),
        "{doctor_value:#}"
    );

    let resolve = run_mempal(
        home.path(),
        &[
            "insight",
            "resolve",
            &insight_id,
            "--actor",
            "codex",
            "--note",
            "drained into issue acceptance criteria",
            "--json",
        ],
    );
    assert_success(&resolve);
    let resolve_value: Value = serde_json::from_str(&stdout(&resolve)).expect("resolve json");
    assert_eq!(resolve_value["resolved"], true);

    let list_after = run_mempal(
        home.path(),
        &["insight", "list", "--status", "open", "--json"],
    );
    assert_success(&list_after);
    let rows_after: Value = serde_json::from_str(&stdout(&list_after)).expect("list json");
    assert_eq!(rows_after.as_array().expect("rows").len(), 0);
}
