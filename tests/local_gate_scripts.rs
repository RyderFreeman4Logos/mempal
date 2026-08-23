use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
#[path = "support/local_gate_child.rs"]
mod local_gate_child;

#[cfg(unix)]
#[path = "support/local_gate_pid_safety.rs"]
mod local_gate_pid_safety;

#[cfg(unix)]
use local_gate_child::{
    GateChild, OwnedGateChild, capture_recorded_process, reap_owned_child, spawn_in_own_session,
};

#[cfg(unix)]
#[test]
fn descendant_monitor_delayed_start_is_joined_after_release() {
    local_gate_child::descendant_monitor_delayed_start_is_joined_after_release();
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn run_bash_script(
    script: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    timeout: Duration,
) -> Output {
    let mut gate = GateChild::new(spawn_bash_script(script, args, envs))
        .expect("capture isolated bash script identity");
    gate.wait_with_timeout(timeout)
        .expect("wait for bash script")
}

fn spawn_bash_script(script: &Path, args: &[&str], envs: &[(&str, &str)]) -> OwnedGateChild {
    let mut command = Command::new("/bin/bash");
    command
        .arg(script)
        .args(args)
        .current_dir(repo_root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    spawn_in_own_session(&mut command).expect("spawn isolated bash script")
}

fn wait_for_file(path: &Path, timeout: Duration, description: &str) {
    let deadline = Instant::now() + timeout;
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "{description} did not become ready");
}

#[cfg(unix)]
struct LockHolder {
    child: Option<OwnedGateChild>,
    release_file: PathBuf,
}

#[cfg(unix)]
impl LockHolder {
    fn spawn(lock_file: &Path, fixture_root: &Path) -> Self {
        let ready_file = fixture_root.join("lock-holder-ready");
        let release_file = fixture_root.join("lock-holder-release");
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    exec {lock_fd}>"${LOCK_FILE:?}"
                    flock "${lock_fd}"
                    : >"${LOCK_READY_FILE:?}"
                    while [[ ! -e "${LOCK_RELEASE_FILE:?}" ]]; do
                        sleep 0.01
                    done
                "#,
            ])
            .env("LOCK_FILE", lock_file)
            .env("LOCK_READY_FILE", &ready_file)
            .env("LOCK_RELEASE_FILE", &release_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let child = spawn_in_own_session(&mut command).expect("spawn coordinated lock holder");
        let holder = Self {
            child: Some(child),
            release_file,
        };
        wait_for_file(&ready_file, Duration::from_secs(2), "lock holder");

        holder
    }

    fn release_and_reap(&mut self) {
        fs::write(&self.release_file, "release\n").expect("release lock holder");
        let child = self.child.take().expect("lock holder already reaped");
        reap_owned_child(child).expect("reap released lock holder");
    }
}

#[cfg(unix)]
impl Drop for LockHolder {
    fn drop(&mut self) {
        let _ = fs::write(&self.release_file, "release\n");
        if let Some(child) = self.child.take() {
            let _ = reap_owned_child(child);
        }
    }
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

#[test]
fn cargo_test_wrapper_times_out_and_reports_process_context() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.sh");

    let output = run_bash_script(
        &script,
        &["/bin/bash", "-c", "sleep 5"],
        &[
            ("MEMPAL_CARGO_TEST_TIMEOUT_SECS", "1"),
            ("MEMPAL_CARGO_TEST_KILL_GRACE_SECS", "1"),
        ],
        Duration::from_secs(6),
    );

    assert_eq!(output.status.code(), Some(124));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("cargo test command timed out"),
        "stderr={stderr}"
    );
    assert!(stderr.contains("active command:"), "stderr={stderr}");
    assert!(stderr.contains("process tree:"), "stderr={stderr}");
}

