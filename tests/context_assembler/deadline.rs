//! Focused hang/reap regression for #1085.
//!
//! `run_context` now routes through `cli_deadline::run_cli_output`. This module
//! reuses the existing supervisor hang fixture so a non-exiting child is killed
//! and reaped inside the test-helper deadline, without the 1800s REST wrapper.

use super::cli_deadline::{
    HANGING_FIXTURE_DEADLINE, HANGING_FIXTURE_RETURN_BOUND, hanging_shell_with_pipe_descendant,
    run_spec_output,
};
use std::time::{Duration, Instant};

/// Floor proving helpers start `Instant::now()` *before* the supervisor call.
const MIN_OBSERVED_DEADLINE_WAIT: Duration = Duration::from_millis(250);

#[cfg(target_os = "linux")]
#[test]
fn test_context_cli_deadline_reaps_hanging_child() {
    let started = Instant::now();
    let output = run_spec_output(
        "context hanging fixture",
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
        "helper must wait the collection budget, elapsed={elapsed:?}"
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
