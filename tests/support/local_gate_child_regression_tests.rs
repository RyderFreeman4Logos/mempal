#[cfg(test)]
mod regression_tests {
    use super::*;
    use std::fs;
    use std::io::{Read, Write};
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
        started_file: &Path,
        ready_file: &Path,
        pid_file: &Path,
    ) -> OwnedGateChild {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    : >"${STARTED_FILE:?}"
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
            .env("STARTED_FILE", started_file)
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

    fn spawn_term_handler_setsid_escape(
        pid_file: &Path,
        armed_file: &Path,
        ready_file: &Path,
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
                        : >"${READY_FILE:?}"
                        exec /bin/sleep 60
                    }
                    export -f escape
                    trap 'setsid /bin/bash -c escape </dev/null >/dev/null 2>&1 &
                          while [[ ! -e "${READY_FILE:?}" ]]; do :; done
                          /bin/sleep 0.02' TERM
                    : >"${ARMED_FILE:?}"
                    /bin/sleep 60
                "#,
            ])
            .env("PID_FILE", pid_file)
            .env("ARMED_FILE", armed_file)
            .env("READY_FILE", ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn TERM-handler escape fixture")
    }

    fn spawn_closing_pipe_escape_after_release(
        release_file: &Path,
        started_file: &Path,
        ready_file: &Path,
        parent_ready_file: &Path,
        close_file: &Path,
        closed_file: &Path,
        pid_file: &Path,
    ) -> OwnedGateChild {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    : >"${STARTED_FILE:?}"
                    while [[ ! -e "${RELEASE_FILE:?}" ]]; do /bin/sleep 0.01; done
                    exec 3>&1 4>&2
                    IFS= read -r _ < <(
                        setsid /bin/bash -c '
                            trap "" TERM
                            pid="${BASHPID}"
                            start_time="$(awk "{print \$22}" "/proc/${pid}/stat")"
                            printf "%s %s\n" "${pid}" "${start_time}" >"${PID_FILE:?}"
                            : >"${READY_FILE:?}"
                            printf "ready\n"
                            exec >&3 2>&4
                            while [[ ! -e "${CLOSE_FILE:?}" ]]; do /bin/sleep 0.01; done
                            exec </dev/null >/dev/null 2>&1 3>&- 4>&-
                            : >"${CLOSED_FILE:?}"
                            while true; do /bin/sleep 60; done
                        ' 3>&3 4>&4
                    )
                    : >"${PARENT_READY_FILE:?}"
                    exit 0
                "#,
            ])
            .env("RELEASE_FILE", release_file)
            .env("STARTED_FILE", started_file)
            .env("READY_FILE", ready_file)
            .env("PARENT_READY_FILE", parent_ready_file)
            .env("CLOSE_FILE", close_file)
            .env("CLOSED_FILE", closed_file)
            .env("PID_FILE", pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn closing-pipe escape fixture")
    }

    fn wait_for_owned_child_ready(
        child: &mut OwnedGateChild,
        process: &ProcessHandle,
        ready_file: &Path,
        timeout: Duration,
        description: &str,
    ) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if ready_file.exists() {
                let recorded = escaped_identity(ready_file);
                if recorded.pid != process.identity.pid
                    || recorded.start_time_ticks != process.identity.start_time_ticks
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!(
                            "{description} published the wrong identity: expected {:?}, got {recorded:?}",
                            process.identity
                        ),
                    ));
                }
                if process.is_running()? {
                    return Ok(());
                }
            }
            if let Some(status) = child.child_mut().try_wait()? {
                let mut stderr = Vec::new();
                if let Some(pipe) = child.child_mut().stderr.take() {
                    pipe.take(4096).read_to_end(&mut stderr)?;
                }
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    format!(
                        "{description} exited before readiness: {status}; stderr={}",
                        String::from_utf8_lossy(&stderr)
                    ),
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("{description} did not become ready"),
                ));
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(10)),
            );
        }
    }

    fn spawn_non_utf8_comm_process(ready_file: &Path) -> OwnedGateChild {
        let mut command = Command::new("/usr/bin/python3");
        command
            .args([
                "-c",
                r#"
import ctypes
import os
import time

if ctypes.CDLL(None, use_errno=True).prctl(15, b"\xff", 0, 0, 0) != 0:
    raise OSError(ctypes.get_errno(), "prctl(PR_SET_NAME) failed")
pid = os.getpid()
comm = open("/proc/self/comm", "rb").read()
if comm != b"\xff\n":
    raise RuntimeError(f"non-UTF-8 comm was not established: {comm!r}")
fields = open(f"/proc/{pid}/stat", "rb").read().rpartition(b") ")[2].split()
ready = os.environ["READY_FILE"]
temporary = ready + ".tmp"
with open(temporary, "xb") as marker:
    marker.write(str(pid).encode() + b" " + fields[19] + b"\n")
os.replace(temporary, ready)
while True:
    time.sleep(60)
                "#,
            ])
            .env("READY_FILE", ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn non-UTF-8 comm fixture")
    }

    pub(super) fn assert_non_utf8_comm_does_not_abort_pipe_cleanup() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create non-UTF-8 comm fixture");
        let ready_file = fixture.path().join("identity");
        let mut unrelated = spawn_non_utf8_comm_process(&ready_file);
        let unrelated_process =
            capture_owned_child(unrelated.child()).expect("capture non-UTF-8 fixture identity");
        let unrelated_identity = unrelated_process.identity;
        wait_for_owned_child_ready(
            &mut unrelated,
            &unrelated_process,
            &ready_file,
            Duration::from_secs(2),
            "non-UTF-8 comm process",
        )
        .expect("wait for identity-bound non-UTF-8 readiness");
        let comm = fs::read(format!("/proc/{}/comm", unrelated_identity.pid))
            .expect("read live non-UTF-8 fixture comm");
        assert_eq!(comm, b"\xff\n", "fixture must establish non-UTF-8 comm");

        let mut writer_command = Command::new("/bin/sleep");
        writer_command
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let writer = spawn_in_own_session(&mut writer_command).expect("spawn pipe writer");
        let mut gate = GateChild::new(writer).expect("capture pipe writer identity");
        let scan_result = gate.refresh_tracked_processes();
        let cleanup_result = gate.terminate_and_collect_until(cleanup_deadline()).map(|_| ());
        let unrelated_cleanup = reap_owned_child(unrelated);

        scan_result.expect("unrelated non-UTF-8 comm must be skipped during global scan");
        cleanup_result.expect("pipe cleanup must complete after scanning unrelated processes");
        unrelated_cleanup.expect("clean up non-UTF-8 comm fixture");
        assert!(
            !unrelated_identity
                .is_running()
                .expect("inspect reaped non-UTF-8 fixture identity"),
            "non-UTF-8 fixture remained alive after cleanup"
        );
    }

    #[test]
    fn readiness_wait_reports_pre_ready_child_exit_promptly() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create pre-ready exit fixture");
        let ready_file = fixture.path().join("never-ready");
        let mut command = Command::new("/bin/bash");
        command
            .args(["-c", "read -r _; printf 'fixture failed' >&2; exit 23"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        let mut child = spawn_in_own_session(&mut command).expect("spawn pre-ready exit fixture");
        let process =
            capture_owned_child(child.child()).expect("capture pre-ready fixture identity");
        let identity = process.identity;
        child
            .child_mut()
            .stdin
            .take()
            .expect("pre-ready fixture stdin")
            .write_all(b"exit\n")
            .expect("release pre-ready fixture");

        let started = Instant::now();
        let error = wait_for_owned_child_ready(
            &mut child,
            &process,
            &ready_file,
            Duration::from_secs(2),
            "pre-ready fixture",
        )
        .expect_err("pre-ready exit must fail readiness");
        let elapsed = started.elapsed();
        let message = error.to_string();
        drop(child);

        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert!(
            elapsed < Duration::from_secs(1),
            "pre-ready child exit was reported only after {elapsed:?}: {message}"
        );
        assert!(message.contains("exit status: 23"), "message={message}");
        assert!(message.contains("fixture failed"), "message={message}");
        assert!(
            !identity
                .is_running()
                .expect("inspect reaped pre-ready fixture identity"),
            "pre-ready fixture remained alive after cleanup"
        );
    }

    #[test]
    fn gate_child_terminates_post_reap_non_pipe_setsid_descendant() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create non-pipe escaped-descendant fixture");
        let release_file = fixture.path().join("release");
        let started_file = fixture.path().join("started");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut gate = GateChild::new(spawn_non_pipe_setsid_escape_after_release(
            &release_file,
            &started_file,
            &ready_file,
            &pid_file,
        ))
        .expect("capture non-pipe escape leader");

        gate.descendant_monitor
            .stop_and_drain(
                &mut gate.tracked_processes,
                Instant::now() + Duration::from_secs(1),
            )
            .expect("stop descendant monitor before exercising ownership fallback");

        wait_for_file(
            &started_file,
            Duration::from_secs(2),
            "non-pipe escaped descendant release waiter",
        );
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
            .expect("finish cleanup for reparented non-pipe descendant");
        let escaped_is_running = match capture_recorded_process(escaped)
            .expect("re-verify escaped descendant identity")
        {
            Some(process) => {
                let is_running = process
                    .is_running()
                    .expect("inspect escaped descendant running state");
                if is_running {
                    process
                        .send_signal(libc::SIGKILL)
                        .expect("pidfd-safe fallback cleanup for a running escaped descendant");
                }
                is_running
            }
            None => false,
        };

        assert!(output.status.success(), "leader status: {}", output.status);
        assert!(
            !escaped_is_running,
            "a post-reap setsid descendant without output pipes must terminate"
        );
    }

    #[test]
    fn gate_child_terminates_closed_pipe_descendant_by_ownership_token() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create closed-pipe descendant fixture");
        let release_file = fixture.path().join("release");
        let started_file = fixture.path().join("started");
        let ready_file = fixture.path().join("ready");
        let parent_ready_file = fixture.path().join("parent-ready");
        let close_file = fixture.path().join("close");
        let closed_file = fixture.path().join("closed");
        let pid_file = fixture.path().join("pid");
        let mut gate = GateChild::new(spawn_closing_pipe_escape_after_release(
            &release_file,
            &started_file,
            &ready_file,
            &parent_ready_file,
            &close_file,
            &closed_file,
            &pid_file,
        ))
        .expect("capture closing-pipe escape leader");

        wait_for_file(
            &started_file,
            Duration::from_secs(5),
            "closing-pipe escaped descendant release waiter",
        );
        gate.descendant_monitor
            .stop_and_drain(
                &mut gate.tracked_processes,
                Instant::now() + Duration::from_secs(1),
            )
            .expect("stop descendant monitor before exercising ownership fallback");
        fs::write(&release_file, "release\n").expect("release escaped descendant creation");
        wait_for_file(
            &ready_file,
            Duration::from_secs(2),
            "closing-pipe escaped descendant",
        );
        wait_for_file(
            &parent_ready_file,
            Duration::from_secs(2),
            "closing-pipe escaped descendant parent readiness",
        );
        let escaped = escaped_identity(&pid_file);
        assert!(
            wait_for_child_exit(gate.child.child_mut(), Duration::from_secs(2))
                .expect("reap closing-pipe leader"),
            "leader did not exit after creating its closing-pipe descendant"
        );
        fs::write(&close_file, "close\n").expect("release descendant pipe closure");
        wait_for_file(
            &closed_file,
            Duration::from_secs(2),
            "escaped descendant pipe closure",
        );
        gate.refresh_tracked_processes()
            .expect("capture escaped descendant from its inherited ownership token after pipe closure");

        let output = gate
            .wait_with_timeout(Duration::from_millis(50))
            .expect("finish cleanup for escaped descendant after output-pipe closure");
        let escaped_is_running = match capture_recorded_process(escaped)
            .expect("re-verify escaped descendant identity")
        {
            Some(process) => {
                let is_running = process
                    .is_running()
                    .expect("inspect escaped descendant running state");
                if is_running {
                    process
                        .send_signal(libc::SIGKILL)
                        .expect("pidfd-safe fallback cleanup for a running escaped descendant");
                }
                is_running
            }
            None => false,
        };

        assert!(output.status.success(), "leader status: {}", output.status);
        assert!(
            !escaped_is_running,
            "the escaped descendant discovered by its inherited ownership token must terminate after output-pipe closure"
        );
    }

    #[test]
    fn gate_child_timeout_bounds_proc_discovery_with_fd_heavy_sibling() {
        let _process_lock = process_lifecycle_test_lock_blocking();
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
        let _process_lock = process_lifecycle_test_lock_blocking();
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
        signal_root_process_tree(leader.child(), &recycled_root, libc::SIGTERM)
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
    fn gate_child_terminates_setsid_descendant_created_by_term_handler() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create TERM-handler escape fixture");
        let pid_file = fixture.path().join("escaped.pid");
        let armed_file = fixture.path().join("term-handler-armed");
        let ready_file = fixture.path().join("escaped-ready");
        let mut gate = GateChild::new(spawn_term_handler_setsid_escape(
            &pid_file,
            &armed_file,
            &ready_file,
        ))
        .expect("capture TERM-handler escape leader");

        wait_for_file(
            &armed_file,
            Duration::from_secs(1),
            "TERM-handler escape fixture arming",
        );

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
        let escaped_is_running = match capture_recorded_process(escaped)
            .expect("re-verify TERM-handler descendant identity")
        {
            Some(process) => {
                let is_running = process
                    .is_running()
                    .expect("inspect TERM-handler descendant running state");
                if is_running {
                    process
                        .send_signal(libc::SIGKILL)
                        .expect("pidfd-safe fallback cleanup for a running TERM-handler descendant");
                }
                is_running
            }
            None => false,
        };

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_millis(25) + CLEANUP_TIMEOUT_MARGIN,
            "TERM-handler descendant cleanup exceeded its deadline"
        );
        assert!(
            !escaped_is_running,
            "a setsid descendant created by a TERM handler must terminate during cleanup"
        );
    }
}