#[test]
fn cargo_test_wrapper_success_does_not_leave_timeout_sleeper() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.sh");
    let timeout_secs = format!("9{}", std::process::id());
    local_gate_pid_safety::terminate_recorded_processes(&local_gate_pid_safety::sleeper_processes(
        &timeout_secs,
    ));

    let status = Command::new("timeout")
        .arg("3s")
        .arg("/bin/bash")
        .arg(&script)
        .args(["/bin/bash", "-c", "true"])
        .current_dir(repo_root())
        .env("MEMPAL_CARGO_TEST_TIMEOUT_SECS", &timeout_secs)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run wrapper through timeout");

    assert!(status.success());
    let leaked = local_gate_pid_safety::sleeper_processes(&timeout_secs);
    local_gate_pid_safety::terminate_recorded_processes(&leaked);
    assert!(
        leaked.is_empty(),
        "timeout sleeper leaked after successful wrapper run: {leaked:?}"
    );
}

#[test]
fn rest_gate_dry_run_wraps_cargo_test_phases() {
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
    assert!(
        stdout.contains("scripts/gates/cargo-test-with-timeout.sh"),
        "stdout={stdout}"
    );
    assert!(stdout.contains("--features rest --lib --bins"));
    assert!(stdout.contains("--features rest --doc"));
}

#[test]
fn rest_gate_isolates_cleanup_from_the_shared_cargo_target() {
    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let fixture = tempfile::tempdir().expect("create isolated fixture");
    let isolated_target = fixture.path().join("target");
    let isolated_target_text = isolated_target.to_str().expect("UTF-8 target path");
    let output = run_bash_script(
        &script,
        &[],
        &[
            ("REST_GATE_DRY_RUN", "1"),
            ("REST_GATE_TARGET_DIR", isolated_target_text),
            ("REST_TEST_TARGETS_PER_BATCH", "999"),
            ("CARGO_BUILD_JOBS", "2"),
        ],
        Duration::from_secs(10),
    );

    assert!(output.status.success());
    let isolated_target = fs::canonicalize(&isolated_target).expect("canonical isolated target");
    let isolated_target_text = isolated_target.to_str().expect("UTF-8 target path");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(&format!("rest cargo target dir: {isolated_target_text}")),
        "stdout={stdout}"
    );
    assert!(
        stdout.contains("rest cargo build jobs: 2"),
        "stdout={stdout}"
    );
}

#[test]
fn rest_gate_rejects_the_shared_cargo_target() {
    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let shared_target = repo_root().join("target");
    let shared_target_text = shared_target.to_str().expect("UTF-8 target path");
    let output = run_bash_script(
        &script,
        &[],
        &[
            ("REST_GATE_DRY_RUN", "1"),
            ("REST_GATE_TARGET_DIR", shared_target_text),
        ],
        Duration::from_secs(10),
    );

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("must not use the shared Cargo target"),
        "stderr={stderr}"
    );
}

#[cfg(unix)]
#[test]
fn rest_gate_reports_missing_flock_dependency() {
    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let fixture = tempfile::tempdir().expect("create missing-flock fixture");
    let bin_dir = fixture.path().join("bin");
    fs::create_dir(&bin_dir).expect("create fixture bin directory");
    let fixture_path = bin_dir.to_str().expect("UTF-8 fixture bin path");

    let output = run_bash_script(
        &script,
        &[],
        &[("PATH", fixture_path)],
        Duration::from_secs(3),
    );

    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("REST gate requires 'flock' in PATH"),
        "stderr={stderr}"
    );
    assert!(!stderr.contains("lock timed out"), "stderr={stderr}");
}

