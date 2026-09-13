use std::process::{Command, Stdio};

use crate::repo_root;

#[test]
fn rest_gate_runs_lib_worker_tests_in_a_dedicated_cargo_process() {
    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let fixture = tempfile::tempdir().expect("create dry-run fixture");
    let target = fixture.path().join("target");
    let output = Command::new("/bin/bash")
        .arg(&script)
        .current_dir(repo_root())
        .env("REST_GATE_DRY_RUN", "1")
        .env(
            "REST_GATE_TARGET_DIR",
            target.to_str().expect("UTF-8 target path"),
        )
        .env("REST_TEST_TARGETS_PER_BATCH", "999")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run rest-tests dry-run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let cargo_test_lines: Vec<&str> = stdout
        .lines()
        .filter(|line| line.contains(" cargo test "))
        .collect();
    assert!(
        cargo_test_lines
            .iter()
            .any(|line| line.contains("--lib --bins") && line.contains("--skip test_worker_")),
        "parallel rest-lib must skip test_worker_: stdout={stdout}"
    );
    assert!(
        cargo_test_lines
            .iter()
            .any(|line| line.contains("--lib test_worker_") && !line.contains("--bins")),
        "test_worker_ must run in a dedicated cargo process: stdout={stdout}"
    );
}

#[test]
fn rest_gate_runs_mcp_coexistence_without_included_local_gate_child_suites() {
    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let fixture = tempfile::tempdir().expect("create dry-run fixture");
    let target = fixture.path().join("target");
    let output = Command::new("/bin/bash")
        .arg(&script)
        .current_dir(repo_root())
        .env("REST_GATE_DRY_RUN", "1")
        .env(
            "REST_GATE_TARGET_DIR",
            target.to_str().expect("UTF-8 target path"),
        )
        .env("REST_TEST_TARGETS_PER_BATCH", "999")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("run rest-tests dry-run");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mcp_lines: Vec<&str> = stdout
        .lines()
        .filter(|line| {
            line.contains(" cargo test ") && line.contains("--test daemon_mcp_coexistence")
        })
        .collect();
    assert_eq!(
        mcp_lines.len(),
        3,
        "rest MCP coexistence must keep three explicit cargo lanes: stdout={stdout}"
    );
    assert!(
        mcp_lines.iter().any(|line| {
            line.contains("--skip mcp_lifecycle_timeouts_reap_hostile_children")
                && line.contains("--skip local_gate_child::")
        }),
        "coexistence initialize must not share a cargo with included local_gate_child suites: stdout={stdout}"
    );
    assert!(
        mcp_lines.iter().any(|line| {
            line.contains("daemon_mcp_coexistence local_gate_child::")
                && !line.contains("--skip local_gate_child::")
        }),
        "included local_gate_child suites must keep a dedicated cargo process: stdout={stdout}"
    );
    assert!(
        mcp_lines.iter().any(|line| {
            line.contains("daemon_mcp_coexistence mcp_lifecycle_timeouts_reap_hostile_children")
                && !line.contains("--skip mcp_lifecycle_timeouts_reap_hostile_children")
        }),
        "hostile-child lifecycle must keep a dedicated cargo process: stdout={stdout}"
    );
}
