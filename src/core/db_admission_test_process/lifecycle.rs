use std::fmt;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::{Duration, Instant};

use super::*;

const READS_PER_PUMP: usize = 1;

impl ProcessIdentity {
    pub fn still_refers_to_original_process(self) -> bool {
        let Some(expected) = self.start_time_ticks else {
            // SAFETY: signal 0 does not deliver a signal; it only queries the kernel about this
            // PID when no start-time identity was available for the test fixture.
            return unsafe { libc::kill(self.pid, 0) } == 0;
        };
        read_process_start_time(self.pid).is_ok_and(|actual| actual == expected)
    }
}

impl DeadlineChild {
    pub(super) fn pump(&mut self, deadline: Instant) -> io::Result<()> {
        self.drain_setup(deadline)?;
        self.drain_output(true, deadline)?;
        self.drain_output(false, deadline)?;
        self.observe_leader()?;
        self.try_reap()
    }

    fn drain_setup(&mut self, deadline: Instant) -> io::Result<()> {
        let Some(active) = self.active_mut() else {
            return Ok(());
        };
        let Some(fd) = active.setup_status.as_ref().map(AsRawFd::as_raw_fd) else {
            return Ok(());
        };
        for _ in 0..READS_PER_PUMP {
            if Instant::now() >= deadline {
                return Ok(());
            }
            let target = &mut active.setup_bytes[active.setup_filled..];
            if target.is_empty() {
                let record = active.setup_bytes;
                let (stage, errno) = decode_setup_record(record);
                active.setup_outcome = SetupOutcome::Failed { stage, errno };
                active.setup_status.take();
                return Ok(());
            }
            // SAFETY: `fd` is borrowed from `active.setup_status`, which remains owned and
            // open throughout this call; `target` is a mutable, initialized byte slice whose
            // pointer and length describe writable storage for the nonblocking read.
            let result = unsafe { libc::read(fd, target.as_mut_ptr().cast(), target.len()) };
            if result > 0 {
                active.setup_filled += result as usize;
                continue;
            }
            if result == 0 {
                active.setup_status.take();
                active.setup_outcome = if active.setup_filled == 0 {
                    SetupOutcome::ExecSucceeded
                } else if active.setup_filled == SETUP_RECORD_BYTES {
                    let (stage, errno) = decode_setup_record(active.setup_bytes);
                    SetupOutcome::Failed { stage, errno }
                } else {
                    SetupOutcome::Failed {
                        stage: SetupStage::Unknown,
                        errno: libc::EPROTO,
                    }
                };
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }

    fn drain_output(&mut self, stdout: bool, deadline: Instant) -> io::Result<()> {
        let fd = self.active().and_then(|active| {
            if stdout {
                active.stdout.as_ref()
            } else {
                active.stderr.as_ref()
            }
            .map(AsRawFd::as_raw_fd)
        });
        let Some(fd) = fd else {
            return Ok(());
        };
        let mut buffer = [0u8; 16 * 1024];
        for _ in 0..READS_PER_PUMP {
            if Instant::now() >= deadline {
                return Ok(());
            }
            // SAFETY: `fd` is borrowed from the selected OwnedFd and remains open for this
            // call; `buffer` is initialized writable storage, and its exact capacity is passed
            // to the nonblocking read before any bytes are appended to a capture.
            let result = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if result > 0 {
                if stdout {
                    self.stdout_capture.append(&buffer[..result as usize]);
                } else {
                    self.stderr_capture.append(&buffer[..result as usize]);
                }
                continue;
            }
            if result == 0 {
                if let Some(active) = self.active_mut() {
                    if stdout {
                        active.stdout.take();
                    } else {
                        active.stderr.take();
                    }
                }
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() == io::ErrorKind::WouldBlock {
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }

    fn observe_leader(&mut self) -> io::Result<()> {
        let Some(active) = self.active_mut() else {
            return Ok(());
        };
        if matches!(active.leader, LeaderState::ExitedUnreaped(_)) {
            return Ok(());
        }
        // SAFETY: `siginfo_t` is a C output record with a valid all-zero representation; waitid
        // initializes it before any accessor is used.
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
        // SAFETY: `active.identity.pid` names the direct, still-unreaped child owned by this
        // supervisor, and `info` is a writable initialized output buffer for WNOWAIT observation.
        let result = unsafe {
            libc::waitid(
                libc::P_PID,
                active.identity.pid as libc::id_t,
                &mut info,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful waitid call initialized `info`; si_pid is read only to determine
        // whether an exit record was reported.
        if unsafe { info.si_pid() } != 0 {
            active.leader = LeaderState::ExitedUnreaped(ExitFacts {
                code: info.si_code,
                // SAFETY: this branch follows successful waitid with a nonzero si_pid, so the
                // exit status field belongs to the reported direct child event.
                status: unsafe { info.si_status() },
            });
        }
        Ok(())
    }

    pub(super) fn signal_group(
        &mut self,
        signal: i32,
        next_state: GroupFenceState,
        report: &mut CleanupReport,
    ) {
        let Some(active) = self.active_mut() else {
            return;
        };
        // SAFETY: the direct child remains unreaped while it anchors ownership, preventing PID
        // reuse; querying its PGID is therefore an identity check before using a negative PGID.
        if !active.group_ready
            && unsafe { libc::getpgid(active.identity.pid) } == active.identity.pid
        {
            active.group_ready = true;
            active.group_error = None;
        }
        let target = if active.group_ready {
            -active.identity.pid
        } else {
            active.identity.pid
        };
        // SAFETY: a negative target is used only after the owned child verified itself as the
        // process-group leader; otherwise the still-owned direct child PID is signaled directly.
        if unsafe { libc::kill(target, signal) } == 0 {
            active.group = next_state;
            return;
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            active.group = next_state;
        } else {
            report.errors.push(CleanupError {
                operation: if signal == libc::SIGTERM {
                    "signal process group with TERM"
                } else {
                    "signal process group with KILL"
                },
                error,
            });
        }
    }

    pub(super) fn ready_for_final_fence(&self) -> bool {
        self.active().is_none_or(|active| {
            matches!(active.leader, LeaderState::ExitedUnreaped(_))
                && active.stdout.is_none()
                && active.stderr.is_none()
        })
    }

    fn try_reap(&mut self) -> io::Result<()> {
        let ready = self.active().is_some_and(|active| {
            active.group == GroupFenceState::KillFenceSent
                && matches!(active.leader, LeaderState::ExitedUnreaped(_))
                && active.setup_status.is_none()
                && active.stdout.is_none()
                && active.stderr.is_none()
        });
        if !ready {
            return Ok(());
        }
        let active = match &mut self.state {
            Lifecycle::Active(active) => active,
            Lifecycle::Complete(_) => return Ok(()),
        };
        let mut raw_status = 0;
        let waited = loop {
            // SAFETY: this supervisor owns the direct child and retains it unreaped until the
            // final group fence; `raw_status` is valid writable storage for WNOHANG wait status.
            let result =
                unsafe { libc::waitpid(active.identity.pid, &mut raw_status, libc::WNOHANG) };
            if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            break result;
        };
        if waited == 0 {
            return Ok(());
        }
        if waited != active.identity.pid {
            return Err(io::Error::last_os_error());
        }
        let identity = active.identity;
        self.state = Lifecycle::Complete(CompletedChild {
            identity,
            status: ExitStatus::from_raw(raw_status),
        });
        Ok(())
    }

    pub(super) fn wait_for_event(&self, deadline: Instant) -> io::Result<()> {
        let Some(active) = self.active() else {
            return Ok(());
        };
        let mut pollfds = Vec::with_capacity(4);
        if let Some(fd) = &active.pidfd {
            pollfds.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        for fd in [&active.setup_status, &active.stdout, &active.stderr]
            .into_iter()
            .flatten()
        {
            pollfds.push(libc::pollfd {
                fd: fd.as_raw_fd(),
                events: libc::POLLIN | libc::POLLHUP,
                revents: 0,
            });
        }
        poll_many(&mut pollfds, deadline)
    }

    pub fn close_stdin(&mut self) {
        if let Some(active) = self.active_mut() {
            active.stdin.take();
        }
    }

    /// Wait after spawn+stdin write: drain pipes, escalate termination, and reap.
    pub fn wait_output(mut self, timeout: Duration) -> Result<DeadlineOutput, SupervisionError> {
        let deadline = deadline_after(timeout);
        let collection_deadline = work_deadline(deadline, timeout);
        let timed_out = !self.wait_for_leader_exit(collection_deadline)?;
        self.finish_output(deadline, timed_out)
    }

    /// Consume `self` while applying a kill fence so error paths can panic only
    /// after ownership is either reaped or moved into [`IncompleteCleanup`].
    ///
    /// Prefer this over panicking while an active [`DeadlineChild`] is still in
    /// scope so the caller can retain the cleanup report. `Drop` remains
    /// fail-closed: it SIGKILLs an unreaped child group and performs a bounded
    /// reap before panic unwinding continues.
    pub fn force_kill_owned(
        mut self,
        timeout: Duration,
    ) -> Result<CleanupReport, IncompleteCleanup> {
        match self.cleanup_until(deadline_after(timeout), CleanupMode::Kill) {
            CleanupProgress::Complete(report) => Ok(report),
            CleanupProgress::Incomplete { report, resources } => Err(IncompleteCleanup {
                owner: Box::new(self),
                report,
                resources,
            }),
        }
    }

    pub(super) fn active(&self) -> Option<&ActiveChild> {
        match &self.state {
            Lifecycle::Active(active) => Some(active),
            Lifecycle::Complete(_) => None,
        }
    }

    fn active_mut(&mut self) -> Option<&mut ActiveChild> {
        match &mut self.state {
            Lifecycle::Active(active) => Some(active),
            Lifecycle::Complete(_) => None,
        }
    }

    pub(super) fn into_output(mut self, timed_out: bool, cleanup: CleanupReport) -> DeadlineOutput {
        let (identity, status) = match &self.state {
            Lifecycle::Complete(complete) => (complete.identity, complete.status),
            Lifecycle::Active(_) => unreachable!("completed cleanup required before output"),
        };
        let stdout = std::mem::take(&mut self.stdout_capture).finish();
        let stderr = std::mem::take(&mut self.stderr_capture).finish();
        output_from_captures(identity, status, stdout, stderr, timed_out, cleanup)
    }
}

impl fmt::Debug for DeadlineChild {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DeadlineChild")
            .field("resources", &self.resources())
            .finish_non_exhaustive()
    }
}

impl Drop for DeadlineChild {
    fn drop(&mut self) {
        if self.active().is_none() {
            return;
        }
        // Drop only reaches this fail-closed path while the direct child remains owned and
        // unreaped. Keep the SIGKILL process-group fence, then use the ordinary bounded cleanup
        // path to waitpid-reap the direct child. Do not hard-exit here: panic unwinding must be
        // allowed to complete after abandoned cleanup ownership.
        let _ = self.cleanup_until(deadline_after(CLEANUP_RESERVE), CleanupMode::Kill);
    }
}
