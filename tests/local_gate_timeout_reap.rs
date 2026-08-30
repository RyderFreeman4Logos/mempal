use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
#[allow(dead_code)]
#[path = "support/local_gate_child.rs"]
mod local_gate_child;

#[cfg(unix)]
#[allow(dead_code)]
#[path = "support/local_gate_pid_safety.rs"]
mod local_gate_pid_safety;

#[cfg(unix)]
use local_gate_child::{capture_recorded_process, spawn_in_own_session};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn wait_for_file(path: &Path, timeout: Duration, description: &str) {
    let deadline = Instant::now() + timeout;
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(10));
    }
    assert!(path.exists(), "{description} did not become ready");
}

#[cfg(unix)]
fn spawn_piped_wrapper(
    script: &Path,
    command: &str,
    timeout_secs: &str,
    identity_path: &Path,
) -> (
    std::process::Child,
    Receiver<std::io::Result<Vec<u8>>>,
    Receiver<std::io::Result<Vec<u8>>>,
) {
    let mut wrapper = Command::new("/bin/bash");
    wrapper
        .arg(script)
        .args(["/bin/bash", "-c", command])
        .current_dir(repo_root())
        .env("IDENTITY_FILE", identity_path)
        .env("MEMPAL_CARGO_TEST_TIMEOUT_SECS", timeout_secs)
        .env("MEMPAL_CARGO_TEST_KILL_GRACE_SECS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut wrapper = wrapper.spawn().expect("spawn timeout wrapper");
    let stdout = wrapper.stdout.take().expect("capture wrapper stdout");
    let stderr = wrapper.stderr.take().expect("capture wrapper stderr");
    let stdout_done = spawn_pipe_reader(stdout);
    let stderr_done = spawn_pipe_reader(stderr);
    (wrapper, stdout_done, stderr_done)
}

#[cfg(unix)]
fn spawn_pipe_reader<R: Read + Send + 'static>(mut pipe: R) -> Receiver<std::io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = pipe.read_to_end(&mut output).map(|_| output);
        let _ = sender.send(result);
    });
    receiver
}

#[cfg(unix)]
fn wait_for_wrapper(
    wrapper: &mut std::process::Child,
    timeout: Duration,
    description: &str,
) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = wrapper.try_wait().expect("poll timeout wrapper") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "{description} did not exit in time"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn recorded_process_is_running(record: &str) -> bool {
    let identity = local_gate_pid_safety::recorded_process_identity(record);
    capture_recorded_process(identity)
        .expect("re-verify fixture process identity")
        .map(|process| {
            process
                .is_running()
                .expect("inspect fixture process liveness")
        })
        .unwrap_or(false)
}

#[cfg(unix)]
fn kill_recorded_process(record: &str) {
    let identity = local_gate_pid_safety::recorded_process_identity(record);
    if let Some(process) =
        capture_recorded_process(identity).expect("re-verify fixture identity before cleanup")
    {
        if process
            .is_running()
            .expect("inspect fixture before cleanup")
        {
            process
                .send_signal(libc::SIGKILL)
                .expect("kill surviving fixture process");
        }
    }
}

#[cfg(unix)]
fn receive_pipe(
    receiver: &Receiver<std::io::Result<Vec<u8>>>,
    already_eof: Option<std::io::Result<Vec<u8>>>,
) -> Vec<u8> {
    already_eof
        .or_else(|| receiver.recv_timeout(Duration::from_secs(2)).ok())
        .expect("wrapper output did not reach EOF")
        .expect("read wrapper output")
}

#[cfg(unix)]
#[test]
fn cargo_test_wrapper_timeout_reaps_a_term_ignoring_descendant() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.sh");
    let fixture = tempfile::tempdir().expect("create timeout-tree fixture");
    let identity_file = fixture.path().join("descendant-identity");
    let identity_path = identity_file.to_str().expect("UTF-8 identity path");
    let mut command = Command::new("/bin/bash");
    command
        .arg(&script)
        .args(["/bin/bash", "-c"])
        .arg(
            r#"
                trap 'exit 0' TERM
                /bin/bash -c '
                    trap "" TERM
                    pid="$BASHPID"
                    start_time="$(awk "{print \$22}" "/proc/${pid}/stat")"
                    printf "%s %s\\n" "$pid" "$start_time" >"${IDENTITY_FILE:?}"
                    while :; do printf x; done
                ' &
                wait
            "#,
        )
        .env("IDENTITY_FILE", identity_path)
        .current_dir(repo_root())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.env("MEMPAL_CARGO_TEST_TIMEOUT_SECS", "1");
    command.env("MEMPAL_CARGO_TEST_KILL_GRACE_SECS", "1");
    let mut wrapper = command.spawn().expect("spawn timeout wrapper");
    wait_for_file(
        &identity_file,
        Duration::from_secs(2),
        "term-ignoring descendant identity",
    );
    let identity_record = fs::read_to_string(&identity_file).expect("read descendant identity");

    let deadline = Instant::now() + Duration::from_secs(4);
    let status = loop {
        if let Some(status) = wrapper.try_wait().expect("poll timeout wrapper") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "timeout wrapper did not return within the bounded test deadline"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(124));

    let identity = local_gate_pid_safety::recorded_process_identity(&identity_record);
    let descendant = capture_recorded_process(identity)
        .expect("re-verify descendant identity after wrapper exit");
    let descendant_running = match descendant {
        Some(descendant) => {
            let running = descendant
                .is_running()
                .expect("inspect descendant liveness");
            if running {
                descendant
                    .send_signal(libc::SIGKILL)
                    .expect("clean up surviving descendant");
            }
            running
        }
        None => false,
    };
    wrapper.wait().expect("reap timeout wrapper");
    assert!(
        !descendant_running,
        "timeout wrapper returned while its owned descendant was still alive"
    );
}

