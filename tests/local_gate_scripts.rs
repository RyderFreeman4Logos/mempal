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

fn run_bash_script(
    script: &Path,
    args: &[&str],
    envs: &[(&str, &str)],
    timeout: Duration,
) -> Output {
    let mut command = Command::new("bash");
    command
        .arg(script)
        .args(args)
        .current_dir(repo_root())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in envs {
        command.env(key, value);
    }
    let child = command.spawn().expect("spawn bash script");
    wait_with_timeout(child, timeout).expect("wait for bash script")
}

fn sleeper_pids(timeout_secs: &str) -> Vec<String> {
    let ps = Command::new("ps")
        .args(["-eo", "pid=,args="])
        .output()
        .expect("run ps");
    let stdout = String::from_utf8_lossy(&ps.stdout);
    let expected = format!("sleep {timeout_secs}");
    stdout
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim();
            let (pid, args) = trimmed.split_once(' ')?;
            (args.trim() == expected).then(|| pid.to_string())
        })
        .collect()
}

fn kill_pids(pids: &[String]) {
    for pid in pids {
        let _ = Command::new("kill").arg(pid).status();
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

#[test]
fn cargo_test_wrapper_times_out_and_reports_process_context() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.sh");

    let output = run_bash_script(
        &script,
        &["bash", "-c", "sleep 5"],
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
    kill_pids(&sleeper_pids("313"));

    let status = Command::new("timeout")
        .arg("3s")
        .arg("bash")
        .arg(&script)
        .args(["bash", "-c", "true"])
        .current_dir(repo_root())
        .env("MEMPAL_CARGO_TEST_TIMEOUT_SECS", "313")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .expect("run wrapper through timeout");

    assert!(status.success());
    let leaked = sleeper_pids("313");
    kill_pids(&leaked);
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
    write_executable(&bin_dir.join("bash"), "#!/bin/sh\nexec /bin/bash \"$@\"\n");
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
    write_executable(
        &bin_dir.join("mise"),
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "$#" -lt 4 || "$1" != "x" || "$3" != "--" || "$4" != "cargo" ]]; then
    exit 64
fi
shift 4
exec cargo "$@"
"#,
    );
    write_executable(
        &bin_dir.join("cargo"),
        r#"#!/usr/bin/env bash
set -euo pipefail
if [[ "${1:-}" == "test" && ! -e "${FAKE_CARGO_TRIGGERED}" ]]; then
    : >"${FAKE_CARGO_TRIGGERED}"
    (
        trap '' HUP
        printf '%s\n' "${BASHPID}" >"${HOLDER_PID_FILE}"
        : >"${HOLDER_READY_FILE}"
        exec sleep 30
    ) </dev/null >/dev/null 2>&1 &
    while [[ ! -s "${HOLDER_PID_FILE}" || ! -e "${HOLDER_READY_FILE}" ]]; do
        sleep 0.01
    done
    kill -TERM "$$"
fi
exit 0
"#,
    );

    let target = fixture.path().join("target");
    let triggered_file = fixture.path().join("cargo-triggered");
    let holder_pid_file = fixture.path().join("holder.pid");
    let holder_ready_file = fixture.path().join("holder-ready");
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

    let holder_pid = fs::read_to_string(&holder_pid_file).expect("read holder pid");
    kill_pids(&[holder_pid.trim().to_string()]);
    assert!(
        second.status.success(),
        "detached child retained REST lock: stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
}

#[test]
fn rest_gate_reports_when_another_rest_gate_holds_the_lock() {
    let fixture = tempfile::tempdir().expect("create lock fixture");
    let target = fixture.path().join("target");
    fs::create_dir(&target).expect("create isolated target");
    let target = fs::canonicalize(target).expect("canonical isolated target");
    let mut lock_file = target.as_os_str().to_os_string();
    lock_file.push(".lock");
    let lock_file = PathBuf::from(lock_file);
    let unsafe_override = fixture.path().join("different.lock");
    let ready_file = fixture.path().join("lock-ready");
    let lock_holder = Command::new("flock")
        .arg(&lock_file)
        .args(["bash", "-c", "touch \"$LOCK_READY\"; sleep 1"])
        .env("LOCK_READY", &ready_file)
        .spawn()
        .expect("spawn lock holder");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(ready_file.exists(), "lock holder did not become ready");

    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let output = run_bash_script(
        &script,
        &[],
        &[
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
        ],
        Duration::from_secs(10),
    );
    wait_with_timeout(lock_holder, Duration::from_secs(3)).expect("reap lock holder");

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

#[test]
fn rest_gate_fails_with_a_bounded_lock_timeout() {
    let fixture = tempfile::tempdir().expect("create timeout fixture");
    let target = fixture.path().join("target");
    fs::create_dir(&target).expect("create isolated target");
    let target = fs::canonicalize(target).expect("canonical isolated target");
    let mut lock_file = target.as_os_str().to_os_string();
    lock_file.push(".lock");
    let lock_file = PathBuf::from(lock_file);
    let ready_file = fixture.path().join("lock-ready");
    let lock_holder = Command::new("flock")
        .arg(&lock_file)
        .args(["bash", "-c", "touch \"$LOCK_READY\"; sleep 3"])
        .env("LOCK_READY", &ready_file)
        .spawn()
        .expect("spawn lock holder");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready_file.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(25));
    }
    assert!(ready_file.exists(), "lock holder did not become ready");

    let script = repo_root().join("scripts/gates/rest-tests.sh");
    let output = run_bash_script(
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
    wait_with_timeout(lock_holder, Duration::from_secs(5)).expect("reap lock holder");

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
    assert!(
        justfile
            .contains("CARGO_BUILD_JOBS=1 {{cargo}} test --locked --all-features -j 1 --no-run")
    );
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
