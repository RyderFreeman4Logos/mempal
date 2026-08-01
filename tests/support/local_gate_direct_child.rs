use super::*;
use std::os::unix::process::CommandExt;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_OWNERSHIP_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Owns a direct test child from spawn until it has been reaped.
///
/// This is deliberately established before any fallible `/proc` or pidfd work.
/// Dropping a `std::process::Child` alone neither terminates nor reaps it.
pub(crate) struct OwnedGateChild {
    child: Child,
    ownership_token: Option<String>,
    first_cleanup_error: Option<io::Error>,
    cleanup_deadline: Option<Instant>,
}

pub(crate) fn spawn_in_own_session(command: &mut Command) -> io::Result<OwnedGateChild> {
    let ownership_token = format!(
        "{}-{}",
        std::process::id(),
        NEXT_OWNERSHIP_TOKEN.fetch_add(1, Ordering::Relaxed)
    );
    command.env(OWNERSHIP_TOKEN_ENV, &ownership_token);
    // SAFETY: The post-fork closure invokes only async-signal-safe `setsid` before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
        .spawn()
        .map(|child| OwnedGateChild::with_ownership_token(child, ownership_token))
}

impl OwnedGateChild {
    pub(crate) fn new(child: Child) -> Self {
        Self {
            child,
            ownership_token: None,
            first_cleanup_error: None,
            cleanup_deadline: None,
        }
    }

    fn with_ownership_token(child: Child, ownership_token: String) -> Self {
        Self {
            child,
            ownership_token: Some(ownership_token),
            first_cleanup_error: None,
            cleanup_deadline: None,
        }
    }

    pub(super) fn child(&self) -> &Child {
        &self.child
    }

    pub(super) fn child_mut(&mut self) -> &mut Child {
        &mut self.child
    }

    pub(super) fn ownership_token(&self) -> Option<&str> {
        self.ownership_token.as_deref()
    }

    fn record_cleanup_error(&mut self, error: io::Error) {
        if self.first_cleanup_error.is_none() {
            self.first_cleanup_error = Some(error);
        }
    }

    pub(super) fn take_cleanup_error(&mut self) -> Option<io::Error> {
        self.first_cleanup_error.take()
    }

    pub(super) fn ensure_direct_child_cleanup(&mut self, deadline: Instant) {
        self.cleanup_deadline = Some(
            self.cleanup_deadline
                .map_or(deadline, |existing| existing.min(deadline)),
        );
        let mut first_error = None;
        let mut record = |error: io::Error| {
            if first_error.is_none() {
                first_error = Some(error);
            }
        };

        let child_state = match child_exit_state(&self.child) {
            Ok(state) => Some(state),
            Err(error) => {
                record(error);
                None
            }
        };
        if matches!(
            child_state,
            Some(ChildExitState::Running | ChildExitState::ExitedUnreaped)
        ) {
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
            match wait_for_child_exit_until(&mut self.child, deadline) {
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
        self.ensure_direct_child_cleanup(self.cleanup_deadline.unwrap_or_else(cleanup_deadline));
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

pub(super) fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> io::Result<bool> {
    wait_for_child_exit_until(child, Instant::now() + timeout)
}

pub(super) fn wait_for_child_exit_until(child: &mut Child, deadline: Instant) -> io::Result<bool> {
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(10)),
        );
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ChildExitState {
    Running,
    ExitedUnreaped,
    Reaped,
}

pub(super) fn child_exit_state(child: &Child) -> io::Result<ChildExitState> {
    // SAFETY: all-zero is a valid initial representation for the waitid output record.
    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
    loop {
        // SAFETY: `child.id()` names our direct child. WNOWAIT observes an exit without
        // releasing the PID that fences its dedicated process-group identity.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                child.id() as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result == 0 {
            // SAFETY: successful waitid initialized `info`; zero means no exit event yet.
            return Ok(if unsafe { info.si_pid() } == 0 {
                ChildExitState::Running
            } else {
                ChildExitState::ExitedUnreaped
            });
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        if error.raw_os_error() == Some(libc::ECHILD) {
            return Ok(ChildExitState::Reaped);
        }
        return Err(error);
    }
}

pub(super) fn wait_for_child_exit_unreaped_until(
    child: &Child,
    deadline: Instant,
) -> io::Result<bool> {
    loop {
        if child_exit_state(child)? != ChildExitState::Running {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(
            deadline
                .saturating_duration_since(Instant::now())
                .min(Duration::from_millis(10)),
        );
    }
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
