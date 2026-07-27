use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[path = "local_gate_receipt_scripts/failure_modes.rs"]
mod failure_modes;

/// Bounded wait for local-gate fixture *script* children under load / cold `target` rebuilds.
/// Keep well under cargo test timeouts while remaining larger than the old 5s flake budget.
const FIXTURE_CHILD_WAIT_TIMEOUT: Duration = Duration::from_secs(30);
/// Direct fixture `git` commands stay short: they should finish quickly unless intentionally
/// stalled, and the stalled-git reaper test outer RED bound assumes a ~5s self-timeout.
const FIXTURE_GIT_WAIT_TIMEOUT: Duration = Duration::from_secs(5);

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
            kill_waited_child_process_group(&mut child)?;
            let output = child.wait_with_output()?;
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "child did not exit within {timeout:?}; stdout={}, stderr={}",
                    bounded_diagnostic(&output.stdout),
                    bounded_diagnostic(&output.stderr)
                ),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(unix)]
fn spawn_waited_child(command: &mut Command) -> io::Result<Child> {
    use std::os::unix::process::CommandExt;

    command.process_group(0).spawn()
}

#[cfg(unix)]
fn kill_waited_child_process_group(child: &mut Child) -> io::Result<()> {
    let pid = i32::try_from(child.id()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "waited child PID exceeds i32 range",
        )
    })?;
    // Every wait_with_timeout caller uses spawn_waited_child, so this targets only the
    // child-owned process group and clears descendants that inherited captured pipes.
    if unsafe { libc::kill(-pid, libc::SIGKILL) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(unix))]
fn spawn_waited_child(command: &mut Command) -> io::Result<Child> {
    command.spawn()
}

#[cfg(not(unix))]
fn kill_waited_child_process_group(child: &mut Child) -> io::Result<()> {
    child.kill()
}

fn bounded_diagnostic(output: &[u8]) -> String {
    const MAX_DIAGNOSTIC_BYTES: usize = 8 * 1024;

    String::from_utf8_lossy(&output[..output.len().min(MAX_DIAGNOSTIC_BYTES)]).into_owned()
}

