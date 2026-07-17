//! Bounded, ownership-safe subprocess supervision for DB-admission tests.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(20);
const TERM_GRACE: Duration = Duration::from_millis(100);
const DEFAULT_SUPERVISION_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_RESERVE: Duration = Duration::from_millis(250);
const PIPE_DRAIN_BYTES_PER_POLL: usize = 64 * 1024;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupStage {
    Term,
    Kill,
    ForcedKill,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupError {
    Signal { signal: libc::c_int, detail: String },
    PipeDrain { stage: CleanupStage, detail: String },
    Reap { stage: CleanupStage, detail: String },
}

#[derive(Debug, Default)]
pub struct CleanupReport {
    pub errors: Vec<CleanupError>,
    pub transferred_to_background_reaper: bool,
}

/// Output and cleanup facts from a deadline-bound child invocation.
pub struct DeadlineOutput {
    pub status: Option<ExitStatus>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub stdout_total_bytes: usize,
    pub stderr_total_bytes: usize,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub cleanup: CleanupReport,
}

impl DeadlineOutput {
    pub fn success(&self) -> bool {
        !self.timed_out && self.status.is_some_and(|status| status.success())
    }
}

enum ChildState {
    /// The only state that may address the child process group. The unreaped
    /// direct child is the identity anchor for every TERM/KILL signal.
    Owned(OwnedChild),
    /// A normal `try_wait` consumed the child. Its numeric PID/PGID is never
    /// used again, even if the operating system later reuses that number.
    Reaped(ExitStatus),
    /// An asynchronous reaper owns the exact `Child` handle after the caller
    /// deadline or a setup failure; the foreground supervisor is disarmed.
    HandedOff,
}

struct OwnedChild {
    child: Child,
    process_group: i32,
}

/// Test-only state machine for a child process and its known process group.
///
/// The spawn thread is intentionally detached: a blocking `Command::spawn`
/// cannot make the caller miss its deadline. If it returns after the caller
/// has timed out, that thread keeps the exact child handle and reaps it.
pub struct DeadlineChild {
    state: ChildState,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}

enum SpawnAttempt {
    Started(DeadlineChild),
    TimedOut,
}

impl DeadlineChild {
    pub fn spawn(command: Command, timeout: Duration) -> io::Result<Self> {
        let deadline = Instant::now() + timeout;
        match Self::spawn_until(command, deadline)? {
            SpawnAttempt::Started(child) => Ok(child),
            SpawnAttempt::TimedOut => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "child spawn exceeded its deadline; background supervisor owns cleanup",
            )),
        }
    }

    pub fn output(mut command: Command, timeout: Duration) -> io::Result<DeadlineOutput> {
        let deadline = Instant::now() + timeout;
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        match Self::spawn_until(command, deadline)? {
            SpawnAttempt::Started(child) => child.wait_for_output_until(deadline),
            SpawnAttempt::TimedOut => Ok(DeadlineOutput {
                status: None,
                stdout: Vec::new(),
                stderr: Vec::new(),
                timed_out: true,
                stdout_total_bytes: 0,
                stderr_total_bytes: 0,
                stdout_truncated: false,
                stderr_truncated: false,
                cleanup: CleanupReport {
                    errors: Vec::new(),
                    transferred_to_background_reaper: true,
                },
            }),
        }
    }

    pub fn write_stdin(&mut self, bytes: &[u8], deadline: Instant) -> io::Result<()> {
        let mut written = 0usize;
        {
            let child = self.owned_child_mut()?;
            let stdin = child.stdin.as_mut().ok_or_else(|| {
                io::Error::new(io::ErrorKind::BrokenPipe, "child stdin is closed")
            })?;
            set_nonblocking(stdin)?;
            while written < bytes.len() {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "deadline expired while writing child stdin",
                    ));
                }
                match stdin.write(&bytes[written..]) {
                    Ok(0) => {
                        return Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "child stdin accepted no bytes",
                        ));
                    }
                    Ok(count) => written = written.saturating_add(count),
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(
                            POLL_INTERVAL.min(deadline.saturating_duration_since(Instant::now())),
                        );
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        Ok(())
    }

    /// Force an abnormal exit while retaining the direct-child ownership
    /// anchor through the group KILL and final reap.
    pub fn force_kill(&mut self) -> CleanupReport {
        let mut stdout = BoundedCapture::default();
        let mut stderr = BoundedCapture::default();
        let deadline = Instant::now() + DEFAULT_SUPERVISION_TIMEOUT;
        let mut report = CleanupReport::default();
        self.signal_owned_group(libc::SIGKILL, &mut report);
        self.close_stdin();
        if let Err(error) = self.poll_until(deadline, &mut stdout, &mut stderr, true) {
            report.errors.push(CleanupError::Reap {
                stage: CleanupStage::ForcedKill,
                detail: error.to_string(),
            });
        }
        if self.is_owned() {
            report.transferred_to_background_reaper = true;
            self.handoff_to_background_reaper();
        }
        report
    }

    pub fn exit_diagnostic(&mut self) -> Option<String> {
        match self.poll_child() {
            Ok(()) => match &self.state {
                ChildState::Reaped(status) => Some(format!("status={status}")),
                ChildState::Owned(_) | ChildState::HandedOff => None,
            },
            Err(error) => Some(format!("wait_error={error}")),
        }
    }

    fn spawn_until(mut command: Command, deadline: Instant) -> io::Result<SpawnAttempt> {
        let (sender, receiver) = mpsc::sync_channel::<io::Result<Child>>(1);
        std::thread::spawn(move || {
            let result = command.process_group(0).spawn();
            if let Err(send_error) = sender.send(result)
                && let Ok(child) = send_error.0
            {
                reap_after_detached_spawn(child);
            }
        });
        let remaining = deadline.saturating_duration_since(Instant::now());
        let child = match receiver.recv_timeout(remaining) {
            Ok(Ok(child)) => child,
            Ok(Err(error)) => return Err(error),
            Err(RecvTimeoutError::Timeout) => return Ok(SpawnAttempt::TimedOut),
            Err(RecvTimeoutError::Disconnected) => {
                return Err(io::Error::other(
                    "spawn supervisor disconnected before reporting",
                ));
            }
        };
        let process_group = child.id() as i32;
        let mut supervised = Self {
            state: ChildState::Owned(OwnedChild {
                child,
                process_group,
            }),
            stdout: None,
            stderr: None,
        };
        let setup = supervised.configure_pipes();
        if let Err(error) = setup {
            supervised.handoff_to_background_reaper();
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "subprocess pipe setup failed; background reaper retained ownership: {error}"
                ),
            ));
        }
        Ok(SpawnAttempt::Started(supervised))
    }

    fn configure_pipes(&mut self) -> io::Result<()> {
        let (stdout, stderr) = {
            let child = self.owned_child_mut()?;
            (child.stdout.take(), child.stderr.take())
        };
        self.stdout = stdout;
        self.stderr = stderr;
        if let Some(stdout) = self.stdout.as_ref() {
            set_nonblocking(stdout)?;
        }
        if let Some(stderr) = self.stderr.as_ref() {
            set_nonblocking(stderr)?;
        }
        Ok(())
    }

    fn wait_for_output_until(mut self, deadline: Instant) -> io::Result<DeadlineOutput> {
        let mut stdout = BoundedCapture::default();
        let mut stderr = BoundedCapture::default();
        let collection_deadline = deadline
            .checked_sub(CLEANUP_RESERVE)
            .unwrap_or_else(Instant::now);
        let completed = match self.poll_until(collection_deadline, &mut stdout, &mut stderr, true) {
            Ok(completed) => completed,
            Err(error) => {
                let cleanup = self.terminate_until(deadline, &mut stdout, &mut stderr);
                return Err(io::Error::new(
                    error.kind(),
                    format!("subprocess supervision failed; cleanup={cleanup:?}: {error}"),
                ));
            }
        };
        let cleanup = if completed {
            CleanupReport::default()
        } else {
            self.terminate_until(deadline, &mut stdout, &mut stderr)
        };
        let status = match &self.state {
            ChildState::Reaped(status) => Some(*status),
            ChildState::Owned(_) | ChildState::HandedOff => None,
        };
        Ok(DeadlineOutput {
            status,
            stdout_total_bytes: stdout.total_bytes,
            stderr_total_bytes: stderr.total_bytes,
            stdout_truncated: stdout.truncated,
            stderr_truncated: stderr.truncated,
            stdout: stdout.into_bytes(),
            stderr: stderr.into_bytes(),
            timed_out: !completed,
            cleanup,
        })
    }

    fn poll_until(
        &mut self,
        deadline: Instant,
        stdout: &mut BoundedCapture,
        stderr: &mut BoundedCapture,
        reap_child: bool,
    ) -> io::Result<bool> {
        loop {
            if self.poll_resources(stdout, stderr, reap_child)? {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            std::thread::sleep(POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
        }
    }

    fn poll_resources(
        &mut self,
        stdout: &mut BoundedCapture,
        stderr: &mut BoundedCapture,
        reap_child: bool,
    ) -> io::Result<bool> {
        let stdout_eof = self
            .stdout
            .as_mut()
            .map(|pipe| drain_pipe(pipe, stdout))
            .transpose()?
            .unwrap_or(true);
        if stdout_eof {
            self.stdout.take();
        }
        let stderr_eof = self
            .stderr
            .as_mut()
            .map(|pipe| drain_pipe(pipe, stderr))
            .transpose()?
            .unwrap_or(true);
        if stderr_eof {
            self.stderr.take();
        }
        if reap_child {
            self.poll_child()?;
        }
        Ok(matches!(self.state, ChildState::Reaped(_))
            && self.stdout.is_none()
            && self.stderr.is_none())
    }

    fn poll_child(&mut self) -> io::Result<()> {
        let status = match &mut self.state {
            ChildState::Owned(owned) => owned.child.try_wait()?,
            ChildState::Reaped(_) | ChildState::HandedOff => return Ok(()),
        };
        if let Some(status) = status {
            self.state = ChildState::Reaped(status);
        }
        Ok(())
    }

    fn terminate_until(
        &mut self,
        deadline: Instant,
        stdout: &mut BoundedCapture,
        stderr: &mut BoundedCapture,
    ) -> CleanupReport {
        let mut report = CleanupReport::default();
        if !self.is_owned() {
            return report;
        }
        self.close_stdin();
        self.signal_owned_group(libc::SIGTERM, &mut report);
        let grace_deadline = (Instant::now() + TERM_GRACE).min(deadline);
        if let Err(error) = self.poll_until(grace_deadline, stdout, stderr, false) {
            report.errors.push(CleanupError::PipeDrain {
                stage: CleanupStage::Term,
                detail: error.to_string(),
            });
        }
        self.signal_owned_group(libc::SIGKILL, &mut report);
        if let Err(error) = self.poll_until(deadline, stdout, stderr, true) {
            report.errors.push(CleanupError::Reap {
                stage: CleanupStage::Kill,
                detail: error.to_string(),
            });
        }
        if self.is_owned() {
            report.transferred_to_background_reaper = true;
            self.handoff_to_background_reaper();
        }
        report
    }

    fn owned_child_mut(&mut self) -> io::Result<&mut Child> {
        match &mut self.state {
            ChildState::Owned(owned) => Ok(&mut owned.child),
            ChildState::Reaped(_) | ChildState::HandedOff => Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "child ownership is no longer available",
            )),
        }
    }

    fn is_owned(&self) -> bool {
        matches!(self.state, ChildState::Owned(_))
    }

    fn signal_owned_group(&self, signal: libc::c_int, report: &mut CleanupReport) {
        self.signal_owned_group_with(signal, report, signal_process_group);
    }

    fn signal_owned_group_with(
        &self,
        signal: libc::c_int,
        report: &mut CleanupReport,
        send_signal: impl FnOnce(i32, libc::c_int) -> io::Result<()>,
    ) {
        let ChildState::Owned(owned) = &self.state else {
            return;
        };
        if let Err(error) = send_signal(owned.process_group, signal) {
            report.errors.push(CleanupError::Signal {
                signal,
                detail: error.to_string(),
            });
        }
    }

    fn close_stdin(&mut self) {
        if let ChildState::Owned(owned) = &mut self.state {
            owned.child.stdin.take();
        }
    }

    fn handoff_to_background_reaper(&mut self) {
        let state = std::mem::replace(&mut self.state, ChildState::HandedOff);
        let ChildState::Owned(owned) = state else {
            self.state = state;
            return;
        };
        self.stdout.take();
        self.stderr.take();
        std::thread::spawn(move || reap_owned_child(owned));
    }
}