#[cfg(unix)]
#[test]
fn rest_gate_children_cannot_retain_the_parent_lock() {
    let fixture = tempfile::tempdir().expect("create inherited-lock fixture");
    let bin_dir = fixture.path().join("bin");
    fs::create_dir(&bin_dir).expect("create fixture bin directory");
    symlink_path_command_to_stable_proxy(&bin_dir, "mise");
    symlink_path_command_to_stable_proxy(&bin_dir, "cargo");

    let target = fixture.path().join("target");
    let triggered_file = fixture.path().join("cargo-triggered");
    let holder_pid_file = fixture.path().join("holder.pid");
    let holder_lock_state_file = fixture.path().join("holder-lock-state");
    let holder_ready_file = fixture.path().join("holder-ready");
    let mut lock_file = target.as_os_str().to_os_string();
    lock_file.push(".lock");
    let lock_file = PathBuf::from(lock_file);
    let path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").expect("PATH is set")
    );
    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let first = run_bash_script(
        &script,
        &[],
        &[
            ("PATH", &path),
            (
                "REST_GATE_TARGET_DIR",
                target.to_str().expect("UTF-8 target path"),
            ),
            (
                "FAKE_CARGO_TRIGGERED",
                triggered_file.to_str().expect("UTF-8 trigger path"),
            ),
            (
                "HOLDER_PID_FILE",
                holder_pid_file.to_str().expect("UTF-8 holder pid path"),
            ),
            (
                "HOLDER_READY_FILE",
                holder_ready_file.to_str().expect("UTF-8 holder ready path"),
            ),
            (
                "HOLDER_LOCK_FILE",
                lock_file.to_str().expect("UTF-8 lock file path"),
            ),
            (
                "HOLDER_LOCK_STATE_FILE",
                holder_lock_state_file
                    .to_str()
                    .expect("UTF-8 holder lock state path"),
            ),
        ],
        Duration::from_secs(10),
    );
    assert!(
        !first.status.success(),
        "simulated Cargo parent must terminate"
    );
    assert!(
        holder_ready_file.exists(),
        "descriptor holder did not start"
    );
    let holder_identity = local_gate_pid_safety::recorded_process_identity(
        &fs::read_to_string(&holder_pid_file).expect("read holder process identity"),
    );
    assert!(
        holder_identity.pid > 0 && holder_identity.start_time_ticks > 0,
        "holder fixture must record a complete PID identity"
    );
    assert_eq!(
        fs::read_to_string(&holder_lock_state_file).expect("read holder lock state"),
        "closed\n",
        "the simulated Cargo descendant must not inherit the REST lock descriptor"
    );
    let holder_is_running = match capture_recorded_process(holder_identity)
        .expect("re-verify recorded holder identity after gate cleanup")
    {
        Some(holder) => {
            let is_running = holder
                .is_running()
                .expect("inspect recorded holder running state");
            if is_running {
                holder
                    .send_signal(libc::SIGKILL)
                    .expect("pidfd-safe fallback cleanup for a running recorded holder");
            }
            is_running
        }
        None => false,
    };
    assert!(
        !holder_is_running,
        "the recorded descriptor holder must terminate with the gate process tree"
    );

    let second = run_bash_script(
        &script,
        &[],
        &[
            ("REST_GATE_DRY_RUN", "1"),
            ("REST_GATE_LOCK_TIMEOUT_SECS", "1"),
            (
                "REST_GATE_TARGET_DIR",
                target.to_str().expect("UTF-8 target path"),
            ),
            ("REST_TEST_TARGETS_PER_BATCH", "999"),
        ],
        Duration::from_secs(5),
    );

    assert!(
        second.status.success(),
        "detached child retained REST lock: stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
}

