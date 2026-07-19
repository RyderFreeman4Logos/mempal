#[cfg(test)]
mod regression_tests {
    use super::*;
    use std::fs;
    use std::path::Path;
    use std::process::{Command, Stdio};

    fn wait_for_file(path: &Path, timeout: Duration, description: &str) {
        let deadline = Instant::now() + timeout;
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(path.exists(), "{description} did not become ready");
    }

    fn escaped_identity(pid_file: &Path) -> RecordedProcessIdentity {
        let record = fs::read_to_string(pid_file).expect("read escaped descendant identity");
        let mut fields = record.split_ascii_whitespace();
        let pid = fields
            .next()
            .expect("escaped descendant PID")
            .parse()
            .expect("numeric escaped descendant PID");
        let start_time_ticks = fields
            .next()
            .expect("escaped descendant start time")
            .parse()
            .expect("numeric escaped descendant start time");
        assert!(
            fields.next().is_none(),
            "escaped descendant identity contains unexpected fields"
        );
        RecordedProcessIdentity {
            pid,
            start_time_ticks,
        }
    }

    fn spawn_non_pipe_setsid_escape_after_release(
        release_file: &Path,
        ready_file: &Path,
        pid_file: &Path,
    ) -> OwnedGateChild {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    while [[ ! -e "${RELEASE_FILE:?}" ]]; do /bin/sleep 0.01; done
                    setsid /bin/bash -c '
                        trap "" TERM
                        pid="${BASHPID}"
                        start_time="$(awk "{print \$22}" "/proc/${pid}/stat")"
                        printf "%s %s\n" "${pid}" "${start_time}" >"${PID_FILE:?}"
                        : >"${READY_FILE:?}"
                        while true; do /bin/sleep 60; done
                    ' </dev/null >/dev/null 2>&1 &
                    while [[ ! -e "${READY_FILE:?}" ]]; do /bin/sleep 0.01; done
                    # Keep the leader alive long enough for its owner to observe the new child.
                    /bin/sleep 0.2
                    exit 0
                "#,
            ])
            .env("RELEASE_FILE", release_file)
            .env("READY_FILE", ready_file)
            .env("PID_FILE", pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn non-pipe escape fixture")
    }

    fn spawn_fd_heavy_sibling(ready_file: &Path) -> OwnedGateChild {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    for ((fd = 3; fd < 16384; fd++)); do
                        eval "exec ${fd}</dev/null" || break
                    done
                    : >"${READY_FILE:?}"
                    exec /bin/sleep 60
                "#,
            ])
            .env("READY_FILE", ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        spawn_in_own_session(&mut command).expect("spawn FD-heavy unrelated sibling")
    }

    #[test]
    fn gate_child_reaps_post_reap_non_pipe_setsid_descendant() {
        let fixture = tempfile::tempdir().expect("create non-pipe escaped-descendant fixture");
        let release_file = fixture.path().join("release");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut gate = GateChild::new(spawn_non_pipe_setsid_escape_after_release(
            &release_file,
            &ready_file,
            &pid_file,
        ))
        .expect("capture non-pipe escape leader");

        fs::write(&release_file, "release\n").expect("release escaped descendant creation");
        wait_for_file(
            &ready_file,
            Duration::from_secs(2),
            "non-pipe escaped descendant",
        );
        let escaped = escaped_identity(&pid_file);
        assert!(
            wait_for_child_exit(gate.child.child_mut(), Duration::from_secs(2))
                .expect("wait for leader exit"),
            "leader did not exit after creating its non-pipe descendant"
        );

        let output = gate
            .wait_with_timeout(Duration::from_millis(50))
            .expect("reap reparented non-pipe descendant");
        let escaped_survived = capture_recorded_process(escaped)
            .expect("re-verify escaped descendant identity")
            .is_some();
        if let Some(process) = capture_recorded_process(escaped)
            .expect("capture escaped descendant only if its identity still matches")
        {
            process
                .send_signal(libc::SIGKILL)
                .expect("pidfd-safe fallback cleanup for failed assertion");
        }

        assert!(output.status.success(), "leader status: {}", output.status);
        assert!(
            !escaped_survived,
            "a post-reap setsid descendant without output pipes must be reaped"
        );
    }

    #[test]
    fn gate_child_timeout_bounds_proc_discovery_with_fd_heavy_sibling() {
        let fixture = tempfile::tempdir().expect("create FD-heavy discovery fixture");
        let ready_file = fixture.path().join("fd-heavy-ready");
        let sibling = spawn_fd_heavy_sibling(&ready_file);
        wait_for_file(&ready_file, Duration::from_secs(10), "FD-heavy unrelated sibling");

        let mut command = Command::new("/bin/sleep");
        command
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut gate = GateChild::new(
            spawn_in_own_session(&mut command).expect("spawn deadline-bounded gate child"),
        )
        .expect("capture deadline-bounded gate child");

        let timeout = Duration::from_millis(25);
        let started = Instant::now();
        let error = gate
            .wait_with_timeout(timeout)
            .expect_err("sleeping gate child must time out");
        let elapsed = started.elapsed();
        reap_owned_child(sibling).expect("clean up FD-heavy unrelated sibling");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            elapsed < timeout + CLEANUP_TIMEOUT_MARGIN,
            "deadline-bounded discovery exceeded its timeout margin: {elapsed:?}"
        );
    }
}
