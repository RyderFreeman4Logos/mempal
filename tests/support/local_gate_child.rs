use std::fs;
use std::io;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// A gate timeout may spend this bounded interval terminating and draining its process tree.
const CLEANUP_TIMEOUT_MARGIN: Duration = Duration::from_secs(2);
const TERMINATION_GRACE_PERIOD: Duration = Duration::from_millis(50);
const TERMINATION_DISCOVERY_POLL_INTERVAL: Duration = Duration::from_millis(2);
const POST_KILL_DISCOVERY_RESERVE: Duration = Duration::from_millis(25);
const OUTPUT_DRAIN_RESERVE: Duration = Duration::from_millis(50);
const OWNERSHIP_TOKEN_ENV: &str = "MEMPAL_LOCAL_GATE_OWNER";

fn cleanup_deadline() -> Instant {
    Instant::now() + CLEANUP_TIMEOUT_MARGIN
}

/// Leaves a bounded output-drain reserve while every discovery pass shares one absolute deadline.
fn discovery_deadline(cleanup_deadline: Instant) -> Instant {
    Instant::now()
        + cleanup_deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(OUTPUT_DRAIN_RESERVE)
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct ProcessIdentity {
    pid: i32,
    start_time_ticks: u64,
}

impl ProcessIdentity {
    fn is_running(self) -> io::Result<bool> {
        Ok(self.matches_running_snapshot(inspect_process(self.pid)?.as_ref()))
    }

    fn matches_running_snapshot(self, snapshot: Option<&ProcessSnapshot>) -> bool {
        snapshot.is_some_and(|process| process.identity == self && process.state != 'Z')
    }
}

struct ProcessSnapshot {
    identity: ProcessIdentity,
    parent_pid: i32,
    state: char,
}

struct ProcessHandle {
    identity: ProcessIdentity,
    pidfd: Option<OwnedFd>,
}

#[path = "local_gate_direct_child.rs"]
mod local_gate_direct_child;
use local_gate_direct_child::{
    ChildExitState, CleanupDiagnostics, capture_owned_child, child_exit_state, timeout_error,
    wait_for_child_exit, wait_for_child_exit_unreaped_until,
};
pub(crate) use local_gate_direct_child::{OwnedGateChild, spawn_in_own_session};

#[path = "local_gate_recorded_process.rs"]
mod local_gate_recorded_process;
pub(crate) use local_gate_recorded_process::{RecordedProcessIdentity, capture_recorded_process};

#[path = "local_gate_discovery.rs"]
mod local_gate_discovery;
use local_gate_discovery::{
    DescendantMonitor, capture_live_children, output_pipe_targets, process_holds_writable_pipe,
    refresh_after_leader_reap, refresh_owned_processes, refresh_owned_processes_with_token,
};

enum TrackedProcessSource {
    Descendant,
    PipeFallback { pipe_targets: Vec<PathBuf> },
}

struct TrackedProcess {
    process: ProcessHandle,
    source: TrackedProcessSource,
}

impl TrackedProcess {
    fn descendant(process: ProcessHandle) -> Self {
        Self {
            process,
            source: TrackedProcessSource::Descendant,
        }
    }

    fn pipe_fallback(process: ProcessHandle, pipe_targets: Vec<PathBuf>) -> Self {
        Self {
            process,
            source: TrackedProcessSource::PipeFallback { pipe_targets },
        }
    }

    fn identity(&self) -> ProcessIdentity {
        self.process.identity
    }

    fn requires_cleanup(&self, deadline: Instant) -> io::Result<bool> {
        match &self.source {
            TrackedProcessSource::Descendant => self.process.is_running(),
            TrackedProcessSource::PipeFallback { pipe_targets } => {
                if process_holds_writable_pipe(&self.process, pipe_targets, deadline)? {
                    return Ok(true);
                }
                if self.process.is_running()? {
                    return Err(io::Error::other(format!(
                        "tracked gate process {} no longer holds the gate output pipe; refusing unverified PID cleanup",
                        self.process.identity.pid
                    )));
                }
                Ok(false)
            }
        }
    }

    fn send_signal(&self, signal: i32, deadline: Instant) -> io::Result<()> {
        if self.requires_cleanup(deadline)? {
            self.process.send_signal(signal)?;
        }
        Ok(())
    }
}

impl ProcessHandle {
    fn capture(pid: i32) -> io::Result<Option<Self>> {
        Self::capture_with_parent(pid, None)
    }

    fn capture_child(parent: ProcessIdentity, pid: i32) -> io::Result<Option<Self>> {
        Self::capture_with_parent(pid, Some(parent))
    }

    fn capture_with_parent(
        pid: i32,
        expected_parent: Option<ProcessIdentity>,
    ) -> io::Result<Option<Self>> {
        let Some(initial) = inspect_process(pid)? else {
            return Ok(None);
        };
        if let Some(parent) = expected_parent {
            if !parent.is_running()? || initial.parent_pid != parent.pid {
                return Ok(None);
            }
        }
        let pidfd = open_pidfd(pid)?;
        let Some(confirmed) = inspect_process(pid)? else {
            return Ok(None);
        };
        if confirmed.identity != initial.identity {
            return Ok(None);
        }

        Ok(Some(Self {
            identity: initial.identity,
            pidfd,
        }))
    }

    fn is_running(&self) -> io::Result<bool> {
        self.identity.is_running()
    }

    fn has_pidfd(&self) -> bool {
        self.pidfd.is_some()
    }

    fn send_signal(&self, signal: i32) -> io::Result<()> {
        if !self.is_running()? {
            return Ok(());
        }
        let Some(pidfd) = self.pidfd.as_ref() else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "pidfd is required to signal a tracked gate process safely",
            ));
        };
        send_pidfd_signal(pidfd, signal)
    }
}

