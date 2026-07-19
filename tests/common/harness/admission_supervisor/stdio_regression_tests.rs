use std::io::Write;
use std::process::Command;
use std::time::{Duration, Instant};

use super::*;

const CLOSED_STDIO_CASE_ENV: &str = "MEMPAL_SUPERVISOR_CLOSED_STDIO_CASE";
const CLOSED_STDIO_FIXTURE_TEST: &str =
    "admission_supervisor::stdio_regression_tests::closed_parent_stdio_fixture";
const PIPE_HOLDER_CASE_ENV: &str = "MEMPAL_SUPERVISOR_PIPE_HOLDER_CASE";
const PIPE_HOLDER_FIXTURE_TEST: &str =
    "admission_supervisor::stdio_regression_tests::pipe_holder_fixture";

fn fixture_spec(test: &str, environment: (&str, String)) -> SpawnSpec {
    let executable = std::env::current_exe().expect("current test executable");
    let mut spec = SpawnSpec::new(executable).expect("absolute test executable");
    spec.args(["--exact", test, "--nocapture", "--test-threads=1"])
        .env(environment.0, environment.1);
    spec
}

fn assert_reaped(identity: ProcessIdentity, context: &str) {
    ExactProcessGuard::new(identity).assert_gone(context);
    let mut status = 0;
    // SAFETY: the completed DeadlineChild has reaped only its retained direct child; this
    // WNOHANG query checks the expected ECHILD state without observing another process.
    assert_eq!(
        unsafe { libc::waitpid(identity.pid, &mut status, libc::WNOHANG) },
        -1,
        "{context}: direct child was not reaped"
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD),
        "{context}: direct child did not report ECHILD"
    );
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

