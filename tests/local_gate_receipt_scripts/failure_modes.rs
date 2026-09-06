use super::{
    GateFixture, configure_fixture_git_environment, fixture, fixture_git_command, receipt_files,
    repo_root, run_fixture, spawn_waited_child, successful_aggregate,
    symlink_path_command_to_stable_proxy, wait_with_timeout,
};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

pub(super) fn fixture_justfile() -> &'static str {
    "fmt-check:\n    true\n\nquality-gates:\n    true\n\ntest-rest:\n    true\n\ntest-rest-feature-contract:\n    true\n\nrelease-gate:\n    true\n"
}

const FAULT_KIND_ENV: &str = "LOCAL_GATE_RECEIPT_FAULT_KIND";
const FAULT_LOG_ENV: &str = "LOCAL_GATE_RECEIPT_FAULT_LOG";
const FAULT_ARM_ENV: &str = "LOCAL_GATE_RECEIPT_FAULT_ARM";
const REAL_GIT_ENV: &str = "LOCAL_GATE_RECEIPT_REAL_GIT";
const FAULT_FIXTURE_WAIT_TIMEOUT: Duration = super::FIXTURE_CHILD_WAIT_TIMEOUT;

#[derive(Clone, Copy)]
enum FaultKind {
    Status,
    GitDir,
    GitCommonDir,
    CheckIgnore,
}

impl FaultKind {
    fn label(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::GitDir => "git-dir",
            Self::GitCommonDir => "git-common-dir",
            Self::CheckIgnore => "check-ignore",
        }
    }

    fn selected_args(self) -> &'static [&'static str] {
        match self {
            Self::Status => &["status", "--porcelain=v1", "--untracked-files=no"],
            Self::GitDir => &["rev-parse", "--git-dir"],
            Self::GitCommonDir => &["rev-parse", "--git-common-dir"],
            Self::CheckIgnore => &["check-ignore", "-q", "--", "target"],
        }
    }
}

struct FaultShim {
    bin_dir: PathBuf,
    log: PathBuf,
    arm: Option<PathBuf>,
    kind: FaultKind,
    real_git: PathBuf,
}

fn find_real_executable(name: &str) -> PathBuf {
    let paths = std::env::var_os("PATH").expect("PATH is set");
    std::env::split_paths(&paths)
        .map(|directory| directory.join(name))
        .find(|candidate| {
            fs::metadata(candidate)
                .map(|metadata| metadata.is_file())
                .unwrap_or(false)
        })
        .unwrap_or_else(|| panic!("find real {name} executable"))
}

