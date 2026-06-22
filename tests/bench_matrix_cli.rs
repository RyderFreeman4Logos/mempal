use std::fs;
use std::process::{Command, Output};

use serde_json::Value;
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
fn bench_matrix_json_default_runs_no_llm_fixture_without_provider_calls() {
    let home = TempDir::new().expect("temp home");

    let output = run_mempal(&home, &["bench", "matrix", "--format", "json"]);

    assert!(
        output.status.success(),
        "status={:?}\nstderr={}",
        output.status,
        stderr(&output)
    );
    let parsed: Value = serde_json::from_str(&stdout(&output)).expect("parse matrix json");
    assert_eq!(parsed["schema_version"], "mempal.benchmark_matrix.v1");
    assert_eq!(parsed["dataset"]["id"], "builtin_recall_citation_v1");
    assert_eq!(parsed["runs"].as_array().expect("runs array").len(), 1);
    assert_eq!(parsed["runs"][0]["mode"], "no_llm");
    assert_eq!(parsed["runs"][0]["retrieval_engine"], "sqlite_fts_bm25");
    assert_eq!(parsed["runs"][0]["remote_calls"]["total"], 0);
    assert_eq!(
        parsed["reproducibility"]["provider_calls_enabled"],
        Value::Bool(false)
    );
    assert!(parsed["runs"][0]["recall"]["recall_at_k"].as_f64().unwrap() > 0.0);
    assert!(
        parsed["runs"][0]["citation"]["correctness_at_k"]
            .as_f64()
            .unwrap()
            > 0.0
    );
}

#[test]
fn bench_matrix_all_modes_distinguishes_configs_without_leaking_remote_details() {
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
"#,
    )
    .expect("write config");

    let output = run_mempal(
        &home,
        &["bench", "matrix", "--mode", "all", "--format", "json"],
    );

    assert!(
        output.status.success(),
        "status={:?}\nstderr={}",
        output.status,
        stderr(&output)
    );
    let body = stdout(&output);
    let parsed: Value = serde_json::from_str(&body).expect("parse matrix json");
    let modes = parsed["runs"]
        .as_array()
        .expect("runs array")
        .iter()
        .map(|run| run["mode"].as_str().expect("mode"))
        .collect::<Vec<_>>();
    assert_eq!(modes, vec!["no_llm", "local_llm", "cloud_llm"]);
    for run in parsed["runs"].as_array().expect("runs array") {
        assert_eq!(run["remote_calls"]["total"], 0);
        assert_eq!(run["remote_calls"]["estimated_cost_usd"], 0.0);
        assert_eq!(run["provider_execution"], "disabled_deterministic_fixture");
    }
    assert!(body.contains("\"status\": \"remote_endpoint\""));
    assert!(!body.contains("api.openai.com"));
    assert!(!body.contains("llm.example.com"));
    assert!(!body.contains("private-embed-path"));
    assert!(!body.contains("private-chat-path"));
    assert!(!body.contains("MEMPAL_SECRET_TOKEN_ENV"));
    assert!(!body.contains("sk-secret-should-not-print"));
    assert!(!body.contains("api_key"));
}
