use super::super::{
    FIXTURE_CHILD_WAIT_TIMEOUT, configure_fixture_git_environment, repo_root, run_git,
    spawn_waited_child, wait_with_timeout,
};
use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};

fn run_temp_fixture_check(root: &Path, git_index: Option<&Path>) -> Output {
    let mut command = Command::new("/bin/bash");
    command
        .arg(repo_root().join("scripts/gates/check-test-temp-fixtures.sh"))
        .current_dir(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(git_index) = git_index {
        command.env("GIT_INDEX_FILE", git_index);
    }
    configure_fixture_git_environment(&mut command, root);
    let child = spawn_waited_child(&mut command).expect("spawn temp fixture check");
    wait_with_timeout(child, FIXTURE_CHILD_WAIT_TIMEOUT).expect("wait for temp fixture check")
}

#[test]
fn temp_fixture_check_reads_staged_bytes_not_divergent_worktree_bytes() {
    let tempdir = tempfile::tempdir().expect("create temp fixture check repo");
    let root = tempdir.path();
    run_git(root, &["init", "--quiet"]);
    fs::write(
        root.join("probe.rs"),
        "let temp = TempDir::new_in(\"/tmp\");\n",
    )
    .expect("write staged violation");
    run_git(root, &["add", "probe.rs"]);
    fs::write(
        root.join("probe.rs"),
        "let temp = TempDir::new_in(temp_root);\n",
    )
    .expect("write clean divergent worktree");

    let output = run_temp_fixture_check(root, None);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stdout).contains("TempDir::new_in(\"/tmp\")"));

    run_git(root, &["add", "probe.rs"]);
    assert!(run_temp_fixture_check(root, None).status.success());
}

#[test]
fn temp_fixture_check_propagates_git_grep_operational_errors() {
    let tempdir = tempfile::tempdir().expect("create temp fixture check repo");
    let root = tempdir.path();
    run_git(root, &["init", "--quiet"]);
    fs::write(root.join("probe.rs"), "fn probe() {}\n").expect("write clean source");
    run_git(root, &["add", "probe.rs"]);
    let broken_index = root.join("broken-index");
    fs::write(&broken_index, "not a git index\n").expect("write broken index");

    let output = run_temp_fixture_check(root, Some(&broken_index));
    assert_eq!(output.status.code(), Some(128));
    assert!(String::from_utf8_lossy(&output.stderr).contains("fatal:"));
}
