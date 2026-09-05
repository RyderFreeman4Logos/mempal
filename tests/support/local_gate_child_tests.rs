#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::fs;
    use std::os::fd::AsFd;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::process::{Command, Stdio};
    use std::sync::mpsc;

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn wait_for_file(path: &Path, timeout: Duration, description: &str) {
        let deadline = Instant::now() + timeout;
        while !path.exists() && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(Duration::from_millis(25).min(remaining));
        }
        assert!(path.exists(), "{description} did not become ready");
    }

    fn spawn_pipe_holding_child(ready_file: &Path, pid_file: &Path) -> OwnedGateChild {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    (
                        trap '' TERM
                        pid="${BASHPID}"
                        start_time="$(awk '{print $22}' "/proc/${pid}/stat")"
                        printf '%s %s\n' "${pid}" "${start_time}" >"${PID_FILE:?}"
                        exec /bin/sleep 60
                    ) &
                    while [[ ! -s "${PID_FILE:?}" ]]; do /bin/sleep 0.01; done
                    : >"${READY_FILE:?}"
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
    ) -> OwnedGateChild {
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
                        /bin/sleep 0.2
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

    fn spawn_setsid_escape_after_release(
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
                        while true; do
                            printf "escaped stdout\n"
                            printf "escaped stderr\n" >&2
                            /bin/sleep 60
                        done
                    ' &
                    while [[ ! -e "${READY_FILE:?}" ]]; do /bin/sleep 0.01; done
                    exit 0
                "#,
            ])
            .env("RELEASE_FILE", release_file)
            .env("READY_FILE", ready_file)
            .env("PID_FILE", pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn release-coordinated fixture")
    }

    fn spawn_non_utf8_comm_process(ready_file: &Path) -> OwnedGateChild {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    printf '\377' > /proc/self/comm
                    : >"${READY_FILE:?}"
                    while true; do /bin/sleep 60; done
                "#,
            ])
            .env("READY_FILE", ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        spawn_in_own_session(&mut command).expect("spawn non-UTF-8 comm fixture")
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

    fn assert_process_identity_gone_within(identity: ProcessIdentity, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while identity
            .is_running()
            .expect("inspect escaped process identity")
            && Instant::now() < deadline
        {
            thread::sleep(Duration::from_millis(10));
        }
        assert_process_identity_gone(identity);
    }

    fn spawn_sleeping_direct_child() -> OwnedGateChild {
        let mut command = Command::new("/bin/sleep");
        command
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        spawn_in_own_session(&mut command).expect("spawn isolated direct child")
    }

    fn assert_capture_failure_reaps_direct_child(
        capture: impl FnOnce(&Child) -> io::Result<ProcessHandle>,
    ) {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let child = spawn_sleeping_direct_child();
        let identity = capture_owned_child(child.child())
            .expect("capture direct child identity before injected failure")
            .identity;

        let error = match GateChild::new_with_capture_for_test(child, capture) {
            Ok(_) => panic!("injected setup failure must be returned"),
            Err(error) => error,
        };

        assert!(!error.to_string().is_empty(), "failure must retain diagnostics");
        assert_process_identity_gone_within(identity, Duration::from_secs(1));
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
    fn procfs_capture_failure_reaps_direct_child() {
        assert_capture_failure_reaps_direct_child(|_| {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "injected /proc inspection failure",
            ))
        });
    }
    #[test]
    fn pidfd_capture_failure_reaps_direct_child() {
        assert_capture_failure_reaps_direct_child(|_| {
            Err(io::Error::other("injected pidfd_open failure"))
        });
    }
    #[test]
    fn fd_exhaustion_during_capture_reaps_direct_child() {
        assert_capture_failure_reaps_direct_child(|_| {
            Err(io::Error::from_raw_os_error(libc::EMFILE))
        });
    }
    #[test]
    fn panic_during_capture_reaps_direct_child() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let child = spawn_sleeping_direct_child();
        let identity = capture_owned_child(child.child())
            .expect("capture direct child identity before injected panic")
            .identity;

        let panic = catch_unwind(AssertUnwindSafe(|| {
            let _ = GateChild::new_with_capture_for_test(child, |_| -> io::Result<ProcessHandle> {
                panic!("injected capture panic");
            });
        }));

        assert!(panic.is_err(), "injected setup panic must unwind");
        assert_process_identity_gone_within(identity, Duration::from_secs(1));
    }
    #[test]
    fn gate_child_success_reaps_the_direct_child() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let mut command = Command::new("/bin/true");
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = spawn_in_own_session(&mut command).expect("spawn isolated success child");
        let identity = capture_owned_child(child.child())
            .expect("capture success child identity")
            .identity;
        let mut gate = GateChild::new(child).expect("capture success gate child identity");

        let output = gate
            .wait_with_timeout(Duration::from_secs(1))
            .expect("collect successful child output");

        assert!(output.status.success());
        assert_process_identity_gone_within(identity, Duration::from_secs(1));
    }
    #[test]
    fn gate_child_timeout_reaps_the_direct_child() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let child = spawn_sleeping_direct_child();
        let identity = capture_owned_child(child.child())
            .expect("capture timeout child identity")
            .identity;
        let mut gate = GateChild::new(child).expect("capture timeout gate child identity");

        let error = gate
            .wait_with_timeout(Duration::from_millis(25))
            .expect_err("sleeping child must time out");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_process_identity_gone_within(identity, Duration::from_secs(1));
    }
    #[test]
    fn gate_child_drop_reaps_the_direct_child() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let child = spawn_sleeping_direct_child();
        let identity = capture_owned_child(child.child())
            .expect("capture drop child identity")
            .identity;
        let gate = GateChild::new(child).expect("capture drop gate child identity");

        drop(gate);

        assert_process_identity_gone_within(identity, Duration::from_secs(1));
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
    fn pipe_reader_sibling_is_never_tracked_or_signaled_as_a_writer() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create pipe-endpoint fixture");
        let sibling_ready = fixture.path().join("sibling-ready");
        let term_file = fixture.path().join("sibling-term");
        let mut writer_command = Command::new("/bin/sleep");
        writer_command
            .arg("60")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let writer = spawn_in_own_session(&mut writer_command).expect("spawn pipe writer");
        let stdout = writer
            .child()
            .stdout
            .as_ref()
            .expect("writer stdout is piped");
        let reader = File::from(
            stdout
                .as_fd()
                .try_clone_to_owned()
                .expect("clone pipe read endpoint"),
        );

        let mut sibling_command = Command::new("/bin/bash");
        sibling_command
            .args([
                "-c",
                r#"
                    trap ': >"${TERM_FILE:?}"; exit 0' TERM
                    : >"${READY_FILE:?}"
                    while true; do /bin/sleep 60; done
                "#,
            ])
            .env("READY_FILE", &sibling_ready)
            .env("TERM_FILE", &term_file)
            .stdin(Stdio::from(reader))
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let sibling = spawn_in_own_session(&mut sibling_command).expect("spawn pipe reader");
        let sibling_handle =
            capture_owned_child(sibling.child()).expect("capture pipe reader identity");
        wait_for_file(&sibling_ready, Duration::from_secs(2), "pipe reader sibling");

        let mut gate = GateChild::new(writer).expect("capture pipe writer identity");
        gate.refresh_tracked_processes()
            .expect("discover pipe writers without aborting");
        let pipe_targets = output_pipe_targets(gate.child.child())
            .expect("read gate pipe targets");
        let fallback = TrackedProcess::pipe_fallback(
            ProcessHandle::capture(sibling_handle.identity.pid)
                .expect("recapture pipe reader for fallback recheck")
                .expect("pipe reader remains visible for fallback recheck"),
            pipe_targets,
        );
        let fallback_error = signal_tracked_processes(
            &[fallback],
            libc::SIGTERM,
            Instant::now() + Duration::from_secs(1),
        )
        .expect_err("a read-only pipe endpoint must fail fallback ownership revalidation");
        let reader_was_tracked = gate
            .tracked_processes
            .iter()
            .any(|tracked| tracked.identity() == sibling_handle.identity);

        drop(gate);
        thread::sleep(Duration::from_millis(50));
        let reader_survived_cleanup = sibling_handle
            .is_running()
            .expect("inspect pipe reader after gate cleanup");
        let reader_received_term = term_file.exists();
        let _ = reap_owned_child(sibling);

        assert!(
            !reader_was_tracked,
            "a sibling holding only the pipe read end must not become a fallback handle"
        );
        assert!(
            reader_survived_cleanup && !reader_received_term,
            "a read-end-only sibling must not be signaled by pipe fallback cleanup: {fallback_error}"
        );
    }
    #[test]
    fn reaped_leader_performs_a_final_pipe_descendant_discovery() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create post-reap escape fixture");
        let release_file = fixture.path().join("release");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut child =
            spawn_setsid_escape_after_release(&release_file, &ready_file, &pid_file);
        let root = capture_owned_child(child.child()).expect("capture release-coordinated root");
        let mut tracked_processes = Vec::new();
        refresh_owned_processes(
            child.child(),
            &root,
            &mut tracked_processes,
            Instant::now() + Duration::from_secs(1),
        )
            .expect("initial discovery before escape creation");

        fs::write(&release_file, "release\n").expect("release escaped descendant creation");
        wait_for_file(
            &ready_file,
            Duration::from_secs(2),
            "post-reap escaped descendant",
        );
        let escaped = process_identity_from_pid_file(&pid_file);
        assert!(
            wait_for_child_exit(child.child_mut(), Duration::from_secs(2))
                .expect("wait for leader exit"),
            "leader did not exit after creating its escaped descendant"
        );

        refresh_after_leader_reap(
            child.child_mut(),
            &root,
            &mut tracked_processes,
            Instant::now() + Duration::from_secs(1),
        )
            .expect("rediscover pipe descendants after leader reaping");
        let escaped_was_rediscovered = tracked_processes
            .iter()
            .any(|tracked| tracked.identity() == escaped);
        let output = terminate_and_collect(&mut child, &root, &mut tracked_processes)
            .expect("terminate rediscovered escaped descendant");

        assert!(output.status.success(), "leader status: {}", output.status);
        assert!(
            escaped_was_rediscovered,
            "the final post-reap scan must retain the escaped pipe holder"
        );
        assert_process_identity_gone_within(escaped, Duration::from_secs(1));
    }
    #[test]
    fn discovers_children_created_by_non_leader_threads() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let parent_pid = i32::try_from(std::process::id()).expect("test process PID fits i32");
        let parent = ProcessHandle::capture(parent_pid)
            .expect("capture test process")
            .expect("test process remains visible in procfs");
        let (child_pid_sender, child_pid_receiver) = mpsc::channel();
        let (release_sender, release_receiver) = mpsc::channel();
        let worker = thread::spawn(move || {
            let child = OwnedGateChild::new(
                Command::new("/bin/sleep")
                .arg("60")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .expect("spawn child from worker thread"),
            );
            child_pid_sender
                .send(child.child().id())
                .expect("report worker-thread child PID");
            release_receiver
                .recv()
                .expect("receive worker-thread child cleanup signal");
            drop(child);
        });
        let child_pid = i32::try_from(
            child_pid_receiver
                .recv_timeout(Duration::from_secs(2))
                .expect("receive worker-thread child PID"),
        )
        .expect("worker-thread child PID fits i32");

        let descendants = capture_live_children(parent.identity, Instant::now() + Duration::from_secs(1))
            .expect("enumerate children across every parent task");
        release_sender
            .send(())
            .expect("release worker-thread child cleanup");
        worker.join().expect("join child-spawning worker thread");

        assert!(
            descendants
                .iter()
                .any(|descendant| descendant.identity.pid == child_pid),
            "the child created by a non-leader thread must be discovered"
        );
    }

    #[test]
    fn unrelated_non_utf8_comm_does_not_abort_pipe_cleanup() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create non-UTF-8 comm fixture");
        let unrelated_ready = fixture.path().join("unrelated-ready");
        let unrelated = spawn_non_utf8_comm_process(&unrelated_ready);
        wait_for_file(
            &unrelated_ready,
            Duration::from_secs(2),
            "non-UTF-8 comm process",
        );

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
    }
    #[test]
    fn gate_child_wait_timeout_reaps_descendants_that_hold_pipes() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create pipe-holder fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut gate = GateChild::new(spawn_pipe_holding_child(&ready_file, &pid_file))
            .expect("capture pipe-holder child identity");
        wait_for_file(&ready_file, Duration::from_secs(10), "pipe-holder fixture");
        let descendant = process_identity_from_pid_file(&pid_file);
        let started = Instant::now();
        let error = gate
            .wait_with_timeout(Duration::from_millis(250))
            .expect_err("a gate timeout must return an error");
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        drop(gate);
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "GateChild timeout cleanup must not hang on inherited pipes"
        );
        assert_process_identity_gone_within(descendant, Duration::from_secs(5));
    }
    #[test]
    fn gate_child_drop_reaps_descendants_that_hold_pipes() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create drop pipe-holder fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let started = Instant::now();
        {
            let gate = GateChild::new(spawn_pipe_holding_child(&ready_file, &pid_file))
                .expect("capture pipe-holder child identity");
            wait_for_file(&ready_file, Duration::from_secs(10), "pipe-holder fixture");
            let descendant = process_identity_from_pid_file(&pid_file);
            drop(gate);
            assert_process_identity_gone_within(descendant, Duration::from_secs(5));
        }
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "GateChild Drop cleanup must not hang on inherited pipes"
        );
    }
    #[test]
    fn gate_child_timeout_terminates_setsid_escape_without_hanging() {
        let _process_lock = process_lifecycle_test_lock_blocking();
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
        assert_process_identity_gone_within(escaped, Duration::from_secs(1));
    }

    #[test]
    fn gate_child_drop_terminates_setsid_escape_without_hanging() {
        let _process_lock = process_lifecycle_test_lock_blocking();
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
        assert_process_identity_gone_within(escaped, Duration::from_secs(1));
    }
    #[test]
    fn gate_child_reaps_setsid_escape_after_leader_exits() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create exited-leader fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut gate = GateChild::new(spawn_setsid_pipe_holding_child(
            &ready_file,
            &pid_file,
            true,
        ))
        .expect("capture exited leader identity");
        wait_for_file(
            &ready_file,
            Duration::from_secs(2),
            "setsid pipe-holder fixture",
        );
        let escaped = process_identity_from_pid_file(&pid_file);
        assert!(
            wait_for_child_exit(gate.child.child_mut(), Duration::from_secs(2))
                .expect("wait for exited leader"),
            "the fixture leader did not exit"
        );

        let started = Instant::now();
        let output = gate
            .wait_with_timeout(Duration::from_millis(50))
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
        assert_process_identity_gone_within(escaped, Duration::from_secs(1));
    }
    #[test]
    fn rest_gate_fuser_diagnostic_cannot_outlive_lock_budget() {
        let _process_lock = process_lifecycle_test_lock_blocking();
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