fn configure_fixture_git_environment(command: &mut Command, working_dir: &Path) {
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_COUNT", "2")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "false")
        .env("GIT_CONFIG_KEY_1", "core.hooksPath")
        .env("GIT_CONFIG_VALUE_1", working_dir.join(".fixture-git-hooks"));
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

#[cfg(unix)]
fn assert_path_command_is_stable_proxy(command: &Path) {
    assert!(
        fs::symlink_metadata(command)
            .expect("read fixture PATH command metadata")
            .file_type()
            .is_symlink(),
        "fixture PATH command must be a symlink: {}",
        command.display()
    );
    assert_eq!(
        fs::canonicalize(command).expect("canonical fixture PATH command"),
        fs::canonicalize(repo_root().join("tests/fixtures/local-gate-command-proxy.sh"))
            .expect("canonical committed command proxy"),
        "fixture PATH command must target the committed proxy"
    );
}

#[cfg(unix)]
fn symlink_path_command_to_stable_proxy(bin_dir: &Path, command_name: &str) {
    let command = bin_dir.join(command_name);
    std::os::unix::fs::symlink(
        repo_root().join("tests/fixtures/local-gate-command-proxy.sh"),
        &command,
    )
    .expect("link fixture PATH command to committed proxy");
    assert_path_command_is_stable_proxy(&command);
}

fn run_bash_script(
    working_dir: &Path,
    script: &Path,
    action: &str,
    aggregate_log: &Path,
) -> Output {
    let mut command = Command::new("/bin/bash");
    command
        .arg(script)
        .arg(action)
        .current_dir(working_dir)
        .env("LOCAL_GATE_FIXTURE_LOG", aggregate_log)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_fixture_git_environment(&mut command, working_dir);
    let child = spawn_waited_child(&mut command).expect("spawn fixture gate script");
    wait_with_timeout(child, FIXTURE_CHILD_WAIT_TIMEOUT).expect("wait for fixture gate script")
}

fn fixture_git_command(working_dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command.args(args).current_dir(working_dir);
    configure_fixture_git_environment(&mut command, working_dir);
    command
}

fn run_fixture_git_with_timeout(working_dir: &Path, args: &[&str]) -> io::Result<Output> {
    let mut command = fixture_git_command(working_dir, args);
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let child = spawn_waited_child(&mut command)?;
    wait_with_timeout(child, FIXTURE_GIT_WAIT_TIMEOUT)
}

fn fixture_git_output(working_dir: &Path, args: &[&str]) -> Output {
    let output = run_fixture_git_with_timeout(working_dir, args)
        .unwrap_or_else(|error| panic!("git {args:?} failed: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn run_git(working_dir: &Path, args: &[&str]) {
    let _ = fixture_git_output(working_dir, args);
}

const STALLED_GIT_CHILD_ENV: &str = "LOCAL_GATE_RECEIPT_STALLED_GIT_CHILD";
const STALLED_GIT_PID_FILE_ENV: &str = "LOCAL_GATE_RECEIPT_STALLED_GIT_PID_FILE";

fn stalled_fixture_git(working_dir: &Path) -> io::Result<Output> {
    run_fixture_git_with_timeout(working_dir, &["rev-parse", "HEAD"])
}

fn wait_for_test_child(mut child: Child, timeout: Duration) -> (Output, bool) {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait().expect("poll bounded test child").is_some() {
            return (
                child
                    .wait_with_output()
                    .expect("collect bounded test child output"),
                false,
            );
        }
        if Instant::now() >= deadline {
            child.kill().expect("kill bounded test child");
            return (
                child.wait_with_output().expect("reap bounded test child"),
                true,
            );
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(target_os = "linux")]
fn process_start_time(pid: i32) -> io::Result<Option<String>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let start_time = stat
        .rsplit_once(") ")
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "malformed /proc stat"))?
        .1
        .split_ascii_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing /proc start time"))?;
    Ok(Some(start_time.to_owned()))
}

#[cfg(target_os = "linux")]
fn wait_for_process_exit(pid: i32, start_time: &str, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        match process_start_time(pid)? {
            None => return Ok(true),
            Some(current) if current != start_time => return Ok(true),
            Some(_) => {}
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(target_os = "linux")]
fn terminate_matching_process(pid: i32, start_time: &str) -> io::Result<()> {
    if process_start_time(pid)?.as_deref() == Some(start_time) {
        // SAFETY: the PID's Linux start-time identity was revalidated immediately before signal.
        unsafe {
            let _ = libc::kill(pid, libc::SIGKILL);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn fixture_git_commands_time_out_and_reap_stalled_direct_children() {
    if std::env::var_os(STALLED_GIT_CHILD_ENV).is_some() {
        let error = stalled_fixture_git(&repo_root())
            .expect_err("stalled fixture git command must reach its internal timeout");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        return;
    }

    let tempdir = tempfile::tempdir().expect("create stalled git fixture directory");
    let fake_bin_dir = tempdir.path().join("bin");
    fs::create_dir(&fake_bin_dir).expect("create stalled fake binary directory");
    let pid_file = tempdir.path().join("stalled-git.pid");
    symlink_path_command_to_stable_proxy(&fake_bin_dir, "git");

    let inherited_path = std::env::var_os("PATH").expect("PATH is set for fixture");
    let path = std::env::join_paths(
        std::iter::once(fake_bin_dir).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("construct stalled fixture PATH");
    let child = Command::new(std::env::current_exe().expect("current test executable"))
        .args([
            "--exact",
            "fixture_git_commands_time_out_and_reap_stalled_direct_children",
            "--nocapture",
        ])
        .env(STALLED_GIT_CHILD_ENV, "1")
        .env(STALLED_GIT_PID_FILE_ENV, &pid_file)
        .env("PATH", path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn bounded stalled-git test child");
    let started = Instant::now();
    let (output, outer_timed_out) = wait_for_test_child(child, Duration::from_secs(7));
    let elapsed = started.elapsed();

    let pid_record = fs::read_to_string(&pid_file).expect("read stalled git PID record");
    let (pid, start_time) = pid_record
        .split_once(' ')
        .expect("parse stalled git PID record");
    let pid = pid.trim().parse::<i32>().expect("parse stalled git PID");
    let start_time = start_time.trim();
    let reaped = wait_for_process_exit(pid, start_time, Duration::from_secs(1))
        .expect("verify stalled git child identity");
    if !reaped {
        terminate_matching_process(pid, start_time).expect("terminate matching stalled git child");
    }

    assert!(
        !outer_timed_out,
        "outer RED bound killed the test child after {elapsed:?}; fixture git did not self-timeout; stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.status.success(),
        "stalled-git test child failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "timeout exceeded headroom: {elapsed:?}"
    );
    assert!(reaped, "stalled direct git child was not reaped");
}

fn run_review_check(fake_bin_dir: &Path, log: &Path, csa_context: (&str, &str)) -> Output {
    let inherited_path = std::env::var_os("PATH").expect("PATH is set for fixture");
    let path = std::env::join_paths(
        std::iter::once(fake_bin_dir.to_path_buf()).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("construct fixture PATH");
    let mut command = Command::new("/bin/bash");
    command
        .arg(repo_root().join("scripts/hooks/review-check.sh"))
        .current_dir(repo_root())
        .env("PATH", path)
        .env("REVIEW_CHECK_FIXTURE_LOG", log)
        .env_remove("CSA_SESSION_ID")
        .env_remove("CSA_DEPTH")
        .env_remove("CSA_SKIP_REVIEW_CHECK")
        .env(csa_context.0, csa_context.1)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = spawn_waited_child(&mut command).expect("spawn review-check fixture");
    wait_with_timeout(child, FIXTURE_CHILD_WAIT_TIMEOUT).expect("wait for review-check fixture")
}

#[test]
fn review_check_validates_missing_receipts_inside_automatic_csa_contexts() {
    let tempdir = tempfile::tempdir().expect("create review-check fixture directory");
    let fake_bin_dir = tempdir.path().join("bin");
    fs::create_dir(&fake_bin_dir).expect("create fake binary directory");
    symlink_path_command_to_stable_proxy(&fake_bin_dir, "csa");

    for (case, context) in [
        ("session", ("CSA_SESSION_ID", "synthetic-review-gate-probe")),
        ("depth", ("CSA_DEPTH", "1")),
    ] {
        let log = tempdir.path().join(format!("{case}.log"));
        let output = run_review_check(&fake_bin_dir, &log, context);
        assert!(
            !output.status.success(),
            "{case} CSA context bypassed review validation: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read_to_string(&log).expect("read fake csa invocation log"),
            "review --check-verdict\n",
            "{case} CSA context did not run exactly one receipt validation"
        );
    }
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
    fs::create_dir(root.join(".fixture-git-hooks")).expect("create fixture hook directory");
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
    fs::write(root.join(".gitignore"), "/target\n/.fixture-git-hooks\n")
        .expect("write fixture ignore rules");
    run_git(&root, &["init", "--quiet"]);
    run_git(&root, &["config", "user.email", "gates@example.test"]);
    run_git(&root, &["config", "user.name", "Gate Fixture"]);
    write_executable(
        &root.join(".fixture-git-hooks/pre-push"),
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

fn receipt_files(fixture: &GateFixture) -> io::Result<Vec<PathBuf>> {
    let receipt_dir = fixture.root.join("target/local-gates/receipts");
    let entries = match fs::read_dir(receipt_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut receipts = entries
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<io::Result<Vec<_>>>()?;
    receipts.sort();
    Ok(receipts)
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
    let receipts = receipt_files(&fixture).expect("list receipts");
    assert_eq!(receipts.len(), 1, "receipts={receipts:?}");
    let receipt = fs::read_to_string(&receipts[0]).expect("read receipt");
    assert!(receipt.contains("schema=local-gate-receipt-v1"));
    assert!(receipt.contains("status=PASS"));
    let head = fixture_git_output(&fixture.root, &["rev-parse", "HEAD"]);
    assert!(receipt.contains(&format!(
        "head={}",
        String::from_utf8_lossy(&head.stdout).trim()
    )));
    let tree = fixture_git_output(&fixture.root, &["rev-parse", "HEAD^{tree}"]);
    assert!(receipt.contains(&format!(
        "tree={}",
        String::from_utf8_lossy(&tree.stdout).trim()
    )));
    let receipt_relative = receipts[0]
        .strip_prefix(&fixture.root)
        .expect("receipt is inside fixture")
        .to_str()
        .expect("UTF-8 receipt path");
    let ignored = fixture_git_output(&fixture.root, &["check-ignore", "-q", receipt_relative]);
    assert!(ignored.status.success(), "receipt must be ignored");
}

#[cfg(unix)]
#[test]
fn producer_and_validator_reuse_receipts_through_an_ignored_symlinked_target() {
    let fixture = fixture(successful_aggregate());
    let linked_target = tempfile::tempdir().expect("create linked target directory");
    let target_link = fixture.root.join("target");
    std::os::unix::fs::symlink(linked_target.path(), &target_link)
        .expect("link fixture target directory");

    let produced = run_fixture(&fixture, "produce");
    assert!(
        produced.status.success(),
        "producer rejected ignored symlinked target: stderr={}",
        String::from_utf8_lossy(&produced.stderr)
    );

    let receipt_dir = linked_target.path().join("local-gates/receipts");
    let linked_receipts = fs::read_dir(&receipt_dir)
        .expect("receipt directory was created through linked target")
        .map(|entry| entry.expect("read linked receipt").path())
        .collect::<Vec<_>>();
    assert_eq!(
        linked_receipts.len(),
        1,
        "linked receipts={linked_receipts:?}"
    );
    let linked_receipt = linked_receipts
        .into_iter()
        .next()
        .expect("linked receipt exists");
    let receipt_relative = Path::new("target/local-gates/receipts").join(
        linked_receipt
            .file_name()
            .expect("linked receipt has file name"),
    );
    let receipt_relative = receipt_relative
        .to_str()
        .expect("UTF-8 linked receipt path")
        .to_owned();

    let reused = run_fixture(&fixture, "validate");
    assert!(
        reused.status.success(),
        "validator rejected receipt through ignored symlinked target: stderr={}",
        String::from_utf8_lossy(&reused.stderr)
    );
    assert_eq!(
        fs::read_to_string(&fixture.aggregate_log).expect("read aggregate log"),
        "aggregate\n",
        "receipt validation must reuse the linked receipt without rerunning the aggregate"
    );

    let target_is_ignored = fixture_git_output(&fixture.root, &["check-ignore", "-q", "target"]);
    assert!(
        target_is_ignored.status.success(),
        "target symlink must be ignored"
    );
    for untracked_path in ["target".to_owned(), receipt_relative] {
        let output = run_fixture_git_with_timeout(
            &fixture.root,
            &["ls-files", "--error-unmatch", "--", &untracked_path],
        )
        .expect("run git ls-files for linked receipt");
        assert!(
            !output.status.success(),
            "Git unexpectedly tracks {untracked_path}: stdout={}, stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(unix)]
#[test]
fn producer_rejects_an_unignored_symlinked_target() {
    let fixture = fixture(successful_aggregate());
    fs::write(fixture.root.join(".gitignore"), "/.fixture-git-hooks\n")
        .expect("remove target ignore rule");
    run_git(&fixture.root, &["add", ".gitignore"]);
    run_git(
        &fixture.root,
        &["commit", "--quiet", "-m", "unignore fixture target"],
    );

    let linked_target = tempfile::tempdir().expect("create unignored linked target directory");
    std::os::unix::fs::symlink(linked_target.path(), fixture.root.join("target"))
        .expect("link unignored fixture target directory");

    let output = run_fixture(&fixture, "produce");
    assert!(
        !output.status.success(),
        "producer accepted an unignored symlinked target: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("receipt directory must be ignored: target/local-gates/receipts"),
        "unignored symlink rejection lost its diagnostic: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !fixture.aggregate_log.exists(),
        "aggregate ran after unignored symlink rejection"
    );
    assert!(
        !linked_target.path().join("local-gates/receipts").exists(),
        "unignored symlink minted a receipt"
    );
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
        assert!(
            receipt_files(&fixture).expect("list receipts").is_empty(),
            "failure minted PASS"
        );
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
        receipt_files(&fixture).expect("list receipts").is_empty(),
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
            .expect("list receipts")
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
        .find("push-reviewed:")
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
        "gh pr create --base main",
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