#[cfg(unix)]
#[test]
fn rest_gate_reports_when_another_rest_gate_holds_the_lock() {
    let fixture = tempfile::tempdir().expect("create lock fixture");
    let bin_dir = fixture.path().join("bin");
    fs::create_dir(&bin_dir).expect("create fixture bin directory");
    symlink_path_command_to_stable_proxy(&bin_dir, "fuser");
    let target = fixture.path().join("target");
    fs::create_dir(&target).expect("create isolated target");
    let target = fs::canonicalize(target).expect("canonical isolated target");
    let mut lock_file = target.as_os_str().to_os_string();
    lock_file.push(".lock");
    let lock_file = PathBuf::from(lock_file);
    let unsafe_override = fixture.path().join("different.lock");
    let fuser_ready_file = fixture.path().join("fuser-ready");
    let fuser_release_file = fixture.path().join("fuser-release");
    let fuser_released_file = fixture.path().join("fuser-released");
    let mut lock_holder = LockHolder::spawn(&lock_file, fixture.path());
    let inherited_path = std::env::var_os("PATH").expect("PATH is set");
    let path = std::env::join_paths(
        std::iter::once(bin_dir).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("construct fixture PATH");

    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let mut gate = GateChild::new(spawn_bash_script(
        &script,
        &[],
        &[
            ("PATH", path.to_str().expect("UTF-8 fixture PATH")),
            ("REST_GATE_DRY_RUN", "1"),
            ("REST_GATE_LOCK_TIMEOUT_SECS", "3"),
            (
                "REST_GATE_LOCK_FILE",
                unsafe_override.to_str().expect("UTF-8 lock path"),
            ),
            (
                "REST_GATE_TARGET_DIR",
                target.to_str().expect("UTF-8 target path"),
            ),
            ("REST_TEST_TARGETS_PER_BATCH", "999"),
            (
                "REST_GATE_FUSER_READY_FILE",
                fuser_ready_file.to_str().expect("UTF-8 fuser ready path"),
            ),
            (
                "REST_GATE_FUSER_RELEASE_FILE",
                fuser_release_file
                    .to_str()
                    .expect("UTF-8 fuser release path"),
            ),
            (
                "REST_GATE_FUSER_RELEASED_FILE",
                fuser_released_file
                    .to_str()
                    .expect("UTF-8 fuser released path"),
            ),
            (
                "REST_GATE_LOCK_HOLDER_RELEASE_FILE",
                lock_holder
                    .release_file
                    .to_str()
                    .expect("UTF-8 lock holder release path"),
            ),
        ],
    ))
    .expect("capture isolated REST gate identity");
    wait_for_file(
        &fuser_ready_file,
        Duration::from_secs(2),
        "REST lock diagnostic",
    );
    fs::write(&fuser_release_file, "release\n").expect("release REST lock diagnostic");
    let output = gate
        .wait_with_timeout(Duration::from_secs(5))
        .expect("reap REST gate");
    wait_for_file(
        &fuser_released_file,
        Duration::from_secs(2),
        "REST lock diagnostic release",
    );
    lock_holder.release_and_reap();

    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rest gate waiting for lock:"),
        "stderr={stderr}"
    );
    assert!(
        stderr.contains("rest gate acquired lock:"),
        "stderr={stderr}"
    );
    assert!(
        stderr.contains(lock_file.to_str().expect("UTF-8 lock path")),
        "stderr={stderr}"
    );
    assert!(
        !stderr.contains(unsafe_override.to_str().expect("UTF-8 lock path")),
        "stderr={stderr}"
    );
}

