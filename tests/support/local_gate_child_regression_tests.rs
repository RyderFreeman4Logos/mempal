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

    fn spawn_term_observing_session(
        ready_file: &Path,
        term_file: &Path,
    ) -> OwnedGateChild {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    trap ': >"${TERM_FILE:?}"; exit 0' TERM
                    : >"${READY_FILE:?}"
                    while true; do /bin/sleep 60; done
                "#,
            ])
            .env("READY_FILE", ready_file)
            .env("TERM_FILE", term_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        spawn_in_own_session(&mut command).expect("spawn TERM-observing session")
    }

    fn spawn_term_handler_setsid_escape(pid_file: &Path, ready_file: &Path) -> OwnedGateChild {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    escape() {
                        pid="${BASHPID}"
                        start_time="$(awk '{print $22}' "/proc/${pid}/stat")"
                        printf '%s %s\n' "${pid}" "${start_time}" >"${PID_FILE:?}"
                        : >"${READY_FILE:?}"
                        exec /bin/sleep 60
                    }
                    export -f escape
                    trap 'setsid /bin/bash -c escape </dev/null >/dev/null 2>&1 &
                          while [[ ! -e "${READY_FILE:?}" ]]; do :; done
                          /bin/sleep 0.02' TERM
                    /bin/sleep 60
                "#,
            ])
            .env("PID_FILE", pid_file)
            .env("READY_FILE", ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn TERM-handler escape fixture")
    }

    fn spawn_delayed_term_handler_setsid_escape(
        leader_ready_file: &Path,
        pid_file: &Path,
        escape_ready_file: &Path,
    ) -> OwnedGateChild {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    escape() {
                        pid="${BASHPID}"
                        start_time="$(awk '{print $22}' "/proc/${pid}/stat")"
                        printf '%s %s\n' "${pid}" "${start_time}" >"${PID_FILE:?}"
                        : >"${ESCAPE_READY_FILE:?}"
                        exec /bin/sleep 60
                    }
                    export -f escape
                    trap '/bin/sleep 0.005
                          setsid /bin/bash -c escape </dev/null >/dev/null 2>&1 &' TERM
                    : >"${LEADER_READY_FILE:?}"
                    /bin/sleep 60
                "#,
            ])
            .env("LEADER_READY_FILE", leader_ready_file)
            .env("PID_FILE", pid_file)
            .env("ESCAPE_READY_FILE", escape_ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn delayed TERM-handler escape fixture")
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

    #[test]
    fn reaped_gate_leader_does_not_signal_a_reused_process_group() {
        let mut leader_command = Command::new("/bin/sleep");
        leader_command
            .arg("0.01")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut leader =
            spawn_in_own_session(&mut leader_command).expect("spawn quickly exiting gate leader");
        let leader_root = capture_owned_child(leader.child()).expect("capture gate leader pidfd");
        assert!(
            wait_for_child_exit(leader.child_mut(), Duration::from_secs(1))
                .expect("reap quickly exiting gate leader"),
            "gate leader did not exit"
        );

        let fixture = tempfile::tempdir().expect("create reused-process-group fixture");
        let ready_file = fixture.path().join("unrelated-ready");
        let term_file = fixture.path().join("unrelated-term");
        let unrelated = spawn_term_observing_session(&ready_file, &term_file);
        wait_for_file(
            &ready_file,
            Duration::from_secs(1),
            "unrelated session leader",
        );
        let unrelated_identity = capture_owned_child(unrelated.child())
            .expect("capture unrelated session identity")
            .identity;

        // Model a recycled numeric leader PID: the pidfd still names the reaped gate leader,
        // while the numeric process-group ID now belongs to an unrelated setsid process.
        let recycled_root = ProcessHandle {
            identity: unrelated_identity,
            pidfd: leader_root.pidfd,
        };
        signal_root_process_tree(&recycled_root, false, libc::SIGTERM)
            .expect("reaped leader cleanup must not signal its recycled process group");
        thread::sleep(Duration::from_millis(50));
        let unrelated_survived = unrelated_identity
            .is_running()
            .expect("inspect unrelated session after reaped leader cleanup");
        let unrelated_received_term = term_file.exists();
        let _ = reap_owned_child(unrelated);

        assert!(
            unrelated_survived && !unrelated_received_term,
            "reaped gate cleanup must not signal an unrelated recycled process group"
        );
    }

    #[test]
    fn gate_child_reaps_setsid_descendant_created_by_term_handler() {
        let fixture = tempfile::tempdir().expect("create TERM-handler escape fixture");
        let pid_file = fixture.path().join("escaped.pid");
        let ready_file = fixture.path().join("escaped-ready");
        let mut gate = GateChild::new(spawn_term_handler_setsid_escape(&pid_file, &ready_file))
            .expect("capture TERM-handler escape leader");

        let started = Instant::now();
        let error = gate
            .wait_with_timeout(Duration::from_millis(25))
            .expect_err("TERM-handler fixture must time out");
        wait_for_file(
            &ready_file,
            Duration::from_secs(1),
            "TERM-handler setsid descendant",
        );
        let escaped = escaped_identity(&pid_file);
        let escaped_survived = capture_recorded_process(escaped)
            .expect("re-verify TERM-handler descendant identity")
            .is_some();
        if let Some(process) = capture_recorded_process(escaped)
            .expect("capture TERM-handler descendant only if its identity still matches")
        {
            process
                .send_signal(libc::SIGKILL)
                .expect("pidfd-safe fallback cleanup for failed containment assertion");
        }

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_millis(25) + CLEANUP_TIMEOUT_MARGIN,
            "TERM-handler descendant cleanup exceeded its deadline"
        );
        assert!(
            !escaped_survived,
            "a setsid descendant created by a TERM handler must be reaped during cleanup"
        );
    }

    #[test]
    fn gate_child_drop_reaps_delayed_non_pipe_setsid_escape_from_term_handler() {
        let fixture = tempfile::tempdir().expect("create delayed TERM-handler escape fixture");
        let pid_file = fixture.path().join("escaped.pid");
        let leader_ready_file = fixture.path().join("leader-ready");
        let escape_ready_file = fixture.path().join("escaped-ready");
        let gate = GateChild::new(spawn_delayed_term_handler_setsid_escape(
            &leader_ready_file,
            &pid_file,
            &escape_ready_file,
        ))
        .expect("capture delayed TERM-handler escape leader");
        wait_for_file(
            &leader_ready_file,
            Duration::from_secs(1),
            "delayed TERM-handler leader",
        );

        let started = Instant::now();
        drop(gate);
        wait_for_file(
            &escape_ready_file,
            Duration::from_secs(1),
            "delayed TERM-handler setsid descendant",
        );
        let escaped = escaped_identity(&pid_file);
        let escaped_survived = capture_recorded_process(escaped)
            .expect("re-verify delayed TERM-handler descendant identity")
            .is_some();
        if let Some(process) = capture_recorded_process(escaped)
            .expect("capture delayed TERM-handler descendant only if its identity still matches")
        {
            process
                .send_signal(libc::SIGKILL)
                .expect("pidfd-safe fallback cleanup for failed containment assertion");
        }

        assert!(
            started.elapsed() <= CLEANUP_TIMEOUT_MARGIN,
            "delayed TERM-handler descendant cleanup exceeded its deadline"
        );
        assert!(
            !escaped_survived,
            "a delayed setsid descendant without output pipes must be reaped during Drop"
        );
    }
}
