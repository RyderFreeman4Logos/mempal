use std::time::Duration;

use crate::{repo_root, run_bash_script};

#[test]
fn rest_gate_runs_lib_worker_tests_in_a_dedicated_cargo_process() {
    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let fixture = tempfile::tempdir().expect("create dry-run fixture");
    let target = fixture.path().join("target");
    let output = run_bash_script(
        &script,
        &[],
        &[
            ("REST_GATE_DRY_RUN", "1"),
            (
                "REST_GATE_TARGET_DIR",
                target.to_str().expect("UTF-8 target path"),
            ),
            ("REST_TEST_TARGETS_PER_BATCH", "999"),
        ],
        Duration::from_secs(10),
    );

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