#[cfg(unix)]
#[test]
fn rest_gate_does_not_lose_subsecond_budget_at_a_seconds_tick() {
    let fixture = tempfile::tempdir().expect("create subsecond lock fixture");
    let bin_dir = fixture.path().join("bin");
    fs::create_dir(&bin_dir).expect("create fixture bin directory");
    symlink_path_command_to_stable_proxy(&bin_dir, "fuser");
    let target = fixture.path().join("target");
    fs::create_dir(&target).expect("create isolated target");
    let target = fs::canonicalize(target).expect("canonical isolated target");
    let mut lock_file = target.as_os_str().to_os_string();
    lock_file.push(".lock");
    let lock_file = PathBuf::from(lock_file);
    let bash_env_file = fixture.path().join("advance-seconds.sh");
    // This test-only signal handler simulates the one integer `SECONDS` tick
    // that the former budget math could over-count during a 0.30s diagnostic.
    fs::write(&bash_env_file, "trap 'SECONDS=$((SECONDS + 1))' USR1\n")
        .expect("write SECONDS debug environment");
    let fuser_released_file = fixture.path().join("fuser-released");
    let mut lock_holder = LockHolder::spawn(&lock_file, fixture.path());
    let inherited_path = std::env::var_os("PATH").expect("PATH is set");
    let path = std::env::join_paths(
        std::iter::once(bin_dir).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("construct fixture PATH");

    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let output = run_bash_script(
        &script,
        &[],
        &[
            ("PATH", path.to_str().expect("UTF-8 fixture PATH")),
            ("REST_GATE_DRY_RUN", "1"),
            ("REST_GATE_LOCK_TIMEOUT_SECS", "1"),
            ("REST_TEST_TARGETS_PER_BATCH", "999"),
            (
                "BASH_ENV",
                bash_env_file.to_str().expect("UTF-8 Bash environment path"),
            ),
            (
                "REST_GATE_TARGET_DIR",
                target.to_str().expect("UTF-8 target path"),
            ),
            ("REST_GATE_FUSER_DELAY_SECS", "0.30"),
            ("REST_GATE_FUSER_ADVANCE_PARENT_SECONDS", "1"),
            (
                "REST_GATE_FUSER_RELEASED_FILE",
                fuser_released_file
                    .to_str()
                    .expect("UTF-8 fuser release path"),
            ),
            (
                "REST_GATE_LOCK_HOLDER_RELEASE_FILE",
                lock_holder
                    .release_file
                    .to_str()
                    .expect("UTF-8 lock holder release path"),
            ),
        ],
        Duration::from_secs(5),
    );
    lock_holder.release_and_reap();

    assert!(
        fuser_released_file.exists(),
        "the diagnostic must release the holder inside the real lock budget"
    );
    assert!(
        output.status.success(),
        "a subsecond diagnostic that crosses a SECONDS tick must not exhaust a 1s budget: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn rest_gate_rejects_lock_timeout_over_one_day() {
    let fixture = tempfile::tempdir().expect("create invalid timeout fixture");
    let target = fixture.path().join("target");
    let script = repo_root().join("scripts/gates/rest-tests.sh");

    for timeout_secs in ["86401", "18446744073709551615"] {
        let output = run_bash_script(
            &script,
            &[],
            &[
                ("REST_GATE_DRY_RUN", "1"),
                ("REST_GATE_LOCK_TIMEOUT_SECS", timeout_secs),
                (
                    "REST_GATE_TARGET_DIR",
                    target.to_str().expect("UTF-8 target path"),
                ),
                ("REST_TEST_TARGETS_PER_BATCH", "999"),
            ],
            Duration::from_secs(5),
        );

        assert_eq!(output.status.code(), Some(2), "timeout_secs={timeout_secs}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("REST_GATE_LOCK_TIMEOUT_SECS must be a positive integer <= 86400"),
            "timeout_secs={timeout_secs}, stderr={stderr}"
        );
    }
}

#[cfg(unix)]
#[test]
fn rest_gate_fails_with_a_bounded_lock_timeout() {
    let fixture = tempfile::tempdir().expect("create timeout fixture");
    let bin_dir = fixture.path().join("bin");
    fs::create_dir(&bin_dir).expect("create fixture bin directory");
    symlink_path_command_to_stable_proxy(&bin_dir, "fuser");
    let target = fixture.path().join("target");
    fs::create_dir(&target).expect("create isolated target");
    let target = fs::canonicalize(target).expect("canonical isolated target");
    let mut lock_file = target.as_os_str().to_os_string();
    lock_file.push(".lock");
    let lock_file = PathBuf::from(lock_file);
    let fuser_ready_file = fixture.path().join("fuser-ready");
    let fuser_release_file = fixture.path().join("fuser-release");
    let fuser_released_file = fixture.path().join("fuser-released");
    let mut lock_holder = LockHolder::spawn(&lock_file, fixture.path());
    let inherited_path = std::env::var_os("PATH").expect("PATH is set");
    let path = std::env::join_paths(
        std::iter::once(bin_dir).chain(std::env::split_paths(&inherited_path)),
    )
    .expect("construct fixture PATH");

    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let mut gate = GateChild::new(spawn_bash_script(
        &script,
        &[],
        &[
            ("PATH", path.to_str().expect("UTF-8 fixture PATH")),
            ("REST_GATE_DRY_RUN", "1"),
            ("REST_GATE_LOCK_TIMEOUT_SECS", "1"),
            (
                "REST_GATE_TARGET_DIR",
                target.to_str().expect("UTF-8 target path"),
            ),
            ("REST_TEST_TARGETS_PER_BATCH", "999"),
            (
                "REST_GATE_FUSER_READY_FILE",
                fuser_ready_file.to_str().expect("UTF-8 fuser ready path"),
            ),
            (
                "REST_GATE_FUSER_RELEASE_FILE",
                fuser_release_file
                    .to_str()
                    .expect("UTF-8 fuser release path"),
            ),
            (
                "REST_GATE_FUSER_RELEASED_FILE",
                fuser_released_file
                    .to_str()
                    .expect("UTF-8 fuser released path"),
            ),
            (
                "REST_GATE_LOCK_HOLDER_RELEASE_FILE",
                lock_holder
                    .release_file
                    .to_str()
                    .expect("UTF-8 lock holder release path"),
            ),
            ("REST_GATE_FUSER_STALL_AFTER_RELEASE", "1"),
        ],
    ))
    .expect("capture isolated REST gate identity");
    wait_for_file(
        &fuser_ready_file,
        Duration::from_secs(2),
        "blocked REST lock diagnostic",
    );

    let budget_probe = Command::new("flock")
        .args(["-w", "0.5"])
        .arg(&lock_file)
        .arg("true")
        .status()
        .expect("run REST lock deadline probe");
    assert_eq!(
        budget_probe.code(),
        Some(1),
        "the holder must remain locked until the diagnostic releases it"
    );
    fs::write(&fuser_release_file, "release\n").expect("release blocked REST lock diagnostic");
    let output = gate
        .wait_with_timeout(Duration::from_secs(5))
        .expect("reap REST gate");
    wait_for_file(
        &fuser_released_file,
        Duration::from_secs(2),
        "blocked REST lock diagnostic release",
    );
    lock_holder.release_and_reap();

    assert_eq!(output.status.code(), Some(75));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("rest gate lock timed out after 1s:"),
        "stderr={stderr}"
    );
}

#[test]
fn just_test_recipe_uses_cargo_test_timeout_wrapper() {
    let justfile = fs::read_to_string(repo_root().join("justfile")).expect("read justfile");

    assert!(
        justfile.contains("bash scripts/gates/cargo-test-with-timeout.sh {{cargo}} test"),
        "just test must run cargo test through the bounded local-gate wrapper"
    );
}

#[test]
fn just_onnx_recipe_uses_checksum_pinned_shared_runtime() {
    let justfile = fs::read_to_string(repo_root().join("justfile")).expect("read justfile");
    let cargo_toml = fs::read_to_string(repo_root().join("Cargo.toml")).expect("read Cargo.toml");
    let script = fs::read_to_string(repo_root().join("scripts/gates/onnx-tests.sh"))
        .expect("read ONNX test gate");

    assert!(justfile.contains("bash scripts/gates/onnx-tests.sh"));
    assert!(justfile.contains("{{cargo}} test --locked --all-features --no-run"));
    assert!(justfile.contains("just test-onnx-link"));
    assert!(cargo_toml.contains("default-features = false"));
    assert!(cargo_toml.contains("\"load-dynamic\""));
    assert!(script.contains("archive_sha256="));
    assert!(script.contains("runtime_sha256="));
    assert!(script.contains("ORT_PREFER_DYNAMIC_LINK=1"));
    assert!(script.contains("ORT_DYLIB_PATH="));
}

#[cfg(feature = "onnx")]
#[test]
fn onnx_dynamic_runtime_api_is_loadable() {
    let _api = ort::api();
}

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
#[test]
fn onnx_gate_rejects_incomplete_cache_without_deleting_it() {
    let cache = tempfile::tempdir().expect("create ONNX test cache");
    let runtime_dir = cache.path().join("onnxruntime-linux-x64-1.24.2");
    fs::create_dir(&runtime_dir).expect("create incomplete runtime cache");
    let marker = runtime_dir.join("keep-me");
    fs::write(&marker, "existing cache content").expect("write cache marker");

    let script = repo_root().join("scripts/gates/onnx-tests.sh");
    let cache_path = cache.path().to_str().expect("UTF-8 temp path");
    let output = run_bash_script(
        &script,
        &["--no-run"],
        &[("ORT_TEST_CACHE_DIR", cache_path)],
        Duration::from_secs(10),
    );

    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ONNX Runtime cache is incomplete"));
    assert!(
        marker.exists(),
        "the gate must not delete an existing cache"
    );
}