pub(crate) struct GateChild {
    child: OwnedGateChild,
    root: ProcessHandle,
    tracked_processes: Vec<TrackedProcess>,
    descendant_monitor: DescendantMonitor,
}

impl GateChild {
    pub(crate) fn new(child: OwnedGateChild) -> io::Result<Self> {
        let root = capture_owned_child(child.child())?;
        let descendant_monitor = DescendantMonitor::spawn(root.identity)?;
        Ok(Self {
            child,
            root,
            tracked_processes: Vec::new(),
            descendant_monitor,
        })
    }

    #[cfg(test)]
    fn new_with_capture_for_test(
        child: OwnedGateChild,
        capture: impl FnOnce(&Child) -> io::Result<ProcessHandle>,
    ) -> io::Result<Self> {
        let root = capture(child.child())?;
        let descendant_monitor = DescendantMonitor::spawn(root.identity)?;
        Ok(Self {
            child,
            root,
            tracked_processes: Vec::new(),
            descendant_monitor,
        })
    }

    pub(crate) fn wait_with_timeout(&mut self, timeout: Duration) -> io::Result<Output> {
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() >= deadline {
                return timeout_error(
                    timeout,
                    self.terminate_and_collect_until(cleanup_deadline()),
                );
            }
            if child_exit_state(self.child.child())? != ChildExitState::Running {
                return self.terminate_and_collect_until(cleanup_deadline());
            }
            self.refresh_tracked_processes_until(deadline)?;
            if Instant::now() >= deadline {
                return timeout_error(
                    timeout,
                    self.terminate_and_collect_until(cleanup_deadline()),
                );
            }
            if child_exit_state(self.child.child())? != ChildExitState::Running {
                return self.terminate_and_collect_until(cleanup_deadline());
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(25)),
            );
        }
    }

    fn refresh_tracked_processes(&mut self) -> io::Result<()> {
        self.refresh_tracked_processes_until(cleanup_deadline())
    }

    fn refresh_tracked_processes_until(&mut self, deadline: Instant) -> io::Result<()> {
        self.descendant_monitor
            .drain_discovered(&mut self.tracked_processes, deadline);
        refresh_owned_processes_with_token(
            self.child.child(),
            self.child.ownership_token(),
            &self.root,
            &mut self.tracked_processes,
            deadline,
        )
    }

    fn terminate_and_collect_until(&mut self, deadline: Instant) -> io::Result<Output> {
        self.descendant_monitor
            .drain_discovered(&mut self.tracked_processes, deadline);
        terminate_and_collect_until(
            &mut self.child,
            &self.root,
            &mut self.tracked_processes,
            &mut self.descendant_monitor,
            deadline,
        )
    }
}

impl Drop for GateChild {
    fn drop(&mut self) {
        let _ = self.terminate_and_collect_until(cleanup_deadline());
    }
}

pub(crate) fn reap_owned_child(mut child: OwnedGateChild) -> io::Result<()> {
    let root = capture_owned_child(child.child())?;
    let mut tracked_processes = Vec::new();
    let mut descendant_monitor = DescendantMonitor::spawn(root.identity)?;
    let deadline = cleanup_deadline();
    let result = terminate_and_collect_until(
        &mut child,
        &root,
        &mut tracked_processes,
        &mut descendant_monitor,
        deadline,
    );
    result.map(|_| ())
}

