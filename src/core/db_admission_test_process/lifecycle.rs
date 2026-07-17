use std::fmt;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Instant;

use super::*;

impl DeadlineChild {
    pub(super) fn pump(&mut self) -> io::Result<()> {
        self.drain_setup()?;
        self.drain_output(true)?;
        self.drain_output(false)?;
        self.observe_leader()?;
        self.try_reap()
    }

    fn drain_setup(&mut self) -> io::Result<()> {
        let Some(active) = self.active_mut() else {
            return Ok(());
        };
        let Some(fd) = active.setup_status.as_ref().map(AsRawFd::as_raw_fd) else {
            return Ok(());
        };
        loop {
            let target = &mut active.setup_bytes[active.setup_filled..];
            if target.is_empty() {
                let record = active.setup_bytes;
                let (stage, errno) = decode_setup_record(record);
                active.setup_outcome = SetupOutcome::Failed { stage, errno };
                active.setup_status.take();
                return Ok(());
            }
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
    }

    fn drain_output(&mut self, stdout: bool) -> io::Result<()> {
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
        loop {
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
    }

    fn observe_leader(&mut self) -> io::Result<()> {
        let Some(active) = self.active_mut() else {
            return Ok(());
        };
        if matches!(active.leader, LeaderState::ExitedUnreaped(_)) {
            return Ok(());
        }
        let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
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
        if unsafe { info.si_pid() } != 0 {
            active.leader = LeaderState::ExitedUnreaped(ExitFacts {
                code: info.si_code,
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

    pub(super) fn close_stdin(&mut self) {
        if let Some(active) = self.active_mut() {
            active.stdin.take();
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
        let Some(active) = self.active() else {
            return;
        };
        let target = if active.group_ready {
            -active.identity.pid
        } else {
            active.identity.pid
        };
        unsafe {
            libc::kill(target, libc::SIGKILL);
            libc::_exit(DROP_EXIT_CODE);
        }
    }
}
