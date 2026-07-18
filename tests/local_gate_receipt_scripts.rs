use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> io::Result<Output> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return child.wait_with_output();
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let output = child.wait_with_output()?;
            panic!(
                "child did not exit within {timeout:?}; stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn write_executable(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;

    fs::write(path, contents).expect("write executable fixture");
    let mut permissions = fs::metadata(path)
        .expect("read executable fixture metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).expect("set executable fixture permissions");
}

fn run_bash_script(
    working_dir: &Path,
    script: &Path,
    action: &str,
    aggregate_log: &Path,
) -> Output {
    let child = Command::new("bash")
        .arg(script)
        .arg(action)
        .current_dir(working_dir)
        .env("LOCAL_GATE_FIXTURE_LOG", aggregate_log)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn fixture gate script");
    wait_with_timeout(child, Duration::from_secs(5)).expect("wait for fixture gate script")
}

fn run_git(working_dir: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(working_dir)
        .output()
        .expect("run fixture git command");
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

struct GateFixture {
    _tempdir: tempfile::TempDir,
    root: PathBuf,
    script: PathBuf,
    aggregate_log: PathBuf,
}

fn fixture_script(source: &str, aggregate: &str) -> String {
    const START: &str = "# fixture-aggregate-start";
    const END: &str = "# fixture-aggregate-end";

    let start = source.find(START).expect("fixture start marker exists") + START.len();
    let end = source[start..]
        .find(END)
        .map(|offset| start + offset)
        .expect("fixture end marker exists");
    format!("{}{}\n{}", &source[..start], aggregate, &source[end..])
}

fn fixture(aggregate: &str) -> GateFixture {
    let tempdir = tempfile::tempdir().expect("create fixture directory");
    let root = tempdir.path().to_path_buf();
    let script_dir = root.join("scripts/gates");
    fs::create_dir_all(&script_dir).expect("create fixture script directory");
    let source = fs::read_to_string(repo_root().join("scripts/gates/local-gate-receipt.sh"))
        .expect("read local gate receipt script");
    let script = script_dir.join("local-gate-receipt.sh");
    write_executable(&script, &fixture_script(&source, aggregate));
    fs::write(
        root.join("justfile"),
        "fmt-check:\n    true\n\nquality-gates:\n    true\n\ntest-rest:\n    true\n\nrelease-gate:\n    true\n",
    )
    .expect("write fixture justfile");
    fs::write(root.join("lefthook.yml"), "pre-push:\n  commands: {}\n")
        .expect("write fixture lefthook config");
    fs::write(root.join(".gitignore"), "/target\n").expect("write fixture ignore rules");
    run_git(&root, &["init", "--quiet"]);
    run_git(&root, &["config", "user.email", "gates@example.test"]);
    run_git(&root, &["config", "user.name", "Gate Fixture"]);
    write_executable(
        &root.join(".git/hooks/pre-push"),
        "#!/usr/bin/env bash\nexit 0\n",
    );
    run_git(
        &root,
        &[
            "add",
            ".gitignore",
            "justfile",
            "lefthook.yml",
            "scripts/gates/local-gate-receipt.sh",
        ],
    );
    run_git(&root, &["commit", "--quiet", "-m", "fixture gate"]);

    GateFixture {
        aggregate_log: root.join("aggregate.log"),
        _tempdir: tempdir,
        root,
        script,
    }
}

fn run_fixture(fixture: &GateFixture, action: &str) -> Output {
    run_bash_script(
        &fixture.root,
        &fixture.script,
        action,
        &fixture.aggregate_log,
    )
}

fn receipt_files(fixture: &GateFixture) -> Vec<PathBuf> {
    let receipt_dir = fixture.root.join("target/local-gates/receipts");
    let mut receipts = fs::read_dir(receipt_dir)
        .into_iter()
        .flatten()
        .map(|entry| entry.expect("read receipt entry").path())
        .collect::<Vec<_>>();
    receipts.sort();
    receipts
}

fn successful_aggregate() -> &'static str {
    "\n    printf '%s\\n' aggregate >>\"${LOCAL_GATE_FIXTURE_LOG:?}\"\n    true\n    "
}

#[test]
fn pre_commit_uses_the_fast_gate_without_full_acceptance_commands() {
    let justfile = fs::read_to_string(repo_root().join("justfile")).expect("read justfile");
    let lefthook = fs::read_to_string(repo_root().join("lefthook.yml")).expect("read lefthook");
    let fast_start = justfile
        .find("pre-commit-fast:")
        .expect("fast recipe exists");
    let fast = &justfile[fast_start..]
        .split_once("\n\n")
        .expect("fast recipe ends before next recipe")
        .0;

    assert!(fast.contains("just fmt-check"));
    assert!(fast.contains("just find-monolith-files"));
    assert!(fast.contains("just clippy-fast"));
    for forbidden in [
        "quality-gates",
        "just test",
        "test-onnx",
        "test-rest",
        "release-gate",
        "local-gates",
    ] {
        assert!(
            !fast.contains(forbidden),
            "fast recipe contains {forbidden}"
        );
    }
    assert!(lefthook.contains("run: just pre-commit-fast"));
    assert!(!lefthook.contains("run: just quality-gates"));
}

#[test]
fn producer_publishes_a_clean_exact_tree_pass_receipt() {
    let fixture = fixture(successful_aggregate());

    let output = run_fixture(&fixture, "produce");
    assert!(
        output.status.success(),
        "producer failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.aggregate_log).expect("read aggregate log"),
        "aggregate\n"
    );
    let receipts = receipt_files(&fixture);
    assert_eq!(receipts.len(), 1, "receipts={receipts:?}");
    let receipt = fs::read_to_string(&receipts[0]).expect("read receipt");
    assert!(receipt.contains("schema=local-gate-receipt-v1"));
    assert!(receipt.contains("status=PASS"));
    assert!(receipt.contains(&format!(
        "head={}",
        String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&fixture.root)
                .output()
                .expect("read fixture HEAD")
                .stdout
        )
        .trim()
    )));
    assert!(receipt.contains(&format!(
        "tree={}",
        String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD^{tree}"])
                .current_dir(&fixture.root)
                .output()
                .expect("read fixture tree")
                .stdout
        )
        .trim()
    )));
    let receipt_relative = receipts[0]
        .strip_prefix(&fixture.root)
        .expect("receipt is inside fixture")
        .to_str()
        .expect("UTF-8 receipt path");
    let ignored = Command::new("git")
        .args(["check-ignore", "-q", receipt_relative])
        .current_dir(&fixture.root)
        .status()
        .expect("check ignore rule");
    assert!(ignored.success(), "receipt must be ignored");
}

