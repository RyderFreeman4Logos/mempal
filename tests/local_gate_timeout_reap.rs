use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
