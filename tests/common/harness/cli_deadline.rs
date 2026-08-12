//! Shared test-only CLI subprocess helpers with end-to-end deadlines.
//!
//! Routes integration-test CLI invocations through the Linux-only
//! `DeadlineChild` supervisor (via the same path-include used by admission
//! crash fixtures) so spawn, output capture, pipe drain, termination
//! escalation, and reaping stay bounded. Timeout diagnostics report only the
//! command role and elapsed wall time—never argv content, env, credentials,
//! or pipe bodies.
//!
//! ## Capture policy
//!
//! Successful helpers return a `std::process::Output` whose stdout/stderr are
//! the bounded retained buffers (prefix + tail, limit
//! `CAPTURE_LIMIT_BYTES`). Ordinary CLI diagnostics stay well under that
//! limit; oversized streams keep a tail marker for assertions without unbounded
//! RAM growth.

// Integration tests cannot import mempal::core::db_admission_test_process
// (no longer a library module; shared only via path include).
// Each used supervisor item is imported explicitly below (Rust 011).
#[path = "../../../src/core/db_admission_test_process.rs"]
mod process;

const _: fn() = process::reference_shared_test_api;

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;
use std::process::Output;
use std::time::{Duration, Instant};

use process::{DeadlineChild, DeadlineOutput, SpawnSpec, StdioMode, SupervisionError};

/// References the complete path-included helper API without running a process.
pub fn reference_shared_cli_deadline_api() {
    let _ = CLI_HELPER_DEADLINE;
    let _ = HANGING_FIXTURE_DEADLINE;
    let _ = HANGING_FIXTURE_RETURN_BOUND;
    let _ = hanging_shell_with_pipe_descendant;
    let _ = hanging_shell_ignoring_stdin;
    let _ = run_spec_output;
    let _ = run_spec_output_strict;
    let _ = run_spec_stdin_output_strict;
    let _ = run_cli_stdin_output("api reference", |_: &mut SpawnSpec| {}, &[], Duration::ZERO);
    let _ = panic_after_stdin_cleanup;
    let _ = panic_stdin_write;
}

/// Default upper bound for ordinary mempal CLI integration helpers.
///
/// Includes headroom for cold binary startup under a concurrent suite while
/// remaining well below local-gate hard timeouts.
pub const CLI_HELPER_DEADLINE: Duration = Duration::from_secs(30);

/// Upper bound used by the deliberately hanging fixture (must stay non-flaky).
pub const HANGING_FIXTURE_DEADLINE: Duration = Duration::from_secs(1);

/// Non-flaky wall-clock ceiling proving the helper returns and cleans up.
pub const HANGING_FIXTURE_RETURN_BOUND: Duration = Duration::from_secs(10);

/// Bound for force-kill after a stdin write failure while the child is still live.
const STDIN_WRITE_CLEANUP_BUDGET: Duration = Duration::from_secs(5);

/// Build a supervised spawn for the absolute `mempal` test binary.
pub fn mempal_spec() -> SpawnSpec {
    SpawnSpec::new(env!("CARGO_BIN_EXE_mempal")).expect("absolute mempal test binary")
}

/// Shell fixture that ignores TERM, prints a marker, then parks forever while a
/// descendant inherits the captured stdout pipe (so pipe drain cannot complete
/// without process-group termination).
pub fn hanging_shell_with_pipe_descendant() -> SpawnSpec {
    let mut spec = SpawnSpec::new("/bin/sh").expect("absolute shell");
    spec.args([
        "-c",
        "trap '' TERM; \
         (trap '' TERM; while :; do sleep 60; done) & \
         printf 'hang-fixture-ready\\n'; \
         while :; do sleep 60; done",
    ]);
    spec
}

/// Shell that never reads stdin and parks, so a large write fills the pipe and
/// times out without BrokenPipe. Used to prove stdin-write failure cleans up
/// before panicking.
pub fn hanging_shell_ignoring_stdin() -> SpawnSpec {
    let mut spec = SpawnSpec::new("/bin/sh").expect("absolute shell");
    spec.args([
        "-c",
        "trap '' TERM; \
         printf 'stdin-ignore-ready\\n'; \
         while :; do sleep 60; done",
    ]);
    // Leave default stdio as Capture; callers switch to CaptureWithInput.
    spec
}