#[test]
fn producer_never_publishes_pass_after_failure_or_identity_drift() {
    for aggregate in [
        "\n    printf '%s\\n' aggregate >>\"${LOCAL_GATE_FIXTURE_LOG:?}\"\n    false\n    ",
        "\n    printf '%s\\n' aggregate >>\"${LOCAL_GATE_FIXTURE_LOG:?}\"\n    printf 'fmt-check:\\n    true\\n' >justfile\n    git add justfile\n    git commit --quiet -m 'fixture drift'\n    ",
    ] {
        let fixture = fixture(aggregate);
        assert!(!run_fixture(&fixture, "produce").status.success());
        assert_eq!(
            fs::read_to_string(&fixture.aggregate_log).expect("read aggregate log"),
            "aggregate\n"
        );
        assert!(receipt_files(&fixture).is_empty(), "failure minted PASS");
    }
}

#[test]
fn producer_stops_at_an_early_aggregate_failure_without_publishing_pass() {
    let fixture = fixture(
        "\n    printf '%s\\n' before-failure >>\"${LOCAL_GATE_FIXTURE_LOG:?}\"\n    bash -c 'exit 37'\n    printf '%s\\n' after-failure >>\"${LOCAL_GATE_FIXTURE_LOG:?}\"\n    ",
    );

    let output = run_fixture(&fixture, "produce");
    assert_eq!(
        output.status.code(),
        Some(37),
        "producer status changed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.aggregate_log).expect("read aggregate log"),
        "before-failure\n",
        "aggregate continued after its failed command"
    );
    assert!(
        receipt_files(&fixture).is_empty(),
        "early aggregate failure minted PASS"
    );
}

