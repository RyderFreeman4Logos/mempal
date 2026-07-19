use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use super::*;

const FIXTURE_CASE_ENV: &str = "MEMPAL_SUPERVISOR_FIXTURE_CASE";
const DESCENDANT_CASE_ENV: &str = "MEMPAL_SUPERVISOR_DESCENDANT_CASE";
const READY_PATH_ENV: &str = "MEMPAL_SUPERVISOR_READY_PATH";
const FIXTURE_TEST: &str = "admission_supervisor::regression_tests::supervisor_fixture";
const DESCENDANT_TEST: &str =
    "admission_supervisor::regression_tests::supervisor_descendant_fixture";
const SUSTAINED_STDOUT_WRITER_COUNT: usize = 8;

fn current_test_spec(case: &str) -> SpawnSpec {
    let executable = std::env::current_exe().expect("current test executable");
    let mut spec = SpawnSpec::new(executable).expect("absolute test executable");
    spec.args(["--exact", FIXTURE_TEST, "--nocapture", "--test-threads=1"])
        .env(FIXTURE_CASE_ENV, case);
    spec
}

fn fixture_command(case: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().expect("current test executable"));
    command
        .args([
            "--exact",
            DESCENDANT_TEST,
            "--nocapture",
            "--test-threads=1",
        ])
        .env_remove(FIXTURE_CASE_ENV)
        .env(DESCENDANT_CASE_ENV, case);
    command
}

fn append_identity(path: &Path, identity: ProcessIdentity, ready: bool) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open fixture handshake");
    writeln!(
        file,
        "PID {} {}{}",
        identity.pid,
        identity.start_time_ticks.expect("known fixture start time"),
        if ready { " READY" } else { "" }
    )
    .expect("write fixture identity");
    file.flush().expect("flush fixture identity");
}

fn wait_for_ready_file(path: &Path) -> Vec<ProcessIdentity> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Ok(contents) = std::fs::read_to_string(path)
            && contents.lines().any(|line| line.ends_with(" READY"))
        {
            return parse_identities(contents.as_bytes());
        }
        assert!(Instant::now() < deadline, "fixture handshake timed out");
        std::thread::yield_now();
    }
}

fn parse_identities(bytes: &[u8]) -> Vec<ProcessIdentity> {
    String::from_utf8_lossy(bytes)
        .lines()
        .filter_map(|line| {
            let fields = line.split_whitespace().collect::<Vec<_>>();
            let marker = fields.iter().position(|field| *field == "PID")?;
            Some(ProcessIdentity {
                pid: fields.get(marker + 1)?.parse().ok()?,
                start_time_ticks: Some(fields.get(marker + 2)?.parse().ok()?),
            })
        })
        .collect()
}

fn ignore_term_and_pause() -> ! {
    // SAFETY: this is an exec'd, test-only fixture; it changes only its own SIGTERM disposition
    // and then blocks in pause until the supervisor's process-group SIGKILL terminates it.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        loop {
            libc::pause();
        }
    }
}

fn ignore_term_and_emit_sustained_output(write_stdout: bool, write_stderr: bool) -> ! {
    let stdout = [b'o'; 16 * 1024];
    let stderr = [b'e'; 16 * 1024];
    // SAFETY: this exec'd fixture owns its signal disposition; both byte arrays remain live and
    // initialized for each selected write, and STDOUT/STDERR are inherited capture-pipe FDs.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
        loop {
            if write_stdout {
                libc::write(libc::STDOUT_FILENO, stdout.as_ptr().cast(), stdout.len());
            }
            if write_stderr {
                libc::write(libc::STDERR_FILENO, stderr.as_ptr().cast(), stderr.len());
            }
        }
    }
}

