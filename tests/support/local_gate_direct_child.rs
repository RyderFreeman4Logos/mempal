use super::*;

/// Owns a direct test child from spawn until it has been reaped.
///
/// This is deliberately established before any fallible `/proc` or pidfd work.
/// Dropping a `std::process::Child` alone neither terminates nor reaps it.
pub(crate) struct OwnedGateChild {
    child: Child,
    first_cleanup_error: Option<io::Error>,
}

impl OwnedGateChild {
    pub(crate) fn new(child: Child) -> Self {
        Self {
            child,
            first_cleanup_error: None,
        }
    }

    pub(super) fn child(&self) -> &Child {
        &self.child
    }

    pub(super) fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    fn record_cleanup_error(&mut self, error: io::Error) {
        if self.first_cleanup_error.is_none() {
            self.first_cleanup_error = Some(error);
        }
    }

    pub(super) fn take_cleanup_error(&mut self) -> Option<io::Error> {
        self.first_cleanup_error.take()
    }

    pub(super) fn ensure_direct_child_cleanup(&mut self) {
        let mut first_error = None;
        let mut record = |error: io::Error| {
            if first_error.is_none() {
                first_error = Some(error);
            }
        };

        let already_reaped = match self.child.try_wait() {
            Ok(Some(_)) => true,
            Ok(None) => false,
            Err(error) => {
                record(error);
                false
            }
        };
        if !already_reaped {
            match owned_child_pid(&self.child) {
                Ok(process_group_id) => {
                    if let Err(error) = signal_process_group(process_group_id, libc::SIGKILL) {
                        record(error);
                    }
                }
                Err(error) => record(error),
            }
            if let Err(error) = self.child.kill()
                && error.kind() != io::ErrorKind::InvalidInput
            {
                record(error);
            }
            match wait_for_child_exit(&mut self.child, PROCESS_GROUP_KILL_TIMEOUT) {
                Ok(true) => {}
                Ok(false) => record(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned gate child did not exit during direct-child cleanup",
                )),
                Err(error) => record(error),
            }
        }

        if let Some(error) = first_error {
            self.record_cleanup_error(error);
        }
    }
}

impl Drop for OwnedGateChild {
    fn drop(&mut self) {
        self.ensure_direct_child_cleanup();
    }
}

pub(super) fn owned_child_pid(child: &Child) -> io::Result<i32> {
    i32::try_from(child.id()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "owned gate child PID exceeds i32 range",
        )
    })
}

pub(super) fn capture_owned_child(child: &Child) -> io::Result<ProcessHandle> {
    let pid = owned_child_pid(child)?;
    ProcessHandle::capture(pid)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "owned gate child exited before its pidfd identity could be captured",
        )
    })
}

#[derive(Default)]
pub(super) struct CleanupDiagnostics {
    first_error: Option<io::Error>,
}

impl CleanupDiagnostics {
    pub(super) fn record(&mut self, error: io::Error) {
        if self.first_error.is_none() {
            self.first_error = Some(error);
        }
    }

    pub(super) fn capture<T>(&mut self, result: io::Result<T>) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.record(error);
                None
            }
        }
    }

    pub(super) fn has_error(&self) -> bool {
        self.first_error.is_some()
    }

    pub(super) fn into_error(self) -> Option<io::Error> {
        self.first_error
    }

    pub(super) fn finish(self) -> io::Result<()> {
        match self.into_error() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

pub(super) fn timeout_error(timeout: Duration, cleanup: io::Result<Output>) -> io::Result<Output> {
    match cleanup {
        Ok(output) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!(
                "child did not exit within {timeout:?}; stdout={}, stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ),
        )),
        Err(error) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            format!("child did not exit within {timeout:?}; cleanup failed: {error}"),
        )),
    }
}