#[test]
fn validator_reuses_only_the_unchanged_exact_tree_without_running_aggregate() {
    let fixture = fixture(successful_aggregate());
    assert!(run_fixture(&fixture, "produce").status.success());
    assert!(run_fixture(&fixture, "validate").status.success());
    assert_eq!(
        fs::read_to_string(&fixture.aggregate_log).expect("read aggregate log"),
        "aggregate\n",
        "validation must not execute the aggregate"
    );
}

#[test]
fn validator_rejects_dirty_changed_missing_and_malformed_receipts() {
    let dirty = fixture(successful_aggregate());
    assert!(run_fixture(&dirty, "produce").status.success());
    fs::write(dirty.root.join("justfile"), "dirty bytes\n").expect("dirty fixture file");
    assert!(!run_fixture(&dirty, "validate").status.success());

    let changed = fixture(successful_aggregate());
    assert!(run_fixture(&changed, "produce").status.success());
    fs::write(changed.root.join("justfile"), "fmt-check:\n    false\n")
        .expect("change fixture tree");
    run_git(&changed.root, &["add", "justfile"]);
    run_git(&changed.root, &["commit", "--quiet", "-m", "changed tree"]);
    assert!(!run_fixture(&changed, "validate").status.success());

    let malformed = fixture(successful_aggregate());
    assert!(run_fixture(&malformed, "produce").status.success());
    fs::write(
        receipt_files(&malformed)
            .into_iter()
            .next()
            .expect("receipt exists"),
        "not a receipt\n",
    )
    .expect("corrupt receipt");
    assert!(!run_fixture(&malformed, "validate").status.success());

    let missing = fixture(successful_aggregate());
    assert!(!run_fixture(&missing, "validate").status.success());
}

#[test]
fn push_reviewed_validates_existing_receipts_without_rerunning_expensive_gates() {
    let justfile = fs::read_to_string(repo_root().join("justfile")).expect("read justfile");
    let push_workflow = &justfile[justfile
        .find("push-reviewed base=\"main\":")
        .expect("push-reviewed recipe exists")..];
    assert!(
        !push_workflow.contains("just local-gates"),
        "push-reviewed must reuse the existing local-gate receipt"
    );
    for forbidden in ["csa review --sa-mode", "csa review --range"] {
        assert!(
            !push_workflow.contains(forbidden),
            "push-reviewed must not launch {forbidden}"
        );
    }
    let local_gate_validation = push_workflow
        .find("bash scripts/gates/local-gate-receipt.sh validate")
        .expect("local-gate receipt validation exists");
    let review_validation = push_workflow
        .find("scripts/hooks/review-check.sh")
        .expect("review receipt validation exists");
    let push = push_workflow
        .find("git push -u origin HEAD")
        .expect("push command exists");

    assert!(
        local_gate_validation < push && review_validation < push,
        "receipt validation must precede push"
    );
    for preserved_tail in [
        "git push -u origin HEAD",
        "gh pr create --base \"{{base}}\"",
        "CREATE_RC=$?",
        "PR already exists. Continuing.",
    ] {
        assert!(
            push_workflow.contains(preserved_tail),
            "push-reviewed lost its PR creation/reuse tail: {preserved_tail}"
        );
    }
}

#[test]
fn push_reviewed_preserves_pre_push_controls() {
    let lefthook = fs::read_to_string(repo_root().join("lefthook.yml")).expect("read lefthook");
    let review_check = fs::read_to_string(repo_root().join("scripts/hooks/review-check.sh"))
        .expect("read review hook");

    for control in ["branch-protection:", "changelog-check:", "review-check:"] {
        assert!(lefthook.contains(control), "missing pre-push {control}");
    }
    assert!(lefthook.contains("local-gate-receipt.sh validate"));
    assert!(!lefthook.contains("run: just local-gates"));
    assert!(review_check.contains("csa review --check-verdict"));
}
