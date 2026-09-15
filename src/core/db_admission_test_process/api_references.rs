//! Compile-time API references for the path-included test supervisor.
//!
//! Each inclusion root takes a pointer to [`reference_shared_test_api`]. This
//! keeps every shared API item reachable to Rust's lint analysis without a
//! suppression or runtime process activity.

use std::io;

use super::{
    BoundedCapture, CAPTURE_LIMIT_BYTES, CapturedBytes, CleanupProgress, CleanupReport,
    DeadlineChild, DeadlineOutput, IncompleteCleanup, ProcessIdentity, SpawnSpec, StdioMode,
    SupervisionError, TestSetupGate, render_diagnostic,
};

/// References the complete supervisor API from every path-inclusion root.
pub fn reference_shared_test_api() {
    let _ = BoundedCapture::new;
    let _ = BoundedCapture::append;
    let _ = BoundedCapture::finish;
    let _ = CAPTURE_LIMIT_BYTES;
    let _ = render_diagnostic;
    let _ = CapturedBytes {
        bytes: Vec::new(),
        total_bytes: 0,
        omitted_bytes: 0,
    };

    let _ = spawn_spec_api_methods as fn(&mut SpawnSpec) -> io::Result<()>;
    let _ = SpawnSpec::setup_gate;
    let _ = StdioMode::Capture;
    let _ = StdioMode::PipedInput;
    let _ = StdioMode::CaptureWithInput;
    let _ = TestSetupGate::new;
    let _ = TestSetupGate::wait_ready;
    let _ = TestSetupGate::release;

    let _ = ProcessIdentity::still_refers_to_original_process;
    let _ = CleanupProgress::expect_complete;
    let _ = IncompleteCleanup::finish;
    let _ = IncompleteCleanup::finish_output;
    let _ = DeadlineChild::spawn;
    let _ = DeadlineChild::output;
    let _ = DeadlineChild::identity;
    let _ = DeadlineChild::resources;
    let _ = DeadlineChild::exit_diagnostic;
    let _ = DeadlineChild::wait_for_exit_diagnostic;
    let _ = DeadlineChild::write_stdin;
    let _ = DeadlineChild::force_kill;
    let _ = DeadlineChild::force_kill_with_timeout;
    let _ = DeadlineChild::terminate;
    let _ = DeadlineChild::close_stdin;
    let _ = DeadlineChild::wait_output;
    let _ = DeadlineChild::force_kill_owned;
    let _ = DeadlineOutput::success;
    let _ = DeadlineOutput::stdout_diagnostic;
    let _ = DeadlineOutput::stderr_diagnostic;
    let _ = deadline_output_fields as fn(&DeadlineOutput);
    let _ = SupervisionError::SetupTimedOut {
        identity: ProcessIdentity {
            pid: 0,
            start_time_ticks: None,
        },
        cleanup: CleanupReport::new(),
    };
}

fn deadline_output_fields(output: &DeadlineOutput) {
    let _ = (
        output.identity,
        output.status,
        &output.stdout,
        &output.stderr,
        output.timed_out,
        output.stdout_total_bytes,
        output.stderr_total_bytes,
        output.stdout_omitted_bytes,
        output.stderr_omitted_bytes,
        output.stdout_truncated,
        output.stderr_truncated,
        &output.cleanup,
    );
}

fn spawn_spec_api_methods(spec: &mut SpawnSpec) -> io::Result<()> {
    let _ = SpawnSpec::new(std::path::PathBuf::from("/"));
    let _ = SpawnSpec::resolve(std::ffi::OsString::new());
    let _ = spec.arg(std::ffi::OsString::new());
    let _ = spec.args(Vec::<std::ffi::OsString>::new());
    let _ = spec.env(std::ffi::OsString::new(), std::ffi::OsString::new());
    let _ = spec.env_clear();
    let _ = spec.current_dir(std::path::PathBuf::from("/"))?;
    let _ = spec.stdio(StdioMode::Capture);
    Ok(())
}
