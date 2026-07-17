//! Linux-only, test-owned subprocess supervision for admission crash fixtures.
//!
//! The direct child stays unreaped as the process-group identity anchor until a
//! final group fence has been issued and every owned pipe has reached EOF.

#[path = "db_admission_test_process/capture.rs"]
mod capture;
#[path = "db_admission_test_process/lifecycle.rs"]
mod lifecycle;
#[path = "db_admission_test_process/spawn.rs"]
mod spawn;

use std::fmt;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::process::ExitStatus;
use std::time::{Duration, Instant};

pub use capture::{BoundedCapture, CAPTURE_LIMIT_BYTES, CapturedBytes, render_diagnostic};
use spawn::{RawSpawn, SETUP_RECORD_BYTES, decode_setup_record, spawn_owned};
pub use spawn::{SpawnSpec, StdioMode, TestSetupGate};

const CLEANUP_RESERVE: Duration = Duration::from_millis(500);
const TERM_GRACE: Duration = Duration::from_millis(100);
const POLL_SLICE: Duration = Duration::from_millis(25);
const DROP_EXIT_CODE: i32 = 125;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProcessIdentity {
    pub pid: libc::pid_t,
    pub start_time_ticks: Option<u64>,
}

impl ProcessIdentity {
    pub fn still_refers_to_original_process(self) -> bool {
        let Some(expected) = self.start_time_ticks else {
            return unsafe { libc::kill(self.pid, 0) } == 0;
        };
        read_process_start_time(self.pid).is_ok_and(|actual| actual == expected)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetupStage {
    SetProcessGroup,
    ReadyHandshake,
    SetupGate,
    ChangeDirectory,
    StandardInput,
    StandardOutput,
    StandardError,
    Exec,
    Unknown,
}

impl SetupStage {
    fn to_wire(self) -> u8 {
        match self {
            Self::SetProcessGroup => 1,
            Self::ReadyHandshake => 2,
            Self::SetupGate => 3,
            Self::ChangeDirectory => 4,
            Self::StandardInput => 5,
            Self::StandardOutput => 6,
            Self::StandardError => 7,
            Self::Exec => 8,
            Self::Unknown => u8::MAX,
        }
    }

    fn from_wire(value: u8) -> Self {
        match value {
            1 => Self::SetProcessGroup,
            2 => Self::ReadyHandshake,
            3 => Self::SetupGate,
            4 => Self::ChangeDirectory,
            5 => Self::StandardInput,
            6 => Self::StandardOutput,
            7 => Self::StandardError,
            8 => Self::Exec,
            _ => Self::Unknown,
        }
    }
}

pub struct CleanupError {
    pub operation: &'static str,
    pub error: io::Error,
}

impl fmt::Debug for CleanupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CleanupError")
            .field("operation", &self.operation)
            .field("error", &self.error)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupDisposition {
    Complete,
}

#[derive(Debug)]
pub struct CleanupReport {
    pub errors: Vec<CleanupError>,
    pub term_grace_expired: bool,
    pub kill_fence_sent: bool,
    pub disposition: CleanupDisposition,
}

impl CleanupReport {
    fn new() -> Self {
        Self {
            errors: Vec::new(),
            term_grace_expired: false,
            kill_fence_sent: false,
            disposition: CleanupDisposition::Complete,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LeaderResourceState {
    Running,
    ExitedUnreaped,
    Reaped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GroupFenceState {
    Unfenced,
    TermSent,
    KillFenceSent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResourceSnapshot {
    pub identity: ProcessIdentity,
    pub leader: LeaderResourceState,
    pub group: GroupFenceState,
    pub setup_pipe_open: bool,
    pub stdin_pipe_open: bool,
    pub stdout_pipe_open: bool,
    pub stderr_pipe_open: bool,
}

#[derive(Debug)]
pub enum CleanupProgress {
    Complete(CleanupReport),
    Incomplete {
        report: CleanupReport,
        resources: ResourceSnapshot,
    },
}

impl CleanupProgress {
    pub fn expect_complete(self, context: &str) -> CleanupReport {
        match self {
            Self::Complete(report) => report,
            Self::Incomplete { report, resources } => {
                panic!("{context}: cleanup incomplete: {resources:?}; report: {report:?}")
            }
        }
    }
}

pub struct IncompleteCleanup {
    owner: Box<DeadlineChild>,
    pub report: CleanupReport,
    pub resources: ResourceSnapshot,
}

impl IncompleteCleanup {
    pub fn finish(mut self, timeout: Duration) -> Result<CleanupReport, Self> {
        match self
            .owner
            .cleanup_until(deadline_after(timeout), CleanupMode::Kill)
        {
            CleanupProgress::Complete(report) => Ok(report),
            CleanupProgress::Incomplete { report, resources } => {
                self.report = report;
                self.resources = resources;
                Err(self)
            }
        }
    }
}

impl fmt::Debug for IncompleteCleanup {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IncompleteCleanup")
            .field("owner", &self.owner.resources())
            .field("report", &self.report)
            .field("resources", &self.resources)
            .finish_non_exhaustive()
    }
}

pub enum SupervisionError {
    Io(io::Error),
    Setup {
        stage: SetupStage,
        error: io::Error,
        cleanup: CleanupReport,
    },
    SetupTimedOut {
        identity: ProcessIdentity,
        cleanup: CleanupReport,
    },
    CleanupIncomplete(IncompleteCleanup),
}

impl fmt::Debug for SupervisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => formatter.debug_tuple("Io").field(error).finish(),
            Self::Setup {
                stage,
                error,
                cleanup,
            } => formatter
                .debug_struct("Setup")
                .field("stage", stage)
                .field("error", error)
                .field("cleanup", cleanup)
                .finish(),
            Self::SetupTimedOut { identity, cleanup } => formatter
                .debug_struct("SetupTimedOut")
                .field("identity", identity)
                .field("cleanup", cleanup)
                .finish(),
            Self::CleanupIncomplete(cleanup) => formatter
                .debug_tuple("CleanupIncomplete")
                .field(cleanup)
                .finish(),
        }
    }
}

impl fmt::Display for SupervisionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "subprocess I/O failed: {error}"),
            Self::Setup { stage, error, .. } => {
                write!(formatter, "child setup failed at {stage:?}: {error}")
            }
            Self::SetupTimedOut { identity, .. } => {
                write!(
                    formatter,
                    "child {} setup exceeded its deadline",
                    identity.pid
                )
            }
            Self::CleanupIncomplete(cleanup) => {
                write!(
                    formatter,
                    "subprocess cleanup incomplete: {:?}",
                    cleanup.resources
                )
            }
        }
    }
}