/// Capture stdout/stderr for a CLI command under an end-to-end deadline.
///
/// On success, returns a `std::process::Output` compatible with existing
/// assertions. On timeout, panics with the command role and elapsed duration
/// only (no content/credential leakage).
pub fn run_cli_output(
    role: &'static str,
    mut build: impl FnMut(&mut SpawnSpec),
    timeout: Duration,
) -> Output {
    let mut spec = mempal_spec();
    build(&mut spec);
    let started = Instant::now();
    collect_deadline_output(role, DeadlineChild::output(spec, timeout), started)
}

/// Run an arbitrary absolute-path [`SpawnSpec`] under a deadline.
///
/// Returns the full [`DeadlineOutput`] (including `timed_out`) so hang fixtures
/// can assert cleanup metadata. Does **not** panic on timeout.
pub fn run_spec_output(role: &'static str, spec: SpawnSpec, timeout: Duration) -> DeadlineOutput {
    let started = Instant::now();
    match DeadlineChild::output(spec, timeout) {
        Ok(output) => output,
        Err(error) => panic_supervision(role, started.elapsed(), error),
    }
}

/// Like [`run_cli_output`] for an arbitrary [`SpawnSpec`]: panics on timeout
/// with role + elapsed only (used by timeout-message contract tests).
pub fn run_spec_output_strict(role: &'static str, spec: SpawnSpec, timeout: Duration) -> Output {
    let started = Instant::now();
    collect_deadline_output(role, DeadlineChild::output(spec, timeout), started)
}

/// Write a finite stdin payload to an arbitrary [`SpawnSpec`], then collect output.
///
/// On stdin write failure, performs an explicit bounded cleanup **before**
/// panicking. If the cleanup budget expires, [`DeadlineChild`]'s fail-closed
/// `Drop` repeats its KILL fence and bounded reap during unwinding.
pub fn run_spec_stdin_output_strict(
    role: &'static str,
    mut spec: SpawnSpec,
    payload: &[u8],
    timeout: Duration,
) -> Output {
    spec.stdio(StdioMode::CaptureWithInput);
    let started = Instant::now();
    let mut child = match DeadlineChild::spawn(spec, timeout) {
        Ok(child) => child,
        Err(error) => panic_supervision(role, started.elapsed(), error),
    };

    // Bound the stdin write to the remaining work budget; BrokenPipe is fine when the
    // child exits early (matching the previous helper's ErrorKind::BrokenPipe handling).
    let write_budget = timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(50));
    match child.write_stdin(payload, write_budget) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => {}
        Err(error) => panic_after_stdin_cleanup(role, started, child, error),
    }
    child.close_stdin();

    let remaining = timeout
        .saturating_sub(started.elapsed())
        .max(Duration::from_millis(50));
    collect_deadline_output(role, child.wait_output(remaining), started)
}

/// Write a finite stdin payload, close stdin, then collect captured output under a deadline.
pub fn run_cli_stdin_output(
    role: &'static str,
    mut build: impl FnMut(&mut SpawnSpec),
    payload: &[u8],
    timeout: Duration,
) -> Output {
    let mut spec = mempal_spec();
    build(&mut spec);
    run_spec_stdin_output_strict(role, spec, payload, timeout)
}

/// Attach absolute HOME commonly used by CLI fixtures.
pub fn with_home(spec: &mut SpawnSpec, home: &Path) {
    assert!(
        home.is_absolute(),
        "CLI helper HOME must be absolute to satisfy SpawnSpec"
    );
    spec.env("HOME", home.as_os_str());
}

/// Convenience: push stringy argv pieces onto a [`SpawnSpec`].
pub fn push_args<I, S>(spec: &mut SpawnSpec, args: I)
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    for arg in args {
        spec.arg(OsString::from(arg.as_ref()));
    }
}