fn terminate_and_collect(
    child: &mut OwnedGateChild,
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
) -> io::Result<Output> {
    let deadline = cleanup_deadline();
    let mut descendant_monitor = DescendantMonitor::spawn(root.identity)?;
    terminate_and_collect_until(
        child,
        root,
        tracked_processes,
        &mut descendant_monitor,
        deadline,
    )
}

fn terminate_and_collect_until(
    child: &mut OwnedGateChild,
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
    descendant_monitor: &mut DescendantMonitor,
    deadline: Instant,
) -> io::Result<Output> {
    let mut diagnostics = CleanupDiagnostics::default();
    let ownership_token = child.ownership_token().map(str::to_owned);
    if let Err(error) = terminate_owned_process_tree(
        child.child_mut(),
        ownership_token.as_deref(),
        root,
        tracked_processes,
        descendant_monitor,
        deadline,
    ) {
        diagnostics.record(error);
    }
    if let Err(error) = descendant_monitor.stop_and_drain(tracked_processes, deadline) {
        diagnostics.record(error);
    }
    if let Err(error) = signal_tracked_processes(tracked_processes, libc::SIGKILL, deadline) {
        diagnostics.record(error);
    }
    match wait_for_tracked_processes_exit(tracked_processes, deadline) {
        Ok(true) => {}
        Ok(false) => diagnostics.record(io::Error::new(
            io::ErrorKind::TimedOut,
            "tracked gate descendants did not exit before cleanup deadline",
        )),
        Err(error) => diagnostics.record(error),
    }
    let output = match collect_bounded_output(child.child_mut(), deadline) {
        Ok(output) => Some(output),
        Err(error) => {
            diagnostics.record(error);
            None
        }
    };

    if diagnostics.has_error() {
        child.ensure_direct_child_cleanup(deadline);
        if let Some(error) = child.take_cleanup_error() {
            diagnostics.record(error);
        }
        return Err(diagnostics
            .into_error()
            .expect("cleanup diagnostic is present"));
    }

    Ok(output.expect("output collection succeeds without cleanup diagnostics"))
}