impl std::error::Error for SupervisionError {}

impl From<io::Error> for SupervisionError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Debug)]
pub struct DeadlineOutput {
    pub identity: ProcessIdentity,
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
    pub stdout_total_bytes: usize,
    pub stderr_total_bytes: usize,
    pub stdout_omitted_bytes: usize,
    pub stderr_omitted_bytes: usize,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub cleanup: CleanupReport,
}

impl DeadlineOutput {
    pub fn success(&self) -> bool {
        self.status.success()
    }

    pub fn stdout_diagnostic(&self) -> Vec<u8> {
        render_diagnostic(&self.stdout, self.stdout_omitted_bytes)
    }

    pub fn stderr_diagnostic(&self) -> Vec<u8> {
        render_diagnostic(&self.stderr, self.stderr_omitted_bytes)
    }
}

pub struct DeadlineChild {
    state: Lifecycle,
    stdout_capture: BoundedCapture,
    stderr_capture: BoundedCapture,
}

enum Lifecycle {
    Active(ActiveChild),
    Complete(CompletedChild),
}

struct ActiveChild {
    identity: ProcessIdentity,
    pidfd: Option<OwnedFd>,
    leader: LeaderState,
    group: GroupFenceState,
    group_ready: bool,
    group_error: Option<io::Error>,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
    setup_status: Option<OwnedFd>,
    setup_bytes: [u8; SETUP_RECORD_BYTES],
    setup_filled: usize,
    setup_outcome: SetupOutcome,
}

#[derive(Clone, Copy)]
enum LeaderState {
    Running,
    ExitedUnreaped(ExitFacts),
}

#[derive(Clone, Copy)]
struct ExitFacts {
    code: i32,
    status: i32,
}

#[derive(Clone, Copy)]
enum SetupOutcome {
    Pending,
    ExecSucceeded,
    Failed { stage: SetupStage, errno: i32 },
}

struct CompletedChild {
    identity: ProcessIdentity,
    status: ExitStatus,
}

enum SetupWait {
    Ready,
    Failed { stage: SetupStage, errno: i32 },
    TimedOut,
}