#[test]
fn supervisor_fixture() {
    let Ok(case) = std::env::var(FIXTURE_CASE_ENV) else {
        return;
    };
    match case.as_str() {
        "exit-42" => {
            // SAFETY: this isolated fixture must terminate immediately with its asserted status
            // without running inherited test-harness destructors.
            unsafe { libc::_exit(42) }
        }
        "pipe-descendant" | "silent-descendant" => {
            let mut command = fixture_command("pause");
            if case == "silent-descendant" {
                command.stdout(Stdio::null()).stderr(Stdio::null());
            }
            let child = command.spawn().expect("spawn fixture descendant");
            let identity = process_identity(child.id() as libc::pid_t);
            println!(
                "PID {} {} READY",
                identity.pid,
                identity.start_time_ticks.expect("known start time")
            );
            std::io::stdout()
                .flush()
                .expect("flush descendant identity");
            std::mem::forget(child);
            // SAFETY: the fixture intentionally leaves the descendant in the owned process
            // group, then exits without running test-harness cleanup so pipe inheritance persists.
            unsafe { libc::_exit(0) }
        }
        "term-resistant" => {
            let ready = PathBuf::from(std::env::var_os(READY_PATH_ENV).expect("ready path"));
            // SAFETY: this exec'd fixture changes only its own SIGTERM disposition to exercise
            // the supervisor's mandatory KILL escalation.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            append_identity(
                &ready,
                process_identity(std::process::id() as libc::pid_t),
                true,
            );
            ignore_term_and_pause();
        }
        "term-resistant-tree" => {
            let ready = PathBuf::from(std::env::var_os(READY_PATH_ENV).expect("ready path"));
            // SAFETY: this exec'd fixture changes only its own SIGTERM disposition to exercise
            // KILL fencing of the entire owned descendant group.
            unsafe {
                libc::signal(libc::SIGTERM, libc::SIG_IGN);
            }
            append_identity(
                &ready,
                process_identity(std::process::id() as libc::pid_t),
                false,
            );
            let mut command = fixture_command("tree-root");
            command.env(READY_PATH_ENV, &ready);
            let child = command.spawn().expect("spawn tree root");
            std::mem::forget(child);
            ignore_term_and_pause();
        }
        "sustained-output-tree" => {
            let ready = PathBuf::from(std::env::var_os(READY_PATH_ENV).expect("ready path"));
            append_identity(
                &ready,
                process_identity(std::process::id() as libc::pid_t),
                false,
            );
            for _ in 0..SUSTAINED_STDOUT_WRITER_COUNT {
                let mut command = fixture_command("sustained-stdout");
                command.env(READY_PATH_ENV, &ready);
                let child = command.spawn().expect("spawn sustained stdout descendant");
                append_identity(&ready, process_identity(child.id() as libc::pid_t), true);
                std::mem::forget(child);
            }
            let mut command = fixture_command("sustained-stderr");
            command.env(READY_PATH_ENV, &ready);
            let child = command.spawn().expect("spawn sustained stderr descendant");
            append_identity(&ready, process_identity(child.id() as libc::pid_t), true);
            std::mem::forget(child);
            ignore_term_and_emit_sustained_output(true, false);
        }
        other => panic!("unknown supervisor fixture case {other}"),
    }
}

#[test]
fn supervisor_descendant_fixture() {
    let Ok(case) = std::env::var(DESCENDANT_CASE_ENV) else {
        return;
    };
    // SAFETY: this exec'd descendant changes only its own SIGTERM disposition, allowing the
    // parent test to verify that the supervisor eventually uses a process-group SIGKILL.
    unsafe {
        libc::signal(libc::SIGTERM, libc::SIG_IGN);
    }
    match case.as_str() {
        "pause" | "leaf" => ignore_term_and_pause(),
        "tree-root" => {
            let ready = PathBuf::from(std::env::var_os(READY_PATH_ENV).expect("ready path"));
            append_identity(
                &ready,
                process_identity(std::process::id() as libc::pid_t),
                false,
            );
            let mut command = fixture_command("leaf");
            command.env(READY_PATH_ENV, &ready);
            let child = command.spawn().expect("spawn tree leaf");
            append_identity(&ready, process_identity(child.id() as libc::pid_t), true);
            std::mem::forget(child);
            ignore_term_and_pause();
        }
        "sustained-stdout" => ignore_term_and_emit_sustained_output(true, false),
        "sustained-stderr" => ignore_term_and_emit_sustained_output(false, true),
        other => panic!("unknown descendant fixture case {other}"),
    }
}