fn panic_after_stdin_cleanup(
    role: &'static str,
    started: Instant,
    child: DeadlineChild,
    write_error: io::Error,
) -> ! {
    // Ownership is consumed before any panic so cleanup diagnostics remain available.
    match child.force_kill_owned(STDIN_WRITE_CLEANUP_BUDGET) {
        Ok(report) => panic_stdin_write(
            role,
            started.elapsed(),
            write_error,
            report.kill_fence_sent,
            report.errors.len(),
            None,
        ),
        Err(incomplete) => match incomplete.finish(STDIN_WRITE_CLEANUP_BUDGET) {
            Ok(report) => panic_stdin_write(
                role,
                started.elapsed(),
                write_error,
                report.kill_fence_sent,
                report.errors.len(),
                None,
            ),
            Err(still) => {
                // Retain the owner through panic unwinding: its Drop repeats the KILL fence
                // and bounded reap instead of leaking a zombie child.
                let resources = format!("{:?}", still.resources);
                let kill_fence = still.report.kill_fence_sent;
                let error_count = still.report.errors.len();
                panic_stdin_write(
                    role,
                    started.elapsed(),
                    write_error,
                    kill_fence,
                    error_count,
                    Some(resources),
                );
            }
        },
    }
}

fn panic_stdin_write(
    role: &'static str,
    elapsed: Duration,
    write_error: io::Error,
    kill_fence_sent: bool,
    error_count: usize,
    incomplete_resources: Option<String>,
) -> ! {
    match incomplete_resources {
        Some(resources) => panic!(
            "{role}: write stdin payload after {elapsed:?}: {write_error}; \
             cleanup incomplete after kill fence resources={resources} \
             kill_fence={kill_fence_sent} errors={error_count}"
        ),
        None => panic!(
            "{role}: write stdin payload after {elapsed:?}: {write_error}; \
             cleanup kill_fence={kill_fence_sent} errors={error_count}"
        ),
    }
}

fn collect_deadline_output(
    role: &'static str,
    result: Result<DeadlineOutput, SupervisionError>,
    started: Instant,
) -> Output {
    match result {
        Ok(output) if !output.timed_out => deadline_output_to_std(output),
        Ok(output) => panic_timeout(role, started.elapsed(), Some(output)),
        Err(error) => panic_supervision(role, started.elapsed(), error),
    }
}

fn deadline_output_to_std(output: DeadlineOutput) -> Output {
    Output {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    }
}

fn panic_timeout(role: &'static str, elapsed: Duration, output: Option<DeadlineOutput>) -> ! {
    // Deliberately omit stdout/stderr/env/argv content from timeout reports.
    let cleanup = output
        .as_ref()
        .map(|o| {
            format!(
                " timed_out={} kill_fence={} cleanup_errors={}",
                o.timed_out,
                o.cleanup.kill_fence_sent,
                o.cleanup.errors.len()
            )
        })
        .unwrap_or_default();
    panic!(
        "CLI helper `{role}` exceeded deadline after {elapsed:?}{cleanup}; \
         content and credentials are intentionally omitted"
    );
}

fn panic_supervision(role: &'static str, elapsed: Duration, error: SupervisionError) -> ! {
    match error {
        SupervisionError::CleanupIncomplete(cleanup) => {
            panic!(
                "CLI helper `{role}` supervision failed after {elapsed:?}: cleanup incomplete \
                 resources={:?} kill_fence={} term_grace_expired={} \
                 cleanup_errors={} disposition={:?}; content and credentials are intentionally omitted",
                cleanup.resources,
                cleanup.report.kill_fence_sent,
                cleanup.report.term_grace_expired,
                cleanup.report.errors.len(),
                cleanup.report.disposition,
            );
        }
        error => {
            // Display of the remaining supervision errors is content-free
            // (role/stage/identity only).
            panic!("CLI helper `{role}` supervision failed after {elapsed:?}: {error}");
        }
    }
}