#[cfg(unix)]
#[test]
fn cargo_test_wrapper_timeout_reaps_nested_setsid_descendant_and_drains_pipes() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.sh");
    let fixture = tempfile::tempdir().expect("create nested-session fixture");
    let identity_file = fixture.path().join("descendant-identity");
    let command = r#"
        setsid --wait /bin/bash -c '
            trap "" TERM
            pid="$BASHPID"
            start_time="$(awk "{print \$22}" "/proc/${pid}/stat")"
            printf "%s %s\\n" "$pid" "$start_time" >"${IDENTITY_FILE:?}"
            printf "nested stdout\\n"
            printf "nested stderr\\n" >&2
            while :; do :; done
        ' &
        wait
    "#;
    let (mut wrapper, stdout_done, stderr_done) =
        spawn_piped_wrapper(&script, command, "1", &identity_file);
    wait_for_file(
        &identity_file,
        Duration::from_secs(2),
        "nested-session descendant identity",
    );
    let identity_record = fs::read_to_string(&identity_file).expect("read nested identity");

    let status = wait_for_wrapper(
        &mut wrapper,
        Duration::from_secs(4),
        "timeout wrapper with nested session",
    );
    let running_before_cleanup = recorded_process_is_running(&identity_record);
    let stdout_before_cleanup = stdout_done.recv_timeout(Duration::from_millis(300)).ok();
    let stderr_before_cleanup = stderr_done.recv_timeout(Duration::from_millis(300)).ok();
    let pipes_drained_before_cleanup =
        stdout_before_cleanup.is_some() && stderr_before_cleanup.is_some();
    if running_before_cleanup {
        kill_recorded_process(&identity_record);
    }
    let stdout = receive_pipe(&stdout_done, stdout_before_cleanup);
    let stderr = receive_pipe(&stderr_done, stderr_before_cleanup);

    assert_eq!(status.code(), Some(124));
    assert!(
        !running_before_cleanup,
        "timeout wrapper returned while an escaped owned descendant was alive"
    );
    assert!(
        pipes_drained_before_cleanup,
        "timeout wrapper must drain inherited stdout/stderr before returning"
    );
    assert!(String::from_utf8_lossy(&stdout).contains("nested stdout"));
    assert!(String::from_utf8_lossy(&stderr).contains("nested stderr"));
}

#[cfg(unix)]
#[test]
fn cargo_test_wrapper_reaps_descendant_after_leader_exits_and_drains_pipes() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.sh");
    let fixture = tempfile::tempdir().expect("create early-exit fixture");
    let identity_file = fixture.path().join("descendant-identity");
    let command = r#"
        setsid /bin/bash -c '
            pid="$BASHPID"
            start_time="$(awk "{print \$22}" "/proc/${pid}/stat")"
            printf "%s %s\\n" "$pid" "$start_time" >"${IDENTITY_FILE:?}"
            printf "early stdout\\n"
            printf "early stderr\\n" >&2
            while :; do :; done
        ' &
        while [[ ! -s "${IDENTITY_FILE:?}" ]]; do /bin/sleep 0.01; done
        exit 0
    "#;
    let (mut wrapper, stdout_done, stderr_done) =
        spawn_piped_wrapper(&script, command, "10", &identity_file);
    wait_for_file(
        &identity_file,
        Duration::from_secs(2),
        "early-exit descendant identity",
    );
    let identity_record = fs::read_to_string(&identity_file).expect("read early-exit identity");

    let status = wait_for_wrapper(
        &mut wrapper,
        Duration::from_secs(4),
        "wrapper with an early-exiting leader",
    );
    let running_before_cleanup = recorded_process_is_running(&identity_record);
    let stdout_before_cleanup = stdout_done.recv_timeout(Duration::from_millis(300)).ok();
    let stderr_before_cleanup = stderr_done.recv_timeout(Duration::from_millis(300)).ok();
    let pipes_drained_before_cleanup =
        stdout_before_cleanup.is_some() && stderr_before_cleanup.is_some();
    if running_before_cleanup {
        kill_recorded_process(&identity_record);
    }
    let stdout = receive_pipe(&stdout_done, stdout_before_cleanup);
    let stderr = receive_pipe(&stderr_done, stderr_before_cleanup);

    assert_eq!(status.code(), Some(0));
    assert!(
        !running_before_cleanup,
        "wrapper returned after its leader but left an owned descendant alive"
    );
    assert!(
        pipes_drained_before_cleanup,
        "wrapper must drain inherited stdout/stderr before returning"
    );
    assert!(String::from_utf8_lossy(&stdout).contains("early stdout"));
    assert!(String::from_utf8_lossy(&stderr).contains("early stderr"));
}
