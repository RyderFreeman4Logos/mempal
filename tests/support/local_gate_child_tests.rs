#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

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

    fn spawn_pipe_holding_child(ready_file: &Path, pid_file: &Path) -> Child {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    : >"${READY_FILE:?}"
                    printf '%s\n' "${BASHPID}" >"${PID_FILE:?}"
                    ( trap '' TERM; exec /bin/sleep 60 ) &
                    while true; do sleep 0.01; done
                "#,
            ])
            .env("READY_FILE", ready_file)
            .env("PID_FILE", pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn isolated fixture")
    }

    fn spawn_setsid_pipe_holding_child(
        ready_file: &Path,
        pid_file: &Path,
        exit_after_ready: bool,
    ) -> Child {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    setsid /bin/bash -c '
                        trap "" TERM
                        pid="${BASHPID}"
                        start_time="$(awk "{print \$22}" "/proc/${pid}/stat")"
                        printf "%s %s\n" "${pid}" "${start_time}" >"${PID_FILE:?}"
                        : >"${READY_FILE:?}"
                        while true; do
                            printf "escaped stdout\n"
                            printf "escaped stderr\n" >&2
                            /bin/sleep 60
                        done
                    ' &
                    if [[ "${EXIT_AFTER_READY:?}" == "1" ]]; then
                        while [[ ! -e "${READY_FILE:?}" ]]; do /bin/sleep 0.01; done
                        exit 0
                    fi
                    while true; do /bin/sleep 0.01; done
                "#,
            ])
            .env("READY_FILE", ready_file)
            .env("PID_FILE", pid_file)
            .env("EXIT_AFTER_READY", if exit_after_ready { "1" } else { "0" })
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn isolated fixture")
    }

    fn process_identity_from_pid_file(pid_file: &Path) -> ProcessIdentity {
        let pid_file = fs::read_to_string(pid_file).expect("read escaped process identity");
        let mut fields = pid_file.split_whitespace();
        let pid = fields
            .next()
            .expect("escaped process PID")
            .parse()
            .expect("numeric escaped process PID");
        let start_time_ticks = fields
            .next()
            .expect("escaped process start time")
            .parse()
            .expect("numeric escaped process start time");
        assert!(
            fields.next().is_none(),
            "unexpected escaped process identity"
        );
        ProcessIdentity {
            pid,
            start_time_ticks,
        }
    }

    fn assert_process_identity_gone(identity: ProcessIdentity) {
        assert!(
            !identity
                .is_running()
                .expect("inspect escaped process identity"),
            "escaped process remained alive: {identity:?}"
        );
    }

    #[test]
    fn procfs_exit_errors_are_treated_as_absent_processes() {
        assert!(process_is_gone(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(process_is_gone(&io::Error::from_raw_os_error(libc::ESRCH)));
    }

    #[test]
    fn process_identity_mismatch_is_refused_before_signaling() {
        let expected = ProcessIdentity {
            pid: 4242,
            start_time_ticks: 100,
        };
        let recycled = ProcessSnapshot {
            identity: ProcessIdentity {
                pid: expected.pid,
                start_time_ticks: expected.start_time_ticks + 1,
            },
            parent_pid: 1,
            state: 'S',
        };

        assert!(
            !expected.matches_running_snapshot(Some(&recycled)),
            "a reused PID must not be accepted as the tracked process"
        );
    }

    #[test]
    fn reaped_child_identity_is_not_retracked_when_its_pid_is_reused() {
        let direct_child = ProcessIdentity {
            pid: 4242,
            start_time_ticks: 100,
        };
        let recycled = ProcessSnapshot {
            identity: ProcessIdentity {
                pid: direct_child.pid,
                start_time_ticks: direct_child.start_time_ticks + 1,
            },
            parent_pid: 1,
            state: 'S',
        };

        assert!(
            !direct_child.matches_running_snapshot(Some(&recycled)),
            "the original child identity must never adopt a later process with the same PID"
        );
    }

    #[test]
    fn pidfd_signal_path_is_available_for_tracked_processes() {
        let current_pid = i32::try_from(std::process::id()).expect("test process PID fits i32");
        let process = ProcessHandle::capture(current_pid)
            .expect("capture current process")
            .expect("current process remains visible in procfs");

        assert!(
            process.has_pidfd(),
            "local gate cleanup requires pidfd support"
        );
        process
            .send_signal(0)
            .expect("pidfd signal zero checks the captured process without signaling it");
    }

    #[test]
    fn gate_child_wait_timeout_reaps_descendants_that_hold_pipes() {
        let fixture = tempfile::tempdir().expect("create pipe-holder fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut gate = GateChild::new(spawn_pipe_holding_child(&ready_file, &pid_file))
            .expect("capture pipe-holder child identity");
        wait_for_file(&ready_file, Duration::from_secs(2), "pipe-holder fixture");

        let started = Instant::now();
        let error = gate
            .wait_with_timeout(Duration::from_millis(50))
            .expect_err("a gate timeout must return an error");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GateChild timeout cleanup must not hang on inherited pipes"
        );
    }

    #[test]
    fn gate_child_drop_reaps_descendants_that_hold_pipes() {
        let fixture = tempfile::tempdir().expect("create drop pipe-holder fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let started = Instant::now();
        {
            let gate = GateChild::new(spawn_pipe_holding_child(&ready_file, &pid_file))
                .expect("capture pipe-holder child identity");
            wait_for_file(&ready_file, Duration::from_secs(2), "pipe-holder fixture");
            drop(gate);
        }

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GateChild Drop cleanup must not hang on inherited pipes"
        );
    }

    #[test]
    fn gate_child_timeout_terminates_setsid_escape_without_hanging() {
        let fixture = tempfile::tempdir().expect("create setsid pipe-holder fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut gate = GateChild::new(spawn_setsid_pipe_holding_child(
            &ready_file,
            &pid_file,
            false,
        ))
        .expect("capture setsid child identity");
        wait_for_file(
            &ready_file,
            Duration::from_secs(2),
            "setsid pipe-holder fixture",
        );
        let escaped = process_identity_from_pid_file(&pid_file);

        let started = Instant::now();
        let error = gate
            .wait_with_timeout(Duration::from_millis(50))
            .expect_err("a gate timeout must return an error");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GateChild timeout cleanup must not hang on setsid descendants"
        );
        assert_process_identity_gone(escaped);
    }

    #[test]
    fn gate_child_drop_terminates_setsid_escape_without_hanging() {
        let fixture = tempfile::tempdir().expect("create setsid drop fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let escaped;
        let started = Instant::now();
        {
            let gate = GateChild::new(spawn_setsid_pipe_holding_child(
                &ready_file,
                &pid_file,
                false,
            ))
            .expect("capture setsid child identity");
            wait_for_file(
                &ready_file,
                Duration::from_secs(2),
                "setsid pipe-holder fixture",
            );
            escaped = process_identity_from_pid_file(&pid_file);
            drop(gate);
        }

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GateChild Drop cleanup must not hang on setsid descendants"
        );
        assert_process_identity_gone(escaped);
    }

    #[test]
    fn gate_child_reaps_setsid_escape_after_leader_exits() {
        let fixture = tempfile::tempdir().expect("create exited-leader fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut child = spawn_setsid_pipe_holding_child(&ready_file, &pid_file, true);
        wait_for_file(
            &ready_file,
            Duration::from_secs(2),
            "setsid pipe-holder fixture",
        );
        let escaped = process_identity_from_pid_file(&pid_file);
        let root = capture_owned_child(&child).expect("capture exited leader identity");
        assert!(
            wait_for_child_exit(&mut child, Duration::from_secs(2))
                .expect("wait for exited leader"),
            "the fixture leader did not exit"
        );

        let started = Instant::now();
        let mut tracked_processes = Vec::new();
        let output = terminate_and_collect(child, &root, &mut tracked_processes)
            .expect("reap exited leader and escaped descendant");

        assert!(
            output.status.success(),
            "fixture leader status: {}",
            output.status
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cleanup must not depend on the escaped process retaining its parent"
        );
        assert_process_identity_gone(escaped);
    }

    #[test]
    fn rest_gate_fuser_diagnostic_cannot_outlive_lock_budget() {
        let fixture = tempfile::tempdir().expect("create fuser-timeout fixture");
        let bin_dir = fixture.path().join("bin");
        fs::create_dir(&bin_dir).expect("create fixture bin directory");
        symlink(
            repo_root().join("tests/fixtures/local-gate-command-proxy.sh"),
            bin_dir.join("fuser"),
        )
        .expect("link fuser proxy");
        let target = fixture.path().join("target");
        fs::create_dir(&target).expect("create isolated target");
        let target = fs::canonicalize(target).expect("canonical isolated target");
        let mut lock_file = target.as_os_str().to_os_string();
        lock_file.push(".lock");
        let lock_file = PathBuf::from(lock_file);
        let holder_ready_file = fixture.path().join("holder-ready");
        let mut holder_command = Command::new("/bin/bash");
        holder_command
            .args([
                "-c",
                r#"
                    exec {lock_fd}>"${LOCK_FILE:?}"
                    flock "${lock_fd}"
                    : >"${HOLDER_READY_FILE:?}"
                    exec /bin/sleep 60
                "#,
            ])
            .env("LOCK_FILE", &lock_file)
            .env("HOLDER_READY_FILE", &holder_ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _holder = GateChild::new(
            spawn_in_own_session(&mut holder_command).expect("spawn isolated lock holder"),
        )
        .expect("capture lock holder identity");
        wait_for_file(&holder_ready_file, Duration::from_secs(2), "lock holder");

        let inherited_path = std::env::var_os("PATH").expect("PATH is set");
        let path = std::env::join_paths(
            std::iter::once(bin_dir).chain(std::env::split_paths(&inherited_path)),
        )
        .expect("construct fixture PATH");
        let fuser_ready_file = fixture.path().join("fuser-ready");
        let fuser_pid_file = fixture.path().join("fuser.pid");
        let mut command = Command::new("/bin/bash");
        command
            .arg(repo_root().join("scripts/gates/rest-tests.sh"))
            .current_dir(repo_root())
            .env("PATH", path)
            .env("REST_GATE_DRY_RUN", "1")
            .env("REST_GATE_LOCK_TIMEOUT_SECS", "1")
            .env("REST_GATE_TARGET_DIR", &target)
            .env("REST_TEST_TARGETS_PER_BATCH", "999")
            .env("REST_GATE_FUSER_NEVER_RETURN_READY_FILE", &fuser_ready_file)
            .env("REST_GATE_FUSER_PID_FILE", &fuser_pid_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut gate =
            GateChild::new(spawn_in_own_session(&mut command).expect("spawn isolated REST gate"))
                .expect("capture isolated REST gate identity");
        wait_for_file(&fuser_ready_file, Duration::from_secs(2), "stalled fuser");

        let started = Instant::now();
        let output = gate
            .wait_with_timeout(Duration::from_secs(3))
            .expect("reap REST gate");

        assert_eq!(output.status.code(), Some(75));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the fuser diagnostic exceeded the advertised lock deadline: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