#[derive(Clone, Copy)]
enum CleanupMode {
    TermThenKill,
    Kill,
}

impl DeadlineChild {
    pub fn spawn(spec: SpawnSpec, timeout: Duration) -> Result<Self, SupervisionError> {
        let deadline = deadline_after(timeout);
        let setup_deadline = work_deadline(deadline, timeout);
        let mut child = Self::launch(spec)?;
        match child.wait_for_setup(setup_deadline)? {
            SetupWait::Ready => Ok(child),
            SetupWait::Failed { stage, errno } => {
                let cleanup = child.cleanup_until(deadline, CleanupMode::Kill);
                match cleanup {
                    CleanupProgress::Complete(cleanup) => Err(SupervisionError::Setup {
                        stage,
                        error: io::Error::from_raw_os_error(errno),
                        cleanup,
                    }),
                    CleanupProgress::Incomplete { report, resources } => {
                        Err(SupervisionError::CleanupIncomplete(IncompleteCleanup {
                            owner: Box::new(child),
                            report,
                            resources,
                        }))
                    }
                }
            }
            SetupWait::TimedOut => {
                let identity = child.identity();
                match child.cleanup_until(deadline, CleanupMode::Kill) {
                    CleanupProgress::Complete(cleanup) => {
                        Err(SupervisionError::SetupTimedOut { identity, cleanup })
                    }
                    CleanupProgress::Incomplete { report, resources } => {
                        Err(SupervisionError::CleanupIncomplete(IncompleteCleanup {
                            owner: Box::new(child),
                            report,
                            resources,
                        }))
                    }
                }
            }
        }
    }

    pub fn output(spec: SpawnSpec, timeout: Duration) -> Result<DeadlineOutput, SupervisionError> {
        let deadline = deadline_after(timeout);
        let collection_deadline = work_deadline(deadline, timeout);
        let mut child = Self::launch(spec)?;
        let setup = child.wait_for_setup(collection_deadline)?;
        if let SetupWait::Failed { stage, errno } = setup {
            return match child.cleanup_until(deadline, CleanupMode::Kill) {
                CleanupProgress::Complete(cleanup) => Err(SupervisionError::Setup {
                    stage,
                    error: io::Error::from_raw_os_error(errno),
                    cleanup,
                }),
                CleanupProgress::Incomplete { report, resources } => {
                    Err(SupervisionError::CleanupIncomplete(IncompleteCleanup {
                        owner: Box::new(child),
                        report,
                        resources,
                    }))
                }
            };
        }

        let mut timed_out = matches!(setup, SetupWait::TimedOut);
        if !timed_out {
            timed_out = !child.wait_for_leader_exit(collection_deadline)?;
        }
        let mode = if timed_out {
            CleanupMode::TermThenKill
        } else {
            CleanupMode::Kill
        };
        match child.cleanup_until(deadline, mode) {
            CleanupProgress::Complete(cleanup) => Ok(child.into_output(timed_out, cleanup)),
            CleanupProgress::Incomplete { report, resources } => {
                Err(SupervisionError::CleanupIncomplete(IncompleteCleanup {
                    owner: Box::new(child),
                    report,
                    resources,
                }))
            }
        }
    }

    fn launch(spec: SpawnSpec) -> Result<Self, SupervisionError> {
        let raw = spawn_owned(spec)?;
        Ok(Self::from_raw(raw))
    }

    fn from_raw(raw: RawSpawn) -> Self {
        Self {
            state: Lifecycle::Active(ActiveChild {
                identity: raw.identity,
                pidfd: raw.pidfd,
                leader: LeaderState::Running,
                group: GroupFenceState::Unfenced,
                group_ready: raw.group_ready,
                group_error: raw.group_error,
                stdin: raw.stdin,
                stdout: raw.stdout,
                stderr: raw.stderr,
                setup_status: Some(raw.setup_status),
                setup_bytes: [0; SETUP_RECORD_BYTES],
                setup_filled: 0,
                setup_outcome: SetupOutcome::Pending,
            }),
            stdout_capture: BoundedCapture::new(),
            stderr_capture: BoundedCapture::new(),
        }
    }

    pub fn identity(&self) -> ProcessIdentity {
        match &self.state {
            Lifecycle::Active(active) => active.identity,
            Lifecycle::Complete(complete) => complete.identity,
        }
    }