impl Drop for DeadlineChild {
    fn drop(&mut self) {
        self.handoff_to_background_reaper();
    }
}

#[derive(Default)]
struct BoundedCapture {
    prefix: Vec<u8>,
    tail: VecDeque<u8>,
    total_bytes: usize,
    truncated: bool,
}

impl BoundedCapture {
    fn append(&mut self, bytes: &[u8]) {
        self.total_bytes = self.total_bytes.saturating_add(bytes.len());
        let prefix_limit = MAX_CAPTURE_BYTES / 2;
        let tail_limit = MAX_CAPTURE_BYTES - prefix_limit;
        let prefix_remaining = prefix_limit.saturating_sub(self.prefix.len());
        let prefix_count = prefix_remaining.min(bytes.len());
        self.prefix.extend_from_slice(&bytes[..prefix_count]);
        for byte in &bytes[prefix_count..] {
            if self.tail.len() == tail_limit {
                self.tail.pop_front();
            }
            self.tail.push_back(*byte);
            self.truncated = true;
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        if !self.truncated {
            return self.prefix;
        }
        let omitted = self
            .total_bytes
            .saturating_sub(self.prefix.len() + self.tail.len());
        let mut bytes = self.prefix;
        bytes.extend_from_slice(format!("\n<... {omitted} bytes truncated ...>\n").as_bytes());
        bytes.extend(self.tail);
        bytes
    }
}

fn set_nonblocking(stream: &impl AsRawFd) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    // SAFETY: `fd` belongs to the live pipe or stdin handle for this call;
    // fcntl reads/modifies only its file status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` remains live and O_NONBLOCK is valid for child pipes.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_pipe(pipe: &mut impl Read, output: &mut BoundedCapture) -> io::Result<bool> {
    let mut drained = 0usize;
    let mut chunk = [0_u8; 8192];
    while drained < PIPE_DRAIN_BYTES_PER_POLL {
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                drained = drained.saturating_add(read);
                output.append(&chunk[..read]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn reap_after_detached_spawn(mut child: Child) {
    let process_group = child.id() as i32;
    child.stdin.take();
    let _ = signal_process_group(process_group, libc::SIGKILL);
    let _ = child.wait();
}

fn reap_owned_child(mut owned: OwnedChild) {
    owned.child.stdin.take();
    let _ = signal_process_group(owned.process_group, libc::SIGKILL);
    let _ = owned.child.wait();
}

fn signal_process_group(process_group: i32, signal: libc::c_int) -> io::Result<()> {
    // SAFETY: callers hold the unreaped direct-child ownership anchor for
    // this group. Negating its PID therefore targets only that owned group.
    if unsafe { libc::kill(-process_group, signal) } == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::process::ExitStatusExt;
    use std::time::Duration;

    use super::*;

    #[test]
    fn reaped_leader_never_signals_a_reused_numeric_process_group() {
        let supervisor = DeadlineChild {
            state: ChildState::Reaped(ExitStatus::from_raw(0)),
            stdout: None,
            stderr: None,
        };
        let mut signals = Vec::new();
        let mut report = CleanupReport::default();

        supervisor.signal_owned_group_with(libc::SIGKILL, &mut report, |group, signal| {
            signals.push((group, signal));
            Ok(())
        });

        assert!(
            signals.is_empty(),
            "reaped ownership must disarm group signalling"
        );
        assert!(report.errors.is_empty());
    }

    #[test]
    fn signal_failure_is_typed_and_preserves_child_ownership_for_cleanup() {
        let mut command = Command::new("sh");
        command.args(["-c", "trap '' TERM; while :; do sleep 60; done"]);
        let mut supervisor =
            DeadlineChild::spawn(command, Duration::from_secs(1)).expect("spawn fixture");
        let mut report = CleanupReport::default();

        supervisor.signal_owned_group_with(libc::SIGTERM, &mut report, |_group, _signal| {
            Err(io::Error::other("synthetic signal failure"))
        });

        assert!(
            supervisor.is_owned(),
            "failed signal must not discard child ownership"
        );
        assert!(matches!(
            report.errors.as_slice(),
            [CleanupError::Signal {
                signal: libc::SIGTERM,
                ..
            }]
        ));
        let cleanup = supervisor.force_kill();
        assert!(cleanup.errors.is_empty(), "{cleanup:?}");
        assert!(!cleanup.transferred_to_background_reaper);
    }
}
