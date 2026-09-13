#[path = "../../src/core/db_admission_test_process.rs"]
mod process;
const _: fn() = process::reference_shared_test_api;

use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use process::{CleanupProgress, DeadlineOutput, SpawnSpec, SupervisionError};
pub(super) use process::{DeadlineChild, LeaderResourceState, StdioMode};

const CLEANUP_BUDGET: Duration = Duration::from_secs(5);

pub(super) fn spawn_cli(
    home: &Path,
    args: &[&str],
    deadline: Instant,
    stdio: StdioMode,
    role: &'static str,
) -> DeadlineChild {
    let mut spec = SpawnSpec::new(super::mempal_bin()).expect("absolute mempal binary");
    for arg in args {
        spec.arg(*arg);
    }
    spec.env("HOME", home.as_os_str()).stdio(stdio);
    DeadlineChild::spawn(spec, deadline.saturating_duration_since(Instant::now()))
        .unwrap_or_else(|error| panic_supervision(role, error))
}

pub(super) fn wait_output(
    child: DeadlineChild,
    deadline: Instant,
    started: Instant,
    role: &'static str,
) -> Output {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        cleanup_and_panic(child, started, role);
    }
    match child.wait_output(remaining) {
        Ok(output) if !output.timed_out => checked_output(output, role),
        Ok(output) => panic!(
            "{role} timed out after {:?}; kill_fence={} cleanup_errors={}; content omitted",
            started.elapsed(),
            output.cleanup.kill_fence_sent,
            output.cleanup.errors.len()
        ),
        Err(error) => panic_supervision(role, error),
    }
}

fn checked_output(output: DeadlineOutput, role: &str) -> Output {
    assert!(
        !output.stdout_truncated && !output.stderr_truncated,
        "{role} output truncated: stdout_total={} stderr_total={}",
        output.stdout_total_bytes,
        output.stderr_total_bytes
    );
    Output {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    }
}

pub(super) fn cleanup_and_panic(mut child: DeadlineChild, started: Instant, role: &str) -> ! {
    match child.terminate(CLEANUP_BUDGET) {
        CleanupProgress::Complete(report) => panic!(
            "{role} deadline elapsed after {:?}; kill_fence={} cleanup_errors={}; content omitted",
            started.elapsed(),
            report.kill_fence_sent,
            report.errors.len()
        ),
        CleanupProgress::Incomplete { report, resources } => panic!(
            "{role} cleanup incomplete after {:?}: resources={resources:?} kill_fence={} cleanup_errors={}; content omitted",
            started.elapsed(),
            report.kill_fence_sent,
            report.errors.len()
        ),
    }
}

fn panic_supervision(role: &str, error: SupervisionError) -> ! {
    match error {
        SupervisionError::CleanupIncomplete(cleanup) => panic!(
            "{role} supervision cleanup incomplete: resources={:?} kill_fence={} cleanup_errors={}; content omitted",
            cleanup.resources,
            cleanup.report.kill_fence_sent,
            cleanup.report.errors.len()
        ),
        error => panic!("{role} supervision failed: {error}"),
    }
}

#[test]
fn early_exit_with_inherited_pipe_is_collected_without_waiting_for_descendant() {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(5);
    let mut spec = SpawnSpec::new("/bin/sh").expect("absolute shell");
    spec.args(["-c", "(sleep 2) & exit 0"]);
    let child = DeadlineChild::spawn(spec, deadline.saturating_duration_since(Instant::now()))
        .expect("spawn early-exit pipe fixture");
    let output = wait_output(child, deadline, started, "early-exit pipe fixture");
    assert!(output.status.success());
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "an exited leader must retain its group anchor only through bounded descendant cleanup"
    );
}

#[test]
fn timeout_with_inherited_pipe_has_bounded_private_cleanup() {
    const SECRET: &str = "inherited-pipe-fixture-secret";
    const PAYLOAD: &str = "fixture-payload-secret";
    let private = tempfile::tempdir().expect("private fixture dir");
    let pid_file = private.path().join("pids");
    let started = Instant::now();
    let deadline = started + Duration::from_millis(500);
    let mut spec = SpawnSpec::new("/bin/sh").expect("absolute shell");
    spec.args([
        "-c",
        "trap '' TERM; (trap '' TERM; while :; do :; done) & descendant=$!; printf '%s %s\\n' \"$$\" \"$descendant\" > \"$PID_FILE\"; printf '%s\\n' \"$SECRET\"; while :; do :; done",
    ])
    .env("PID_FILE", pid_file.as_os_str())
    .env("SECRET", SECRET)
    .env("PAYLOAD", PAYLOAD)
    .env("HOME", private.path().as_os_str());
    let child = DeadlineChild::spawn(spec, deadline.saturating_duration_since(Instant::now()))
        .expect("spawn timeout pipe fixture");
    while std::fs::metadata(&pid_file).map_or(true, |metadata| metadata.len() == 0)
        && Instant::now() < deadline
    {
        std::thread::yield_now();
    }
    let pids = std::fs::read_to_string(&pid_file).expect("fixture identities");
    let pids: Vec<i32> = pids
        .split_whitespace()
        .map(|pid| pid.parse().expect("numeric fixture pid"))
        .collect();
    assert_eq!(pids.len(), 2);
    let identities: Vec<process::ProcessIdentity> = pids
        .iter()
        .map(|pid| process::ProcessIdentity {
            pid: *pid,
            start_time_ticks: Some(process_start_time(*pid)),
        })
        .collect();
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        wait_output(child, deadline, started, "timeout pipe fixture");
    }))
    .expect_err("timeout must report failure after cleanup");
    let diagnostic = panic
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| panic.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic");
    assert!(identities.iter().all(|identity| {
        !mempal::process_is_live(identity.pid) || !identity.still_refers_to_original_process()
    }));
    let mut status = 0;
    assert_eq!(
        unsafe { libc::waitpid(pids[0], &raw mut status, libc::WNOHANG) },
        -1
    );
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
    assert!(
        started.elapsed() < Duration::from_secs(2)
            && [
                SECRET,
                PAYLOAD,
                private.path().to_str().expect("utf-8 path"),
                "while :;"
            ]
            .iter()
            .all(|secret| !diagnostic.contains(secret)),
        "timeout cleanup must reap exact identities and omit private content: {diagnostic}"
    );
}

fn process_start_time(pid: libc::pid_t) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).expect("process stat");
    stat.rsplit_once(") ")
        .expect("process stat fields")
        .1
        .split_whitespace()
        .nth(19)
        .expect("process start time")
        .parse()
        .expect("numeric process start time")
}