#[test]
fn blocked_setup_is_owned_after_group_ready_and_reaped_before_return() {
    let (gate, child_gate) = TestSetupGate::new().expect("create setup gate");
    let mut spec = SpawnSpec::new("/bin/true").expect("absolute true executable");
    spec.setup_gate(child_gate);

    std::thread::scope(|scope| {
        let worker = scope.spawn(move || DeadlineChild::output(spec, Duration::from_millis(500)));
        let ready_pid = gate
            .wait_ready(Instant::now() + Duration::from_secs(1))
            .expect("child reached post-setpgid gate");
        let output = worker
            .join()
            .expect("owned setup worker")
            .expect("bounded setup output");

        assert_eq!(output.identity.pid, ready_pid);
        assert!(output.timed_out);
        assert!(output.cleanup.kill_fence_sent);
        assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
        ExactProcessGuard::new(output.identity).assert_gone("blocked setup child");
        let mut status = 0;
        // SAFETY: output returned only after the supervisor claimed to reap its direct child;
        // this WNOHANG query uses valid status storage to verify ECHILD rather than reaping it.
        assert_eq!(
            unsafe { libc::waitpid(ready_pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD),
            "direct child must already be reaped"
        );
    });
}

#[test]
fn setup_gate_release_completes_the_same_owned_launch() {
    let (gate, child_gate) = TestSetupGate::new().expect("create setup gate");
    let mut spec = SpawnSpec::new("/bin/true").expect("absolute true executable");
    spec.setup_gate(child_gate);

    std::thread::scope(|scope| {
        let worker = scope.spawn(move || DeadlineChild::output(spec, Duration::from_secs(2)));
        gate.wait_ready(Instant::now() + Duration::from_secs(1))
            .expect("child reached setup gate");
        gate.release().expect("release setup gate");
        let output = worker
            .join()
            .expect("owned setup worker")
            .expect("released setup output");
        assert!(output.success());
        assert!(!output.timed_out);
    });
}

#[test]
fn exit_diagnostic_preserves_unreaped_group_anchor() {
    let mut child = DeadlineChild::spawn(current_test_spec("exit-42"), Duration::from_secs(2))
        .expect("spawn exit fixture");
    let identity = child.identity();
    let diagnostic = child
        .wait_for_exit_diagnostic(Duration::from_secs(2))
        .expect("observe exit without reaping");

    assert!(diagnostic.contains("remains the group anchor"));
    assert_eq!(
        child.resources().leader,
        LeaderResourceState::ExitedUnreaped
    );
    assert_eq!(child.resources().group, GroupFenceState::Unfenced);
    assert!(
        identity.still_refers_to_original_process(),
        "zombie anchor vanished"
    );

    let cleanup = child.force_kill().expect_complete("finish anchored child");
    assert!(cleanup.kill_fence_sent);
    assert!(
        child
            .exit_diagnostic()
            .expect("completed diagnostic")
            .contains("42")
    );
    ExactProcessGuard::new(identity).assert_gone("diagnostic anchor");
}

#[test]
fn exited_leader_retains_group_authority_until_inherited_pipes_close() {
    let output =
        DeadlineChild::output(current_test_spec("pipe-descendant"), Duration::from_secs(3))
            .expect("run inherited-pipe fixture");
    let descendants = parse_identities(&output.stdout);

    assert!(output.success(), "leader status changed: {output:?}");
    assert!(!output.timed_out);
    assert!(output.cleanup.kill_fence_sent);
    assert!(!descendants.is_empty(), "missing descendant identity");
    assert_eq!(output.stdout_omitted_bytes, 0);
    assert_eq!(output.stderr_omitted_bytes, 0);
    assert_eq!(output.stdout_diagnostic(), output.stdout);
    assert_eq!(output.stderr_diagnostic(), output.stderr);
    for identity in descendants {
        ExactProcessGuard::new(identity).assert_gone("inherited-pipe descendant");
    }
}

#[test]
fn exited_leader_still_fences_silent_descendant() {
    let output = DeadlineChild::output(
        current_test_spec("silent-descendant"),
        Duration::from_secs(3),
    )
    .expect("run silent-descendant fixture");
    let descendants = parse_identities(&output.stdout);

    assert!(output.success(), "leader status changed: {output:?}");
    assert!(output.cleanup.kill_fence_sent);
    assert_eq!(descendants.len(), 1, "unexpected fixture identities");
    ExactProcessGuard::new(descendants[0]).assert_gone("silent descendant");
}

#[test]
fn term_grace_expiry_is_an_explicit_kill_escalation() {
    let temp = tempfile::tempdir().expect("temp dir");
    let ready = temp.path().join("ready");
    let mut spec = current_test_spec("term-resistant");
    spec.env(READY_PATH_ENV, &ready);
    let mut child =
        DeadlineChild::spawn(spec, Duration::from_secs(2)).expect("spawn TERM-resistant fixture");
    let identities = wait_for_ready_file(&ready);

    let cleanup = child
        .terminate(Duration::from_secs(2))
        .expect_complete("terminate resistant fixture");
    assert!(cleanup.term_grace_expired);
    assert!(cleanup.kill_fence_sent);
    assert!(cleanup.errors.is_empty(), "{cleanup:#?}");
    for identity in identities {
        ExactProcessGuard::new(identity).assert_gone("TERM-resistant fixture");
    }
}

#[test]
fn term_resistant_process_tree_reaches_kill_and_disappears() {
    let temp = tempfile::tempdir().expect("temp dir");
    let ready = temp.path().join("tree-ready");
    let mut spec = current_test_spec("term-resistant-tree");
    spec.env(READY_PATH_ENV, &ready);
    let mut child = DeadlineChild::spawn(spec, Duration::from_secs(2))
        .expect("spawn TERM-resistant process tree");
    let identities = wait_for_ready_file(&ready);
    assert!(
        identities.len() >= 3,
        "expected leader, child, and grandchild"
    );

    let cleanup = child
        .terminate(Duration::from_secs(2))
        .expect_complete("terminate resistant tree");
    assert!(cleanup.term_grace_expired);
    assert!(cleanup.kill_fence_sent);
    for identity in identities {
        ExactProcessGuard::new(identity).assert_gone("TERM-resistant process tree");
    }
}

#[test]
fn sustained_output_obeys_deadline_and_reaps_the_owned_process_group() {
    let temp = tempfile::tempdir().expect("temp dir");
    let ready = temp.path().join("sustained-output-ready");
    let mut spec = current_test_spec("sustained-output-tree");
    spec.env(READY_PATH_ENV, &ready);
    let timeout = Duration::from_secs(2);
    let started = Instant::now();
    let output = DeadlineChild::output(spec, timeout).expect("supervise sustained output");
    let elapsed = started.elapsed();
    let identities = wait_for_ready_file(&ready);

    assert!(output.timed_out, "sustained output must hit the deadline");
    assert!(
        elapsed <= timeout,
        "supervisor exceeded its configured deadline: {elapsed:?} > {timeout:?}"
    );
    assert!(output.cleanup.kill_fence_sent);
    assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
    assert!(
        output.stdout_total_bytes > 0,
        "stdout received no bounded service"
    );
    assert!(
        output.stderr_total_bytes > 0,
        "stderr was starved by stdout"
    );
    assert_eq!(
        identities.len(),
        SUSTAINED_STDOUT_WRITER_COUNT + 2,
        "expected leader plus stdout and stderr writer identities"
    );
    for identity in identities {
        ExactProcessGuard::new(identity).assert_gone("sustained-output process group member");
    }

    let mut status = 0;
    // SAFETY: output returned only after the supervisor claimed to reap its direct child; this
    // WNOHANG query uses valid status storage to verify ECHILD rather than reaping it.
    assert_eq!(
        unsafe { libc::waitpid(output.identity.pid, &mut status, libc::WNOHANG) },
        -1,
        "the direct child must be reaped before output returns"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}

#[test]
fn expired_cleanup_budget_cannot_report_success_or_drop_ownership() {
    let _retry_api = IncompleteCleanup::finish;
    let temp = tempfile::tempdir().expect("temp dir");
    let ready = temp.path().join("ready");
    let mut spec = current_test_spec("term-resistant");
    spec.env(READY_PATH_ENV, &ready);
    let mut child =
        DeadlineChild::spawn(spec, Duration::from_secs(2)).expect("spawn cleanup-state fixture");
    let identities = wait_for_ready_file(&ready);

    let progress = child.force_kill_with_timeout(Duration::ZERO);
    let resources = match progress {
        CleanupProgress::Incomplete { resources, .. } => resources,
        CleanupProgress::Complete(report) => {
            panic!("expired cleanup budget reported completion: {report:?}")
        }
    };
    assert_eq!(resources.group, GroupFenceState::KillFenceSent);
    assert_ne!(resources.leader, LeaderResourceState::Reaped);
    assert!(resources.stdout_pipe_open || resources.stderr_pipe_open);

    child
        .force_kill()
        .expect_complete("finish retained cleanup ownership");
    for identity in identities {
        ExactProcessGuard::new(identity).assert_gone("retained cleanup fixture");
    }
}

#[test]
fn capture_boundaries_are_lossless_and_chunking_invariant() {
    for size in [
        512 * 1024 - 1,
        512 * 1024 + 1,
        CAPTURE_LIMIT_BYTES,
        CAPTURE_LIMIT_BYTES + 1,
    ] {
        let input = (0..size)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let captures = [
            capture_with_chunks(&input, &[input.len().max(1)]),
            capture_with_chunks(&input, &[1]),
            capture_with_chunks(&input, &[512 * 1024 - 3, 7, 8191]),
        ];
        for captured in &captures {
            assert_eq!(captured.total_bytes, size);
            assert_eq!(
                captured.omitted_bytes,
                size.saturating_sub(CAPTURE_LIMIT_BYTES)
            );
            if size <= CAPTURE_LIMIT_BYTES {
                assert_eq!(captured.bytes, input, "{size} bytes must be lossless");
            } else {
                assert_eq!(&captured.bytes[..512 * 1024], &input[..512 * 1024]);
                assert_eq!(&captured.bytes[512 * 1024..], &input[size - 512 * 1024..]);
            }
        }
        assert!(
            captures
                .windows(2)
                .all(|pair| pair[0].bytes == pair[1].bytes)
        );
    }
}

#[test]
fn diagnostic_marker_is_accounted_inside_the_hard_capture_cap() {
    let size = CAPTURE_LIMIT_BYTES + 1;
    let input = vec![b'x'; size];
    let mut capture = BoundedCapture::new();
    capture.append(&input);
    let captured = capture.finish();
    let diagnostic = render_diagnostic(&captured.bytes, captured.omitted_bytes);

    assert_eq!(captured.bytes.len(), CAPTURE_LIMIT_BYTES);
    assert_eq!(captured.omitted_bytes, 1);
    assert!(diagnostic.len() <= CAPTURE_LIMIT_BYTES);
    assert_eq!(
        diagnostic
            .windows(b"bytes omitted".len())
            .filter(|window| *window == b"bytes omitted")
            .count(),
        1
    );
}

fn capture_with_chunks(input: &[u8], chunk_sizes: &[usize]) -> CapturedBytes {
    let mut capture = BoundedCapture::new();
    let mut offset = 0usize;
    let mut chunk_index = 0usize;
    while offset < input.len() {
        let chunk_size = chunk_sizes[chunk_index % chunk_sizes.len()].max(1);
        let end = (offset + chunk_size).min(input.len());
        capture.append(&input[offset..end]);
        offset = end;
        chunk_index += 1;
    }
    capture.finish()
}
