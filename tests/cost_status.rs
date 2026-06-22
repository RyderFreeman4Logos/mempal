use std::fs;
use std::process::{Command, Output};

use mempal::core::db::Database;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

fn run_mempal(home: &TempDir, args: &[&str]) -> Output {
    let mut command = Command::new(mempal_bin());
    command
        .args(args)
        .env("HOME", home.path())
        .env_remove("MEMPAL_EMBED_BACKEND")
        .env_remove("MEMPAL_EMBED_BASE_URL")
        .env_remove("MEMPAL_EMBED_MODEL")
        .env_remove("MEMPAL_EMBED_DIM");
    command.output().expect("run mempal")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout utf8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr utf8")
}

#[test]
fn cost_status_missing_config_reports_missing_embedding_endpoint() {
    let home = TempDir::new().expect("temp home");

    let output = run_mempal(&home, &["cost", "status"]);

    assert!(
        output.status.success(),
        "status={:?}\nstderr={}",
        output.status,
        stderr(&output)
    );
    let stdout = stdout(&output);
    assert!(stdout.contains("embedding:"));
    assert!(stdout.contains("status: misconfigured"));
    assert!(stdout.contains("openai_compat endpoint is missing"));
    assert!(stdout.contains("llm:"));
    assert!(stdout.contains("rerank:"));
    assert!(stdout.contains("status: disabled"));
    assert!(!stdout.contains("api_key"));
}

#[test]
fn cost_status_redacts_remote_endpoint_secrets() {
    let home = TempDir::new().expect("temp home");
    let mempal_dir = home.path().join(".mempal");
    fs::create_dir_all(&mempal_dir).expect("create .mempal");
    fs::write(
        mempal_dir.join("config.toml"),
        r#"
[privacy.remote_calls]
fail_closed = true

[embed]
backend = "openai_compat"
base_url = "https://api.openai.com/v1/private-embed-path"
api_model = "text-embedding-3-large"

[embed.openai_compat]
api_key_env = "MEMPAL_SECRET_TOKEN_ENV"

[llm]
enabled = true
base_url = "https://llm.example.com/v1/private-chat-path"
model = "judge"
api_key = "sk-secret-should-not-print"
enabled_for = ["gating"]

[search.reranker]
enabled = true
endpoint = "https://rerank.example.com/private-rerank-path"
model = "rerank"
"#,
    )
    .expect("write config");

    let output = run_mempal(&home, &["cost", "status"]);

    assert!(
        output.status.success(),
        "status={:?}\nstderr={}",
        output.status,
        stderr(&output)
    );
    let stdout = stdout(&output);
    assert!(stdout.contains("<remote-endpoint>"));
    assert!(stdout.contains("status: remote_endpoint"));
    assert!(stdout.contains("policy: blocked_by_policy"));
    assert!(!stdout.contains("api.openai.com"));
    assert!(!stdout.contains("llm.example.com"));
    assert!(!stdout.contains("rerank.example.com"));
    assert!(!stdout.contains("private-embed-path"));
    assert!(!stdout.contains("private-chat-path"));
    assert!(!stdout.contains("private-rerank-path"));
    assert!(!stdout.contains("sk-secret-should-not-print"));
    assert!(!stdout.contains("MEMPAL_SECRET_TOKEN_ENV"));
    assert!(!stdout.contains("api_key"));
}

#[test]
fn status_full_reports_blocked_remote_probes_without_secrets() {
    let home = TempDir::new().expect("temp home");
    let mempal_dir = home.path().join(".mempal");
    fs::create_dir_all(&mempal_dir).expect("create .mempal");
    Database::open(&mempal_dir.join("palace.db")).expect("create db");
    fs::write(
        mempal_dir.join("config.toml"),
        r#"
[privacy.remote_calls]
fail_closed = true

[embed]
backend = "openai_compat"
base_url = "https://api.openai.com/v1/private-embed-path"
api_model = "text-embedding-3-large"

[embed.openai_compat]
api_key_env = "MEMPAL_SECRET_TOKEN_ENV"

[llm]
enabled = true
base_url = "https://llm.example.com/v1/private-chat-path"
model = "judge"
api_key = "sk-secret-should-not-print"
"#,
    )
    .expect("write config");

    let output = run_mempal(&home, &["status", "--full"]);

    assert!(
        output.status.success(),
        "status={:?}\nstderr={}",
        output.status,
        stderr(&output)
    );
    let stdout = stdout(&output);
    assert!(stdout.contains("Endpoints:"), "{stdout}");
    assert!(stdout.contains("skipped"), "{stdout}");
    assert!(
        stdout.contains("privacy.remote_calls.fail_closed"),
        "{stdout}"
    );
    assert!(stdout.contains("allow_embedding"), "{stdout}");
    assert!(stdout.contains("allow_llm"), "{stdout}");
    assert!(stdout.contains("<remote-endpoint>"), "{stdout}");
    assert!(!stdout.contains("api.openai.com"), "{stdout}");
    assert!(!stdout.contains("llm.example.com"), "{stdout}");
    assert!(!stdout.contains("private-embed-path"), "{stdout}");
    assert!(!stdout.contains("private-chat-path"), "{stdout}");
    assert!(!stdout.contains("sk-secret-should-not-print"), "{stdout}");
    assert!(!stdout.contains("MEMPAL_SECRET_TOKEN_ENV"), "{stdout}");
    assert!(!stdout.contains("api_key"), "{stdout}");
}
