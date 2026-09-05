//! Isolated-process regression: ordinary `run_context` must env_clear + HOME.
//!
//! #1085 routed helpers through `cli_deadline` but must not inherit ambient
//! `MEMPAL_EMBED_*` overrides. The child process receives the hostile env;
//! the parent test process is left unchanged.

use std::process::Command;

use super::super::openai_embedding_stub;
use super::super::{run_context, setup_cli_home, vector, write_cli_api_config};

const CHILD_MARKER: &str = "MEMPAL_CONTEXT_ENV_ISOLATION_CHILD";
const TEST_FILTER: &str =
    "deadline::env_isolation::test_context_cli_clears_inherited_embed_override";

#[cfg(target_os = "linux")]
#[test]
fn test_context_cli_clears_inherited_embed_override() {
    if std::env::var_os(CHILD_MARKER).is_some() {
        assert_clears_hostile_embed_override();
        return;
    }

    let output = Command::new(std::env::current_exe().expect("current test executable"))
        .args(["--exact", TEST_FILTER, "--nocapture", "--test-threads=1"])
        .env(CHILD_MARKER, "1")
        .env("MEMPAL_EMBED_BACKEND", "stub")
        .env("MEMPAL_EMBED_BASE_URL", "http://127.0.0.1:1/v1")
        .env("MEMPAL_EMBED_MODEL", "hostile-model")
        .env("MEMPAL_EMBED_DIM", "1")
        .output()
        .expect("spawn isolation child");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "isolation child failed: stdout={stdout} stderr={stderr}"
    );
    assert!(
        stdout.contains("running 1 test"),
        "child filter must select the isolation case, stdout={stdout}"
    );
    assert!(
        stdout.contains("test result: ok"),
        "child must pass after env_clear, stdout={stdout}"
    );
}

fn assert_clears_hostile_embed_override() {
    let (tmp, _db) = setup_cli_home();
    let query = "isolation-hostile-embed";
    let stub = openai_embedding_stub::start(query, vector());
    write_cli_api_config(tmp.path(), stub.endpoint());
    let output = run_context(
        tmp.path(),
        vec![
            "context".to_string(),
            query.to_string(),
            "--format".to_string(),
            "json".to_string(),
        ],
    );
    let outcome = stub.stop_and_join();
    assert!(
        output.status.success(),
        "context command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        outcome,
        openai_embedding_stub::StubOutcome::Served,
        "inherited MEMPAL_EMBED_* must not override fixture API config"
    );
}