    pub fn resources(&self) -> ResourceSnapshot {
        match &self.state {
            Lifecycle::Active(active) => ResourceSnapshot {
                identity: active.identity,
                leader: match active.leader {
                    LeaderState::Running => LeaderResourceState::Running,
                    LeaderState::ExitedUnreaped(_) => LeaderResourceState::ExitedUnreaped,
                },
                group: active.group,
                setup_pipe_open: active.setup_status.is_some(),
                stdin_pipe_open: active.stdin.is_some(),
                stdout_pipe_open: active.stdout.is_some(),
                stderr_pipe_open: active.stderr.is_some(),
            },
            Lifecycle::Complete(complete) => ResourceSnapshot {
                identity: complete.identity,
                leader: LeaderResourceState::Reaped,
                group: GroupFenceState::KillFenceSent,
                setup_pipe_open: false,
                stdin_pipe_open: false,
                stdout_pipe_open: false,
                stderr_pipe_open: false,
            },
        }
    }

    pub fn exit_diagnostic(&mut self) -> io::Result<String> {
        self.pump()?;
        Ok(match &self.state {
            Lifecycle::Active(active) => match active.leader {
                LeaderState::Running => format!("child {} is still running", active.identity.pid),
                LeaderState::ExitedUnreaped(facts) => format!(
                    "child {} exited but remains the group anchor (si_code={}, status={})",
                    active.identity.pid, facts.code, facts.status
                ),
            },
            Lifecycle::Complete(complete) => {
                format!(
                    "child {} reaped with {}",
                    complete.identity.pid, complete.status
                )
            }
        })
    }

    pub fn wait_for_exit_diagnostic(&mut self, timeout: Duration) -> io::Result<String> {
        let deadline = deadline_after(timeout);
        loop {
            let diagnostic = self.exit_diagnostic()?;
            if self.resources().leader != LeaderResourceState::Running {
                return Ok(diagnostic);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "child did not exit before diagnostic deadline",
                ));
            }
            self.wait_for_event(deadline)?;
        }
    }

    pub fn write_stdin(&mut self, bytes: &[u8], timeout: Duration) -> io::Result<()> {
        let deadline = deadline_after(timeout);
        let mut written = 0usize;
        while written < bytes.len() {
            let fd = match &self.state {
                Lifecycle::Active(active) => active
                    .stdin
                    .as_ref()
                    .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "stdin unavailable"))?
                    .as_raw_fd(),
                Lifecycle::Complete(_) => {
                    return Err(io::Error::new(io::ErrorKind::BrokenPipe, "child exited"));
                }
            };
            let result =
                unsafe { libc::write(fd, bytes[written..].as_ptr().cast(), bytes.len() - written) };
            if result > 0 {
                written += result as usize;
                continue;
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                if error.kind() != io::ErrorKind::WouldBlock {
                    return Err(error);
                }
            }
            poll_one(fd, libc::POLLOUT, deadline)?;
        }
        Ok(())
    }

    pub fn force_kill(&mut self) -> CleanupProgress {
        self.cleanup_until(deadline_after(Duration::from_secs(5)), CleanupMode::Kill)
    }

    pub fn force_kill_with_timeout(&mut self, timeout: Duration) -> CleanupProgress {
        self.cleanup_until(deadline_after(timeout), CleanupMode::Kill)
    }

    pub fn terminate(&mut self, timeout: Duration) -> CleanupProgress {
        self.cleanup_until(deadline_after(timeout), CleanupMode::TermThenKill)
    }

    fn wait_for_setup(&mut self, deadline: Instant) -> io::Result<SetupWait> {
        loop {
            self.pump()?;
            let outcome = match &self.state {
                Lifecycle::Active(active) => active.setup_outcome,
                Lifecycle::Complete(_) => SetupOutcome::ExecSucceeded,
            };
            match outcome {
                SetupOutcome::ExecSucceeded => return Ok(SetupWait::Ready),
                SetupOutcome::Failed { stage, errno } => {
                    return Ok(SetupWait::Failed { stage, errno });
                }
                SetupOutcome::Pending if Instant::now() >= deadline => {
                    return Ok(SetupWait::TimedOut);
                }
                SetupOutcome::Pending => self.wait_for_event(deadline)?,
            }
        }
    }

    fn wait_for_leader_exit(&mut self, deadline: Instant) -> io::Result<bool> {
        loop {
            self.pump()?;
            if self.resources().leader != LeaderResourceState::Running {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            self.wait_for_event(deadline)?;
        }
    }

    fn cleanup_until(&mut self, deadline: Instant, mode: CleanupMode) -> CleanupProgress {
        let mut report = CleanupReport::new();
        let had_cleanup_budget = Instant::now() < deadline;
        self.close_stdin();
        if matches!(mode, CleanupMode::TermThenKill) {
            self.signal_group(libc::SIGTERM, GroupFenceState::TermSent, &mut report);
            let grace_deadline = (Instant::now() + TERM_GRACE).min(deadline);
            while Instant::now() < grace_deadline {
                if let Err(error) = self.pump() {
                    report.errors.push(CleanupError {
                        operation: "observe during TERM grace",
                        error,
                    });
                    break;
                }
                if self.ready_for_final_fence() {
                    break;
                }
                if let Err(error) = self.wait_for_event(grace_deadline) {
                    if error.kind() != io::ErrorKind::TimedOut {
                        report.errors.push(CleanupError {
                            operation: "poll during TERM grace",
                            error,
                        });
                    }
                    break;
                }
            }
            if !self.ready_for_final_fence() {
                report.term_grace_expired = true;
            }
        }

        self.signal_group(libc::SIGKILL, GroupFenceState::KillFenceSent, &mut report);
        report.kill_fence_sent = self
            .active()
            .is_none_or(|active| active.group == GroupFenceState::KillFenceSent);

        while Instant::now() < deadline {
            if let Err(error) = self.pump() {
                report.errors.push(CleanupError {
                    operation: "drain/observe/reap after KILL fence",
                    error,
                });
                break;
            }
            if matches!(self.state, Lifecycle::Complete(_)) {
                report.disposition = CleanupDisposition::Complete;
                return CleanupProgress::Complete(report);
            }
            if let Err(error) = self.wait_for_event(deadline) {
                if error.kind() != io::ErrorKind::TimedOut {
                    report.errors.push(CleanupError {
                        operation: "poll after KILL fence",
                        error,
                    });
                }
                break;
            }
        }
        if had_cleanup_budget {
            let _ = self.pump().map_err(|error| {
                report.errors.push(CleanupError {
                    operation: "final cleanup observation",
                    error,
                });
            });
        }
        if matches!(self.state, Lifecycle::Complete(_)) {
            CleanupProgress::Complete(report)
        } else {
            CleanupProgress::Incomplete {
                report,
                resources: self.resources(),
            }
        }
    }
}