fn terminate_owned_process_tree(
    child: &mut Child,
    ownership_token: Option<&str>,
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
    descendant_monitor: &mut DescendantMonitor,
    deadline: Instant,
) -> io::Result<()> {
    let mut diagnostics = CleanupDiagnostics::default();
    let term_deadline = deadline.min(Instant::now() + TERMINATION_GRACE_PERIOD);
    let discovery_deadline = discovery_deadline(deadline);
    // Do not let a pre-KILL procfs pass spend the final window required to discover children
    // of processes that are still dying after SIGKILL.
    let pre_kill_discovery_deadline = discovery_deadline
        .checked_sub(POST_KILL_DISCOVERY_RESERVE)
        .unwrap_or_else(Instant::now);
    let leader_alive = match child_exit_state(child) {
        Ok(ChildExitState::Running) => true,
        Ok(ChildExitState::ExitedUnreaped | ChildExitState::Reaped) => false,
        Err(error) => {
            diagnostics.record(error);
            false
        }
    };
    let mut child_exited = !leader_alive;

    // Capture an identity for a setsid escape before the leader receives SIGTERM. Once the
    // leader has exited, the escape can be reparented and no longer be discoverable from root.
    descendant_monitor.discover_and_drain(tracked_processes, term_deadline);
    let _ = diagnostics.capture(signal_root_process_tree(child, root, libc::SIGTERM));

    // A SIGTERM handler can create a setsid escape after the first discovery pass. Repeatedly
    // drain the monitor and synchronously discover descendants while the leader may still be
    // alive. Once the leader has exited, signal every tracked identity via pidfd; this
    // avoids killing an escape while the leader's TERM handler is still publishing it. The
    // synchronous discovery receives the grace deadline itself, never the monitor's 10ms
    // polling slice. Pipe-holder discovery remains outside this loop because its whole-/proc
    // scan can consume the grace period before a TERM handler creates its escaped child.
    while Instant::now() < term_deadline {
        descendant_monitor.discover_and_drain(tracked_processes, term_deadline);
        descendant_monitor.drain_discovered(tracked_processes, term_deadline);
        if !child_exited {
            child_exited = match child_exit_state(child) {
                Ok(ChildExitState::Running) => false,
                Ok(ChildExitState::ExitedUnreaped | ChildExitState::Reaped) => true,
                Err(error) => {
                    diagnostics.record(error);
                    true
                }
            };
        }
        if child_exited {
            let _ = diagnostics.capture(signal_tracked_processes(
                tracked_processes,
                libc::SIGTERM,
                term_deadline,
            ));
            break;
        }

        let remaining = term_deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        thread::sleep(remaining.min(TERMINATION_DISCOVERY_POLL_INTERVAL));
    }
    if !child_exited {
        child_exited = match wait_for_child_exit_unreaped_until(child, term_deadline) {
            Ok(exited) => exited,
            Err(error) => {
                diagnostics.record(error);
                true
            }
        };
    }
    descendant_monitor.drain_discovered(tracked_processes, deadline);
    let _ = diagnostics.capture(refresh_owned_processes_with_token(
        child,
        ownership_token,
        root,
        tracked_processes,
        pre_kill_discovery_deadline,
    ));
    descendant_monitor.drain_discovered(tracked_processes, deadline);
    if child_exited
        && diagnostics
            .capture(wait_for_tracked_processes_exit(
                tracked_processes,
                term_deadline,
            ))
            .unwrap_or(false)
    {
        return diagnostics.finish();
    }

    let _ = diagnostics.capture(refresh_owned_processes_with_token(
        child,
        ownership_token,
        root,
        tracked_processes,
        pre_kill_discovery_deadline,
    ));
    descendant_monitor.drain_discovered(tracked_processes, deadline);
    let _ = diagnostics.capture(signal_root_process_tree(child, root, libc::SIGKILL));
    let _ = diagnostics.capture(signal_tracked_processes(
        tracked_processes,
        libc::SIGKILL,
        deadline,
    ));
    // SIGKILL can leave a still-running tracked process briefly able to expose a child. Make
    // one final discovery pass before the final pidfd sweep so that child is not stranded when
    // its parent is reaped.
    descendant_monitor.discover_and_drain(tracked_processes, deadline);
    let _ = diagnostics.capture(refresh_owned_processes_with_token(
        child,
        ownership_token,
        root,
        tracked_processes,
        discovery_deadline,
    ));
    descendant_monitor.drain_discovered(tracked_processes, deadline);
    let _ = diagnostics.capture(signal_tracked_processes(
        tracked_processes,
        libc::SIGKILL,
        deadline,
    ));
    if !child_exited {
        child_exited = match wait_for_child_exit_unreaped_until(child, deadline) {
            Ok(exited) => exited,
            Err(error) => {
                diagnostics.record(error);
                true
            }
        };
    }
    descendant_monitor.drain_discovered(tracked_processes, deadline);
    let _ = diagnostics.capture(signal_tracked_processes(
        tracked_processes,
        libc::SIGKILL,
        deadline,
    ));
    if child_exited
        && diagnostics
            .capture(wait_for_tracked_processes_exit(tracked_processes, deadline))
            .unwrap_or(false)
    {
        return diagnostics.finish();
    }

    diagnostics.record(io::Error::new(
        io::ErrorKind::TimedOut,
        "owned gate process tree did not exit after SIGKILL",
    ));
    diagnostics.finish()
}

