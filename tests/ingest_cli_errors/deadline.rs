//! Bounded-subprocess regression fixtures for #795.

use super::cli_deadline::{
    HANGING_FIXTURE_DEADLINE, HANGING_FIXTURE_RETURN_BOUND, hanging_shell_ignoring_stdin,
    hanging_shell_with_pipe_descendant, run_spec_output, run_spec_output_strict,
    run_spec_stdin_output_strict,
};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::time::{Duration, Instant};

/// Floor proving helpers start `Instant::now()` *before* the supervisor call.
///
/// The supervisor reserves cleanup headroom (`CLEANUP_RESERVE` / half timeout), so
/// wall time is below the full deadline but far above a post-return Instant bug
/// (which observed ~0).
const MIN_OBSERVED_DEADLINE_WAIT: Duration = Duration::from_millis(250);

#[cfg(target_os = "linux")]
#[test]
fn deadline_helper_bounds_non_exiting_fixture_with_pipe_descendant() {
    let started = Instant::now();
    let output = run_spec_output(
        "hanging pipe-descendant fixture",
        hanging_shell_with_pipe_descendant(),
        HANGING_FIXTURE_DEADLINE,
    );
    let elapsed = started.elapsed();

    assert!(
        elapsed < HANGING_FIXTURE_RETURN_BOUND,
        "helper must return within non-flaky bound, elapsed={elapsed:?}"
    );
    assert!(
        elapsed >= MIN_OBSERVED_DEADLINE_WAIT,
        "helper must wait the collection budget (not Instant::now after return), elapsed={elapsed:?}"
    );
    assert!(
        output.timed_out,
        "intentionally non-exiting fixture must report timed_out"
    );
    assert!(
        output.cleanup.kill_fence_sent,
        "cleanup must send the process-group kill fence"
    );
    assert!(
        output.cleanup.errors.is_empty(),
        "cleanup errors: {:?}",
        output.cleanup.errors
    );
    assert!(
        output.stdout.starts_with(b"hang-fixture-ready"),
        "bounded capture must retain readiness marker: {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        !output.identity.still_refers_to_original_process(),
        "leader identity must not remain live after reaping"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn deadline_helper_timeout_message_is_role_and_elapsed_only() {
    let role = "cli-timeout-role-probe";
    let started = Instant::now();
    let hang = catch_unwind(AssertUnwindSafe(|| {
        // Goes through collect_deadline_output: timed_out → panic(role + elapsed).
        let _ = run_spec_output_strict(
            role,
            hanging_shell_with_pipe_descendant(),
            HANGING_FIXTURE_DEADLINE,
        );
    }));
    let elapsed = started.elapsed();
    assert!(
        elapsed < HANGING_FIXTURE_RETURN_BOUND,
        "timeout probe returned too slowly: {elapsed:?}"
    );
    assert!(
        elapsed >= MIN_OBSERVED_DEADLINE_WAIT,
        "timeout panic must report a real wait (started before helper call), elapsed={elapsed:?}"
    );
    let payload = hang.expect_err("hanging fixture must panic on timeout");
    let message = payload
        .downcast_ref::<String>()
        .map(|s| s.as_str())
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("timeout panic payload must be a string");
    assert!(
        message.contains(role),
        "timeout message must include command role: {message}"
    );
    assert!(
        message.contains("exceeded deadline"),
        "timeout message must include deadline context: {message}"
    );
    assert!(
        !message.contains("hang-fixture-ready"),
        "timeout message must not leak pipe content: {message}"
    );
    assert!(
        !message.contains("HOME="),
        "timeout message must not leak environment: {message}"
    );
    assert!(
        !message.contains("sk-"),
        "timeout message must not look like a credential leak: {message}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cleanup_incomplete_panics_without_hard_exit() {
    let role = "cli-cleanup-incomplete";
    let result = catch_unwind(AssertUnwindSafe(|| {
        let _ = run_spec_output_strict(role, hanging_shell_with_pipe_descendant(), Duration::ZERO);
    }));

    let payload = result.expect_err("an expired cleanup deadline must panic");
    let message = payload
        .downcast_ref::<String>()
        .map(|message| message.as_str())
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("cleanup panic payload must be a string");
    assert!(
        message.contains(role),
        "cleanup message must include command role: {message}"
    );
    assert!(
        message.contains("cleanup incomplete"),
        "cleanup message must identify the incomplete cleanup: {message}"
    );
    assert!(
        !message.contains("hang-fixture-ready"),
        "cleanup message must not leak pipe content: {message}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn stdin_write_timeout_cleans_up_before_panic_without_hard_exit() {
    // Child never reads stdin: a payload larger than the pipe buffer times out
    // the write. The helper must force-kill and reap before panicking so
    // DeadlineChild::Drop only needs its bounded fallback reap during unwinding.
    let role = "cli-stdin-write-timeout";
    let write_deadline = Duration::from_millis(200);
    // Linux pipe capacity is typically 64 KiB; use several megabytes so the
    // write blocks and hits write_budget without relying on BrokenPipe.
    let payload = vec![0u8; 4 * 1024 * 1024];
    let started = Instant::now();
    let hang = catch_unwind(AssertUnwindSafe(|| {
        let _ = run_spec_stdin_output_strict(
            role,
            hanging_shell_ignoring_stdin(),
            &payload,
            write_deadline,
        );
    }));
    let elapsed = started.elapsed();
    assert!(
        elapsed < HANGING_FIXTURE_RETURN_BOUND,
        "stdin write-timeout probe returned too slowly: {elapsed:?}"
    );
    assert!(
        elapsed >= write_deadline,
        "stdin write path must wait the write budget before panicking, elapsed={elapsed:?}"
    );
    let payload = hang.expect_err("stdin write timeout must panic after cleanup");
    let message = payload
        .downcast_ref::<String>()
        .map(|s| s.as_str())
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .expect("stdin write panic payload must be a string");
    assert!(
        message.contains(role),
        "stdin write message must include command role: {message}"
    );
    assert!(
        message.contains("write stdin payload"),
        "stdin write message must name the failure stage: {message}"
    );
    assert!(
        message.contains("cleanup kill_fence=") || message.contains("cleanup incomplete"),
        "stdin write message must prove cleanup ran before panic: {message}"
    );
    assert!(
        !message.contains("stdin-ignore-ready"),
        "stdin write message must not leak pipe content: {message}"
    );
}
