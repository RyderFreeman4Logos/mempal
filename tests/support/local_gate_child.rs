use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Read;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const PROCESS_GROUP_TERM_TIMEOUT: Duration = Duration::from_millis(250);
const PROCESS_GROUP_KILL_TIMEOUT: Duration = Duration::from_millis(250);
const OUTPUT_DRAIN_TIMEOUT: Duration = Duration::from_millis(250);

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
pub(crate) use local_gate_direct_child::OwnedGateChild;
use local_gate_direct_child::{CleanupDiagnostics, capture_owned_child, timeout_error};

#[path = "local_gate_recorded_process.rs"]
mod local_gate_recorded_process;
pub(crate) use local_gate_recorded_process::{RecordedProcessIdentity, capture_recorded_process};

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

    fn requires_cleanup(&self) -> io::Result<bool> {
        match &self.source {
            TrackedProcessSource::Descendant => self.process.is_running(),
            TrackedProcessSource::PipeFallback { pipe_targets } => {
                process_holds_writable_pipe(&self.process, pipe_targets)
            }
        }
    }

    fn send_signal(&self, signal: i32) -> io::Result<()> {
        if self.requires_cleanup()? {
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
}

pub(crate) fn spawn_in_own_session(command: &mut Command) -> io::Result<OwnedGateChild> {
    // SAFETY: The post-fork closure invokes only async-signal-safe `setsid` before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn().map(OwnedGateChild::new)
}

impl GateChild {
    pub(crate) fn new(child: OwnedGateChild) -> io::Result<Self> {
        let root = capture_owned_child(child.child())?;
        Ok(Self {
            child,
            root,
            tracked_processes: Vec::new(),
        })
    }

    #[cfg(test)]
    fn new_with_capture_for_test(
        child: OwnedGateChild,
        capture: impl FnOnce(&Child) -> io::Result<ProcessHandle>,
    ) -> io::Result<Self> {
        let root = capture(child.child())?;
        Ok(Self {
            child,
            root,
            tracked_processes: Vec::new(),
        })
    }

    pub(crate) fn wait_with_timeout(&mut self, timeout: Duration) -> io::Result<Output> {
        let deadline = Instant::now() + timeout;
        loop {
            self.refresh_tracked_processes()?;
            let exited = self.child.child_mut().try_wait()?.is_some();
            if exited {
                return terminate_and_collect(
                    &mut self.child,
                    &self.root,
                    &mut self.tracked_processes,
                );
            }
            if Instant::now() >= deadline {
                return timeout_error(
                    timeout,
                    terminate_and_collect(&mut self.child, &self.root, &mut self.tracked_processes),
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn refresh_tracked_processes(&mut self) -> io::Result<()> {
        refresh_owned_processes(self.child.child(), &self.root, &mut self.tracked_processes)
    }
}

impl Drop for GateChild {
    fn drop(&mut self) {
        let _ = terminate_and_collect(&mut self.child, &self.root, &mut self.tracked_processes);
    }
}

pub(crate) fn reap_owned_child(mut child: OwnedGateChild) -> io::Result<()> {
    let root = capture_owned_child(child.child())?;
    let mut tracked_processes = Vec::new();
    terminate_and_collect(&mut child, &root, &mut tracked_processes).map(|_| ())
}

fn terminate_and_collect(
    child: &mut OwnedGateChild,
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
) -> io::Result<Output> {
    let mut diagnostics = CleanupDiagnostics::default();
    if let Err(error) = terminate_owned_process_tree(child.child_mut(), root, tracked_processes) {
        diagnostics.record(error);
    }
    let output = match collect_bounded_output(child.child_mut()) {
        Ok(output) => Some(output),
        Err(error) => {
            diagnostics.record(error);
            None
        }
    };

    if diagnostics.has_error() {
        child.ensure_direct_child_cleanup();
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
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
) -> io::Result<()> {
    let mut diagnostics = CleanupDiagnostics::default();
    let _ = diagnostics.capture(refresh_owned_processes(child, root, tracked_processes));
    let mut child_exited = diagnostics
        .capture(refresh_after_leader_reap(child, root, tracked_processes))
        .unwrap_or(false);
    if !child_exited {
        let _ = diagnostics.capture(signal_root_process_tree(root, libc::SIGTERM));
    }
    let _ = diagnostics.capture(signal_tracked_processes(tracked_processes, libc::SIGTERM));
    if !child_exited {
        child_exited = diagnostics
            .capture(wait_for_child_exit(child, PROCESS_GROUP_TERM_TIMEOUT))
            .unwrap_or(false);
        if child_exited {
            let _ = diagnostics.capture(refresh_owned_processes(child, root, tracked_processes));
        }
    }
    if child_exited
        && diagnostics
            .capture(wait_for_tracked_processes_exit(
                tracked_processes,
                PROCESS_GROUP_TERM_TIMEOUT,
            ))
            .unwrap_or(false)
    {
        return diagnostics.finish();
    }

    let _ = diagnostics.capture(refresh_owned_processes(child, root, tracked_processes));
    if !child_exited {
        let _ = diagnostics.capture(signal_root_process_tree(root, libc::SIGKILL));
    }
    let _ = diagnostics.capture(signal_tracked_processes(tracked_processes, libc::SIGKILL));
    if !child_exited {
        child_exited = diagnostics
            .capture(wait_for_child_exit(child, PROCESS_GROUP_KILL_TIMEOUT))
            .unwrap_or(false);
        if child_exited {
            let _ = diagnostics.capture(refresh_owned_processes(child, root, tracked_processes));
        }
    }
    if child_exited
        && diagnostics
            .capture(wait_for_tracked_processes_exit(
                tracked_processes,
                PROCESS_GROUP_KILL_TIMEOUT,
            ))
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
    // SAFETY: The direct child is still owned, so its unreaped PID cannot be reused. Fixtures
    // call `setsid` before exec, making this group exclusive to the fixture. `ESRCH` means the
    // group already exited.
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

fn signal_root_process_tree(root: &ProcessHandle, signal: i32) -> io::Result<()> {
    let mut diagnostics = CleanupDiagnostics::default();
    if root.has_pidfd() {
        let _ = diagnostics.capture(root.send_signal(signal));
    }
    let _ = diagnostics.capture(signal_process_group(root.identity.pid, signal));
    diagnostics.finish()
}

fn signal_tracked_processes(processes: &[TrackedProcess], signal: i32) -> io::Result<()> {
    let mut diagnostics = CleanupDiagnostics::default();
    for process in processes {
        let _ = diagnostics.capture(process.send_signal(signal));
    }
    diagnostics.finish()
}

fn wait_for_tracked_processes_exit(
    processes: &[TrackedProcess],
    timeout: Duration,
) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut any_running = false;
        for process in processes {
            if process.requires_cleanup()? {
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
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn refresh_after_leader_reap(
    child: &mut Child,
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
) -> io::Result<bool> {
    let reaped = child.try_wait()?.is_some();
    if reaped {
        refresh_owned_processes(child, root, tracked_processes)?;
    }
    Ok(reaped)
}

fn refresh_owned_processes(
    child: &Child,
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
) -> io::Result<()> {
    track_output_pipe_holders(child, tracked_processes)?;
    let mut pending = Vec::new();
    for process in tracked_processes.iter() {
        if process.requires_cleanup()? {
            pending.push(process.identity());
        }
    }
    if root.is_running()? {
        pending.push(root.identity);
    }

    let mut visited = HashSet::new();
    while let Some(identity) = pending.pop() {
        if !visited.insert(identity) || !identity.is_running()? {
            continue;
        }
        for descendant in capture_live_children(identity)? {
            if !tracked_processes
                .iter()
                .any(|tracked| tracked.identity() == descendant.identity)
            {
                pending.push(descendant.identity);
                tracked_processes.push(TrackedProcess::descendant(descendant));
            }
        }
    }
    Ok(())
}

fn track_output_pipe_holders(
    child: &Child,
    tracked_processes: &mut Vec<TrackedProcess>,
) -> io::Result<()> {
    let pipe_targets = output_pipe_targets(child)?;
    if pipe_targets.is_empty() {
        return Ok(());
    }

    let current_pid = i32::try_from(std::process::id()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "test process PID exceeds i32 range",
        )
    })?;
    for entry in fs::read_dir("/proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<i32>() else {
            continue;
        };
        if pid == current_pid {
            continue;
        }
        let process = match ProcessHandle::capture(pid) {
            Ok(Some(process)) => process,
            Ok(None) => continue,
            Err(error) if process_scan_error(&error) => continue,
            Err(error) => return Err(error),
        };
        let holds_pipe = match process_holds_writable_pipe(&process, &pipe_targets) {
            Ok(holds_pipe) => holds_pipe,
            Err(error) if process_scan_error(&error) => continue,
            Err(error) => return Err(error),
        };
        if !holds_pipe {
            continue;
        }
        if !tracked_processes
            .iter()
            .any(|tracked| tracked.identity() == process.identity)
        {
            tracked_processes.push(TrackedProcess::pipe_fallback(process, pipe_targets.clone()));
        }
    }
    Ok(())
}

fn pipe_target(fd: i32) -> io::Result<PathBuf> {
    fs::read_link(format!("/proc/self/fd/{fd}"))
}

fn output_pipe_targets(child: &Child) -> io::Result<Vec<PathBuf>> {
    let mut pipe_targets = Vec::new();
    if let Some(stdout) = child.stdout.as_ref() {
        pipe_targets.push(pipe_target(stdout.as_raw_fd())?);
    }
    if let Some(stderr) = child.stderr.as_ref() {
        pipe_targets.push(pipe_target(stderr.as_raw_fd())?);
    }
    Ok(pipe_targets)
}

fn process_holds_writable_pipe(
    process: &ProcessHandle,
    pipe_targets: &[PathBuf],
) -> io::Result<bool> {
    if !process.is_running()? {
        return Ok(false);
    }
    let fd_directory = PathBuf::from(format!("/proc/{}/fd", process.identity.pid));
    let entries = match fs::read_dir(&fd_directory) {
        Ok(entries) => entries,
        Err(error) if process_scan_error(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) if process_scan_error(&error) => continue,
            Err(error) => return Err(error),
        };
        let target = match fs::read_link(entry.path()) {
            Ok(target) => target,
            Err(error) if process_scan_error(&error) => continue,
            Err(error) => return Err(error),
        };
        if pipe_targets.contains(&target) {
            let writable = match fd_is_writable(process, &entry) {
                Ok(writable) => writable,
                Err(error) if process_scan_error(&error) => continue,
                Err(error) => return Err(error),
            };
            if writable {
                return process.is_running();
            }
        }
    }
    Ok(false)
}

fn fd_is_writable(process: &ProcessHandle, entry: &fs::DirEntry) -> io::Result<bool> {
    let fd_name = entry.file_name();
    let fdinfo = fs::read_to_string(format!(
        "/proc/{}/fdinfo/{}",
        process.identity.pid,
        fd_name.to_string_lossy()
    ))?;
    let flags = fdinfo
        .lines()
        .find_map(|line| line.strip_prefix("flags:"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Linux proc fdinfo missing flags",
            )
        })?
        .trim();
    let flags = u32::from_str_radix(flags, 8).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Linux proc fdinfo flags were not octal",
        )
    })?;
    Ok(flags & libc::O_ACCMODE as u32 != libc::O_RDONLY as u32)
}

fn capture_live_children(parent: ProcessIdentity) -> io::Result<Vec<ProcessHandle>> {
    if !parent.is_running()? {
        return Ok(Vec::new());
    }
    let child_pids = child_pids(parent.pid)?;

    let mut children = Vec::new();
    for child_pid in child_pids {
        if let Some(child) = ProcessHandle::capture_child(parent, child_pid)?
            && child.is_running()?
        {
            children.push(child);
        }
    }
    Ok(children)
}

fn child_pids(parent_pid: i32) -> io::Result<Vec<i32>> {
    let task_directory = format!("/proc/{parent_pid}/task");
    let tasks = match fs::read_dir(task_directory) {
        Ok(tasks) => tasks,
        Err(error) if process_is_gone(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut child_pids = HashSet::new();
    for task in tasks {
        let task = match task {
            Ok(task) => task,
            Err(error) if process_is_gone(&error) => continue,
            Err(error) => return Err(error),
        };
        let children = match fs::read_to_string(task.path().join("children")) {
            Ok(children) => children,
            Err(error) if process_is_gone(&error) => continue,
            Err(error) => return Err(error),
        };
        for pid in children.split_whitespace() {
            let pid = pid.parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Linux proc children entry was not a PID",
                )
            })?;
            child_pids.insert(pid);
        }
    }
    Ok(child_pids.into_iter().collect())
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

fn process_scan_error(error: &io::Error) -> bool {
    process_is_gone(error)
        || error.kind() == io::ErrorKind::PermissionDenied
        || error.kind() == io::ErrorKind::InvalidData
}

fn collect_bounded_output(child: &mut Child) -> io::Result<Output> {
    let stdout = spawn_pipe_reader(child.stdout.take());
    let stderr = spawn_pipe_reader(child.stderr.take());
    let status = child.try_wait()?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "owned gate child was not reaped before output collection",
        )
    })?;
    let deadline = Instant::now() + OUTPUT_DRAIN_TIMEOUT;
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