fn fixture_path(prepend: &Path) -> std::ffi::OsString {
    let inherited_path = std::env::var_os("PATH").expect("PATH is set for fixture");
    std::env::join_paths(
        std::iter::once(prepend.to_path_buf()).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("construct fixture PATH")
}

fn fault_shim(fixture: &GateFixture, kind: FaultKind, arm: bool) -> FaultShim {
    let bin_dir = fixture.root.join(format!("fault-bin-{}", kind.label()));
    fs::create_dir(&bin_dir).expect("create fault shim directory");
    symlink_path_command_to_stable_proxy(&bin_dir, "git");

    FaultShim {
        log: fixture.root.join(format!("fault-{}.log", kind.label())),
        arm: arm.then(|| fixture.root.join("fault-active")),
        kind,
        real_git: find_real_executable("git"),
        bin_dir,
    }
}

fn configure_fault(command: &mut Command, shim: &FaultShim) {
    command
        .env("PATH", fixture_path(&shim.bin_dir))
        .env(FAULT_KIND_ENV, shim.kind.label())
        .env(FAULT_LOG_ENV, &shim.log)
        .env(REAL_GIT_ENV, &shim.real_git);
    if let Some(arm) = &shim.arm {
        command.env(FAULT_ARM_ENV, arm);
    }
}

fn run_faulted_git(fixture: &GateFixture, shim: &FaultShim, args: &[&str]) -> Output {
    let mut command = fixture_git_command(&fixture.root, args);
    configure_fault(&mut command, shim);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = spawn_waited_child(&mut command).expect("spawn bounded faulted git command");
    // Faulted git itself is a direct git shim; keep the short git budget so intentionally
    // stalled children still self-timeout under the stalled-git reaper contract.
    wait_with_timeout(child, super::FIXTURE_GIT_WAIT_TIMEOUT).expect("wait for faulted git command")
}

fn assert_fault_shim_is_selective(fixture: &GateFixture, shim: &FaultShim) {
    if let Some(arm) = &shim.arm {
        fs::write(arm, "active\n").expect("arm fault shim proof");
    }
    let selected = run_faulted_git(fixture, shim, shim.kind.selected_args());
    assert_eq!(
        selected.status.code(),
        Some(42),
        "fault shim selected command"
    );
    if let Some(arm) = &shim.arm {
        fs::remove_file(arm).expect("disarm fault shim proof");
    }

    let delegated = run_faulted_git(fixture, shim, &["rev-parse", "HEAD"]);
    assert!(
        delegated.status.success(),
        "fault shim must delegate another real rev-parse successfully: stderr={}",
        String::from_utf8_lossy(&delegated.stderr)
    );
}

fn run_fixture_with_fault(fixture: &GateFixture, action: &str, shim: &FaultShim) -> Output {
    let mut command = Command::new("/bin/bash");
    command
        .arg(&fixture.script)
        .arg(action)
        .current_dir(&fixture.root)
        .env("LOCAL_GATE_FIXTURE_LOG", &fixture.aggregate_log)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_fixture_git_environment(&mut command, &fixture.root);
    configure_fault(&mut command, shim);
    let child = spawn_waited_child(&mut command).expect("spawn bounded faulted fixture");
    wait_with_timeout(child, FAULT_FIXTURE_WAIT_TIMEOUT).expect("wait for faulted fixture")
}

fn receipt_artifacts(fixture: &GateFixture) -> Vec<PathBuf> {
    let receipt_dir = fixture.root.join("target/local-gates/receipts");
    match fs::read_dir(receipt_dir) {
        Ok(entries) => entries
            .map(|entry| entry.expect("read receipt artifact").path())
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("read receipt directory: {error}"),
    }
}

#[test]
fn producer_rejects_initial_status_command_uncertainty() {
    let fixture = fixture(successful_aggregate());
    let shim = fault_shim(&fixture, FaultKind::Status, false);
    assert_fault_shim_is_selective(&fixture, &shim);

    let output = run_fixture_with_fault(&fixture, "produce", &shim);
    assert!(
        !output.status.success(),
        "producer accepted uncertain initial status: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !fixture.aggregate_log.exists(),
        "aggregate ran after initial status uncertainty"
    );
    assert!(
        receipt_artifacts(&fixture).is_empty(),
        "uncertainty minted PASS"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("PASS local gate receipt:"),
        "uncertainty printed PASS"
    );
}

#[test]
fn producer_rejects_post_aggregate_status_command_uncertainty() {
    let fixture = fixture(
        "\n    printf '%s\\n' aggregate >>\"${LOCAL_GATE_FIXTURE_LOG:?}\"\n    : >\"${LOCAL_GATE_RECEIPT_FAULT_ARM:?}\"\n    ",
    );
    let shim = fault_shim(&fixture, FaultKind::Status, true);
    assert_fault_shim_is_selective(&fixture, &shim);

    let output = run_fixture_with_fault(&fixture, "produce", &shim);
    assert!(
        !output.status.success(),
        "producer accepted uncertain post-aggregate status: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.aggregate_log).expect("read aggregate log"),
        "aggregate\n"
    );
    assert!(
        receipt_artifacts(&fixture).is_empty(),
        "uncertainty minted PASS"
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("PASS local gate receipt:"),
        "uncertainty printed PASS"
    );
}

#[cfg(unix)]
#[test]
fn producer_rejects_check_ignore_uncertainty_for_a_symlinked_target() {
    let fixture = fixture(successful_aggregate());
    let linked_target = tempfile::tempdir().expect("create linked target directory");
    std::os::unix::fs::symlink(linked_target.path(), fixture.root.join("target"))
        .expect("link fixture target directory");
    let shim = fault_shim(&fixture, FaultKind::CheckIgnore, false);
    assert_fault_shim_is_selective(&fixture, &shim);
    fs::remove_file(&shim.log).expect("clear check-ignore fault proof log");

    let output = run_fixture_with_fault(&fixture, "produce", &shim);
    assert!(
        !output.status.success(),
        "producer accepted uncertain check-ignore result: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read_to_string(&shim.log).expect("read check-ignore fault log"),
        "faulted-check-ignore\n"
    );
    assert!(
        !fixture.aggregate_log.exists(),
        "aggregate ran after check-ignore uncertainty"
    );
    assert!(
        receipt_artifacts(&fixture).is_empty(),
        "check-ignore uncertainty minted PASS"
    );
}

#[test]
fn validator_rejects_status_command_uncertainty_without_mutating_receipt() {
    let fixture = fixture(successful_aggregate());
    assert!(run_fixture(&fixture, "produce").status.success());
    let receipt = receipt_files(&fixture)
        .expect("list receipts")
        .into_iter()
        .next()
        .expect("receipt exists");
    let receipt_before = fs::read(&receipt).expect("read receipt before validation");
    let aggregate_before =
        fs::read(&fixture.aggregate_log).expect("read aggregate before validation");
    let shim = fault_shim(&fixture, FaultKind::Status, true);
    assert_fault_shim_is_selective(&fixture, &shim);
    fs::write(shim.arm.as_ref().expect("status arm path"), "active\n").expect("arm status fault");

    let output = run_fixture_with_fault(&fixture, "validate", &shim);
    assert!(
        !output.status.success(),
        "validator accepted uncertain status: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::read(&receipt).expect("read receipt after validation"),
        receipt_before
    );
    assert_eq!(
        fs::read(&fixture.aggregate_log).expect("read aggregate after validation"),
        aggregate_before
    );
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("PASS local gate receipt reused:"),
        "uncertainty reused PASS"
    );
}