fn output_from_captures(
    identity: ProcessIdentity,
    status: ExitStatus,
    stdout: CapturedBytes,
    stderr: CapturedBytes,
    timed_out: bool,
    cleanup: CleanupReport,
) -> DeadlineOutput {
    DeadlineOutput {
        identity,
        status,
        timed_out,
        stdout_truncated: stdout.omitted_bytes > 0,
        stderr_truncated: stderr.omitted_bytes > 0,
        stdout_total_bytes: stdout.total_bytes,
        stderr_total_bytes: stderr.total_bytes,
        stdout_omitted_bytes: stdout.omitted_bytes,
        stderr_omitted_bytes: stderr.omitted_bytes,
        stdout: stdout.bytes,
        stderr: stderr.bytes,
        cleanup,
    }
}

fn deadline_after(timeout: Duration) -> Instant {
    Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(Instant::now)
}

fn work_deadline(deadline: Instant, timeout: Duration) -> Instant {
    let reserve = CLEANUP_RESERVE.min(timeout / 2);
    deadline.checked_sub(reserve).unwrap_or(deadline)
}

fn poll_one(fd: RawFd, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    let mut pollfd = [libc::pollfd {
        fd,
        events,
        revents: 0,
    }];
    poll_many(&mut pollfd, deadline)
}

fn poll_many(pollfds: &mut [libc::pollfd], deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline expired"));
        }
        let timeout = remaining.min(POLL_SLICE);
        let timeout_ms = timeout.as_millis().max(1).min(i32::MAX as u128) as i32;
        let result = unsafe {
            libc::poll(
                pollfds.as_mut_ptr(),
                pollfds.len() as libc::nfds_t,
                timeout_ms,
            )
        };
        if result >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn read_process_start_time(pid: libc::pid_t) -> io::Result<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let (_, fields) = stat.rsplit_once(") ").ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "malformed /proc process stat")
    })?;
    fields
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "missing process start time"))?
        .parse()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}
