use super::*;

/// A process identity recorded by a shell fixture before the test later cleans it up.
///
/// The start time makes a recycled numeric PID distinguishable from the original process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RecordedProcessIdentity {
    pub(crate) pid: i32,
    pub(crate) start_time_ticks: u64,
}

/// A pidfd-backed fixture process that can never signal a reused PID.
pub(crate) struct RecordedProcess {
    process: ProcessHandle,
}

impl RecordedProcess {
    pub(crate) fn is_running(&self) -> io::Result<bool> {
        self.process.is_running()
    }

    pub(crate) fn send_signal(&self, signal: i32) -> io::Result<()> {
        self.process.send_signal(signal)
    }
}

pub(crate) fn capture_recorded_process(
    expected: RecordedProcessIdentity,
) -> io::Result<Option<RecordedProcess>> {
    let Some(process) = ProcessHandle::capture(expected.pid)? else {
        return Ok(None);
    };
    if process.identity.start_time_ticks != expected.start_time_ticks {
        return Ok(None);
    }
    Ok(Some(RecordedProcess { process }))
}