fn validator_rejects_rev_parse_uncertainty(kind: FaultKind) {
    let fixture = fixture(successful_aggregate());
    assert!(run_fixture(&fixture, "produce").status.success());
    let shim = fault_shim(&fixture, kind, false);
    assert_fault_shim_is_selective(&fixture, &shim);

    let output = run_fixture_with_fault(&fixture, "validate", &shim);
    assert!(
        !output.status.success(),
        "validator accepted {} uncertainty: stderr={}",
        kind.label(),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn validator_rejects_git_dir_command_uncertainty() {
    validator_rejects_rev_parse_uncertainty(FaultKind::GitDir);
}

#[test]
fn validator_rejects_git_common_dir_command_uncertainty() {
    validator_rejects_rev_parse_uncertainty(FaultKind::GitCommonDir);
}

#[test]
fn review_check_rejects_missing_csa_before_publication_sentinel() {
    let tempdir = tempfile::tempdir().expect("create missing-csa fixture directory");
    let bin_dir = tempdir.path().join("bin");
    fs::create_dir(&bin_dir).expect("create missing-csa PATH");
    let sentinel = tempdir.path().join("publication-success");
    let mut command = Command::new("/bin/bash");
    command
        .args([
            "-c",
            "/bin/bash \"$1\" && : >\"$2\"",
            "--",
            repo_root()
                .join("scripts/hooks/review-check.sh")
                .to_str()
                .expect("UTF-8 review hook path"),
            sentinel.to_str().expect("UTF-8 sentinel path"),
        ])
        .current_dir(repo_root())
        .env("PATH", bin_dir)
        .env_remove("CSA_SESSION_ID")
        .env_remove("CSA_DEPTH")
        .env_remove("CSA_SKIP_REVIEW_CHECK")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = spawn_waited_child(&mut command).expect("spawn bounded missing-csa review check");
    let output = wait_with_timeout(child, FAULT_FIXTURE_WAIT_TIMEOUT)
        .expect("wait for missing-csa review check");

    assert!(
        !output.status.success(),
        "missing csa passed review validation"
    );
    assert!(
        !sentinel.exists(),
        "missing csa reached publication success sentinel"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("review-check requires 'csa' in PATH"),
        "missing-csa diagnostic was not fixed and actionable"
    );
}

#[test]
fn push_reviewed_has_a_fixed_main_base_without_parameters() {
    let justfile = fs::read_to_string(repo_root().join("justfile")).expect("read justfile");
    assert!(
        justfile.contains("push-reviewed:"),
        "push-reviewed has parameters"
    );
    assert!(
        !justfile.contains("push-reviewed base="),
        "push-reviewed accepts an arbitrary base"
    );
    assert!(
        justfile.contains("gh pr create --base main --fill"),
        "push-reviewed must create PRs against canonical main"
    );
    assert!(
        !justfile.contains("{{base}}"),
        "push-reviewed must not interpolate a base"
    );

    let mut command = Command::new("just");
    command
        .args(["--dry-run", "push-reviewed"])
        .current_dir(repo_root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = spawn_waited_child(&mut command).expect("spawn bounded push-reviewed dry run");
    let output = wait_with_timeout(child, FAULT_FIXTURE_WAIT_TIMEOUT)
        .expect("wait for push-reviewed dry run");
    assert!(
        output.status.success(),
        "push-reviewed dry run failed: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let dry_run = [output.stdout, output.stderr].concat();
    assert!(
        String::from_utf8_lossy(&dry_run).contains("gh pr create --base main --fill"),
        "push-reviewed dry run lost canonical main base"
    );
}

#[test]
fn local_gate_aggregate_invokes_rest_feature_contract() {
    let source = fs::read_to_string(repo_root().join("scripts/gates/local-gate-receipt.sh"))
        .expect("read local gate receipt script");
    let start = source
        .find("# fixture-aggregate-start")
        .expect("fixture start marker exists");
    let end = source[start..]
        .find("# fixture-aggregate-end")
        .map(|offset| start + offset)
        .expect("fixture end marker exists");
    let aggregate = &source[start + "# fixture-aggregate-start".len()..end];
    let test_rest = aggregate
        .find("just test-rest\n")
        .expect("aggregate runs test-rest");
    let rest_contract = aggregate
        .find("just test-rest-feature-contract\n")
        .expect("aggregate runs test-rest-feature-contract");
    let release = aggregate
        .find("just release-gate\n")
        .expect("aggregate runs release-gate");
    assert!(
        test_rest < rest_contract && rest_contract < release,
        "rest feature contract must run after test-rest and before release-gate"
    );

    let justfile = fs::read_to_string(repo_root().join("justfile")).expect("read justfile");
    let recipe_start = justfile
        .find("test-rest-feature-contract:")
        .expect("just recipe exists");
    let recipe = &justfile[recipe_start..]
        .split_once("\n\n")
        .expect("recipe ends before next recipe")
        .0;
    assert!(
        recipe.contains("run_contract 1")
            && recipe.contains("run_contract 0 --no-default-features")
            && recipe.contains("--ignored")
            && recipe.contains("--exact")
            && recipe.contains(
                "rest_feature_contract_tests::rest_feature_matches_invocation_expectation",
            )
            && recipe.contains("running 1 test")
            && recipe.contains("1 passed")
            && recipe.contains("MEMPAL_EXPECT_REST must be 0 or 1"),
        "rest feature contract recipe lost dual-mode assertions"
    );

    let fixture = fixture(aggregate);
    let output = run_fixture(&fixture, "produce");
    assert!(
        output.status.success(),
        "literal aggregate recipes failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        receipt_files(&fixture).expect("list receipts").len(),
        1,
        "literal aggregate must still publish exactly one PASS receipt"
    );
}