fn signal_process_group(process_group_id: i32, signal: i32) -> io::Result<()> {
    // SAFETY: Fixtures call `setsid` before exec, making this group exclusive to the fixture.
    // The caller only invokes this while the direct child has not been reaped, so its numeric
    // PID cannot be reused as an unrelated process-group ID.
    unsafe {
        if libc::kill(-process_group_id, signal) == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn signal_root_process_tree(child: &Child, root: &ProcessHandle, signal: i32) -> io::Result<()> {
    let mut diagnostics = CleanupDiagnostics::default();
    let _ = diagnostics.capture(root.send_signal(signal));
    match child_exit_state(child) {
        Ok(ChildExitState::Running | ChildExitState::ExitedUnreaped) => {
            let _ = diagnostics.capture(signal_process_group(root.identity.pid, signal));
        }
        Ok(ChildExitState::Reaped) => {}
        Err(error) => diagnostics.record(error),
    }
    diagnostics.finish()
}

fn signal_tracked_processes(
    processes: &[TrackedProcess],
    signal: i32,
    deadline: Instant,
) -> io::Result<()> {
    let mut diagnostics = CleanupDiagnostics::default();
    for process in processes {
        if Instant::now() >= deadline {
            break;
        }
        let _ = diagnostics.capture(process.send_signal(signal, deadline));
    }
    diagnostics.finish()
}

fn wait_for_tracked_processes_exit(
    processes: &[TrackedProcess],
    deadline: Instant,
) -> io::Result<bool> {
    loop {
        let mut any_running = false;
        for process in processes {
            if Instant::now() >= deadline {
                return Ok(false);
            }
            if process.requires_cleanup(deadline)? {
                any_running = true;
                break;
            }
        }
        if !any_running {
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

fn inspect_process(pid: i32) -> io::Result<Option<ProcessSnapshot>> {
    let stat = match fs::read(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if process_is_gone(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let fields = stat.rsplit(|byte| *byte == b')').next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Linux proc stat was missing the command name terminator",
        )
    })?;
    let mut fields = fields
        .split(u8::is_ascii_whitespace)
        .filter(|field| !field.is_empty());
    let state = fields
        .next()
        .and_then(|state| state.first().copied())
        .map(char::from)
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Linux proc stat missing state")
        })?;
    let parent_pid = std::str::from_utf8(fields.next().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Linux proc stat missing parent PID",
        )
    })?)
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Linux proc parent PID was not numeric",
        )
    })?
    .parse()
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Linux proc parent PID was not numeric",
        )
    })?;
    let start_time_ticks = std::str::from_utf8(fields.nth(17).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Linux proc stat missing start time",
        )
    })?)
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Linux proc start time was not numeric",
        )
    })?
    .parse()
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Linux proc start time was not numeric",
        )
    })?;
    Ok(Some(ProcessSnapshot {
        identity: ProcessIdentity {
            pid,
            start_time_ticks,
        },
        parent_pid,
        state,
    }))
}

fn open_pidfd(pid: i32) -> io::Result<Option<OwnedFd>> {
    // SAFETY: `SYS_pidfd_open` receives a validated signed PID and no pointers. A successful
    // syscall returns a fresh owned file descriptor, transferred into `OwnedFd` below.
    let pidfd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0_u32) };
    if pidfd == -1 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ESRCH) | Some(libc::ENOSYS) | Some(libc::EINVAL) => Ok(None),
            _ => Err(error),
        };
    }
    let pidfd = i32::try_from(pidfd).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "pidfd_open returned a descriptor outside the RawFd range",
        )
    })?;
    // SAFETY: `pidfd_open` returned this new descriptor exactly once, so `OwnedFd` owns it.
    Ok(Some(unsafe { OwnedFd::from_raw_fd(pidfd) }))
}

fn send_pidfd_signal(pidfd: &OwnedFd, signal: i32) -> io::Result<()> {
    // SAFETY: `pidfd` is an owned descriptor returned by `pidfd_open`; the null siginfo pointer
    // and flags 0 select the kernel's ordinary signal delivery semantics for that fixed process.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            pidfd.as_raw_fd(),
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0_u32,
        )
    };
    if result == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error);
        }
    }
    Ok(())
}

fn process_is_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn collect_bounded_output(child: &mut Child, deadline: Instant) -> io::Result<Output> {
    let stdout = spawn_pipe_reader(child.stdout.take());
    let stderr = spawn_pipe_reader(child.stderr.take());
    let status = child.try_wait()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "owned gate child was not reaped before output collection",
        )
    })?;
    Ok(Output {
        status,
        stdout: receive_pipe_output(stdout, deadline)?,
        stderr: receive_pipe_output(stderr, deadline)?,
    })
}

fn spawn_pipe_reader<R: Read + Send + 'static>(
    pipe: Option<R>,
) -> Option<mpsc::Receiver<io::Result<Vec<u8>>>> {
    pipe.map(|mut pipe| {
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            let mut output = Vec::new();
            let _ = sender.send(pipe.read_to_end(&mut output).map(|_| output));
        });
        receiver
    })
}

fn receive_pipe_output(
    receiver: Option<mpsc::Receiver<io::Result<Vec<u8>>>>,
    deadline: Instant,
) -> io::Result<Vec<u8>> {
    let Some(receiver) = receiver else {
        return Ok(Vec::new());
    };
    let remaining = deadline.saturating_duration_since(Instant::now());
    match receiver.recv_timeout(remaining) {
        Ok(output) => output,
        Err(mpsc::RecvTimeoutError::Timeout) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "gate output pipe remained open after process tree cleanup",
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "gate output reader disconnected before completion",
        )),
    }
}

include!("local_gate_child_tests.rs");
include!("local_gate_child_regression_tests.rs");
include!("local_gate_monitor_tests.rs");