#[test]
fn closed_parent_stdio_fixture() {
    let Ok(case) = std::env::var(CLOSED_STDIO_CASE_ENV) else {
        return;
    };
    let (closed_fd, mode) = case.split_once(':').expect("closed-stdio case format");
    let closed_fd: libc::c_int = closed_fd.parse().expect("numeric closed fd");
    assert!((libc::STDIN_FILENO..=libc::STDERR_FILENO).contains(&closed_fd));
    // SAFETY: the helper's test harness is fully initialized before it closes one standard FD;
    // the close changes only this isolated process's descriptor table before SpawnSpec allocates
    // any source descriptors.
    assert_eq!(unsafe { libc::close(closed_fd) }, 0);
    // SAFETY: F_GETFD only queries the descriptor table and verifies the close above persisted
    // before the fixture constructs its actual spawn resources.
    assert_eq!(unsafe { libc::fcntl(closed_fd, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF),
        "outer helper did not preserve closed fd {closed_fd} through exec"
    );
    match mode {
        "capture" => capture_fixture_with_closed_parent_stdio(),
        "piped-input" => piped_input_fixture_with_closed_parent_stdio(),
        other => panic!("unknown closed-stdio mode {other}"),
    }
}

#[test]
fn pipe_holder_fixture() {
    let Ok(()) = std::env::var(PIPE_HOLDER_CASE_ENV).map(|_| ()) else {
        return;
    };
    // This fixture is itself the direct child of `DeadlineChild::output`; the descendant remains
    // in that owned process group until the outer supervisor issues its final KILL fence.
    let child = Command::new("/bin/sh")
        .args(["-c", "sleep 1; printf descendant-survived"])
        .spawn()
        .expect("spawn pipe-holding descendant");
    let identity = process_identity(child.id() as libc::pid_t);
    println!(
        "PID {} {} READY",
        identity.pid,
        identity
            .start_time_ticks
            .expect("known descendant start time")
    );
    std::io::stdout()
        .flush()
        .expect("flush pipe-holding descendant identity");
    std::mem::forget(child);
    // SAFETY: this isolated fixture exits without closing its inherited capture descriptors so
    // the descendant retains them until a supervising process-group fence terminates it.
    unsafe { libc::_exit(0) }
}

#[test]
fn setup_handshake_stall_is_fenced_reaped_and_bounded() {
    let (gate, child_gate) = TestSetupGate::new().expect("create setup gate");
    let mut spec = SpawnSpec::new("/bin/true").expect("absolute true executable");
    spec.setup_gate(child_gate);
    let timeout = Duration::from_millis(500);
    let started = Instant::now();

    std::thread::scope(|scope| {
        let worker = scope.spawn(move || DeadlineChild::output(spec, timeout));
        let ready_pid = gate
            .wait_ready(Instant::now() + Duration::from_secs(1))
            .expect("child reached post-setpgid setup gate");
        let output = worker
            .join()
            .expect("owned setup worker")
            .expect("bounded setup output");

        assert!(
            started.elapsed() <= timeout + Duration::from_secs(1),
            "setup cleanup exceeded its single absolute deadline"
        );
        assert!(output.timed_out, "blocked setup must time out");
        assert_eq!(output.identity.pid, ready_pid);
        assert!(output.cleanup.kill_fence_sent);
        assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
        assert_reaped(output.identity, "blocked setup direct child");
    });
}

#[test]
fn exited_leader_with_pipe_holding_descendant_is_fenced_before_reap() {
    let started = Instant::now();
    let timeout = Duration::from_secs(2);
    let output = DeadlineChild::output(
        fixture_spec(
            PIPE_HOLDER_FIXTURE_TEST,
            (PIPE_HOLDER_CASE_ENV, "1".to_owned()),
        ),
        timeout,
    )
    .expect("supervise pipe-holding descendant");
    let descendants = parse_identities(&output.stdout);

    assert!(
        started.elapsed() <= timeout + Duration::from_secs(1),
        "pipe cleanup exceeded its single absolute deadline"
    );
    assert!(
        output.success(),
        "leader status changed before the final fence: {output:?}"
    );
    assert!(!output.timed_out, "leader must exit before the deadline");
    assert!(output.cleanup.kill_fence_sent);
    assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
    assert!(
        !output
            .stdout
            .windows(b"descendant-survived".len())
            .any(|window| window == b"descendant-survived"),
        "descendant survived the final process-group fence"
    );
    assert_eq!(
        descendants.len(),
        1,
        "expected exactly one recorded pipe-holding descendant"
    );
    assert_reaped(output.identity, "pipe-holder direct child");
    for identity in descendants {
        ExactProcessGuard::new(identity).assert_gone("pipe-holding descendant");
    }
}

#[test]
fn closed_parent_stdio_preserves_capture_and_piped_input_channels() {
    for closed_fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
        for mode in ["capture", "piped-input"] {
            let output = DeadlineChild::output(
                fixture_spec(
                    CLOSED_STDIO_FIXTURE_TEST,
                    (CLOSED_STDIO_CASE_ENV, format!("{closed_fd}:{mode}")),
                ),
                Duration::from_secs(5),
            )
            .expect("supervise closed-stdio fixture");
            assert!(
                output.status.success(),
                "closed fd {closed_fd}, {mode}: status={:?}, stdout={}, stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
            assert!(!output.timed_out, "closed fd {closed_fd}, {mode} timed out");
            assert!(output.cleanup.kill_fence_sent);
            assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
            assert_reaped(output.identity, "closed-stdio direct child");
        }
    }
}

fn capture_fixture_with_closed_parent_stdio() {
    let mut spec = SpawnSpec::new("/bin/sh").expect("absolute shell executable");
    spec.args([
        "-c",
        "set -e; cat >/dev/null; printf capture-stdout; printf capture-stderr >&2",
    ]);
    let output = DeadlineChild::output(spec, Duration::from_secs(2)).expect("capture output");
    assert!(output.success(), "capture fixture status: {output:?}");
    assert!(!output.timed_out, "capture fixture timed out");
    assert_eq!(output.stdout, b"capture-stdout");
    assert_eq!(output.stderr, b"capture-stderr");
    assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
}

fn piped_input_fixture_with_closed_parent_stdio() {
    let mut spec = SpawnSpec::new("/bin/sh").expect("absolute shell executable");
    spec.args([
        "-c",
        "set -e; IFS= read -r line; test \"$line\" = piped-input; if IFS= read -r extra; then exit 31; fi; printf hidden-stdout; printf hidden-stderr >&2",
    ])
    .stdio(StdioMode::PipedInput);
    let mut child = DeadlineChild::spawn(spec, Duration::from_secs(2)).expect("spawn piped-input");
    child
        .write_stdin(b"piped-input\n", Duration::from_secs(1))
        .expect("write piped input");
    child.close_stdin();
    let diagnostic = child
        .wait_for_exit_diagnostic(Duration::from_secs(2))
        .expect("observe piped-input exit");
    let cleanup = child.force_kill().expect_complete("reap piped-input child");
    assert!(
        diagnostic.contains("status=0"),
        "piped input or EOF mapping failed: {diagnostic}"
    );
    assert!(cleanup.kill_fence_sent);
    assert!(cleanup.errors.is_empty(), "{cleanup:#?}");
}
