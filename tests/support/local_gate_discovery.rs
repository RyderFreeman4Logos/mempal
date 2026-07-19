use super::*;
use std::collections::HashSet;

const MONITOR_DISCOVERY_BUDGET: Duration = Duration::from_millis(10);
const MONITOR_POLL_INTERVAL: Duration = Duration::from_millis(2);
const MONITOR_START_TIMEOUT: Duration = Duration::from_millis(50);

pub(super) struct DescendantMonitor {
    root: ProcessIdentity,
    stop_sender: mpsc::Sender<()>,
    discovered_receiver: mpsc::Receiver<ProcessHandle>,
    finished_receiver: mpsc::Receiver<()>,
    worker: Option<thread::JoinHandle<()>>,
}

impl DescendantMonitor {
    pub(super) fn spawn(root: ProcessIdentity) -> io::Result<Self> {
        let (stop_sender, stop_receiver) = mpsc::channel();
        let (discovered_sender, discovered_receiver) = mpsc::channel();
        let (finished_sender, finished_receiver) = mpsc::channel();
        let (started_sender, started_receiver) = mpsc::channel();
        let worker = thread::Builder::new()
            .name("local-gate-descendant-monitor".to_owned())
            .spawn(move || {
                let _ = started_sender.send(());
                let mut reported = HashSet::new();
                'monitor: loop {
                    let deadline = Instant::now() + MONITOR_DISCOVERY_BUDGET;
                    if let Ok(descendants) = discover_live_descendants(root, deadline) {
                        for process in descendants {
                            if Instant::now() >= deadline {
                                break;
                            }
                            if reported.insert(process.identity)
                                && discovered_sender.send(process).is_err()
                            {
                                break 'monitor;
                            }
                        }
                    }
                    if !root.is_running().unwrap_or(false) {
                        break;
                    }
                    match stop_receiver.recv_timeout(MONITOR_POLL_INTERVAL) {
                        Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                    }
                }
                let _ = finished_sender.send(());
            })?;
        started_receiver
            .recv_timeout(MONITOR_START_TIMEOUT)
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("local gate descendant monitor did not start: {error}"),
                )
            })?;
        Ok(Self {
            root,
            stop_sender,
            discovered_receiver,
            finished_receiver,
            worker: Some(worker),
        })
    }

    pub(super) fn drain_discovered(
        &mut self,
        tracked_processes: &mut Vec<TrackedProcess>,
        deadline: Instant,
    ) {
        while Instant::now() < deadline {
            let Ok(process) = self.discovered_receiver.try_recv() else {
                break;
            };
            if !tracked_processes
                .iter()
                .any(|tracked| tracked.identity() == process.identity)
            {
                tracked_processes.push(TrackedProcess::descendant(process));
            }
        }
    }

    pub(super) fn stop_and_drain(
        &mut self,
        tracked_processes: &mut Vec<TrackedProcess>,
        deadline: Instant,
    ) {
        if let Ok(descendants) = discover_live_descendants(
            self.root,
            deadline.min(Instant::now() + MONITOR_DISCOVERY_BUDGET),
        ) {
            for process in descendants {
                if Instant::now() >= deadline {
                    break;
                }
                if !tracked_processes
                    .iter()
                    .any(|tracked| tracked.identity() == process.identity)
                {
                    tracked_processes.push(TrackedProcess::descendant(process));
                }
            }
        }
        let _ = self.stop_sender.send(());
        loop {
            if Instant::now() >= deadline {
                let _ = self.worker.take();
                return;
            }
            self.drain_discovered(tracked_processes, deadline);
            if self.worker.is_none() {
                return;
            }
            match self.finished_receiver.try_recv() {
                Ok(()) | Err(mpsc::TryRecvError::Disconnected) => {
                    if let Some(worker) = self.worker.take() {
                        let _ = worker.join();
                    }
                    self.drain_discovered(tracked_processes, deadline);
                    return;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let _ = self.worker.take();
                return;
            }
            let _ = self
                .finished_receiver
                .recv_timeout(remaining.min(MONITOR_POLL_INTERVAL));
        }
    }
}

pub(super) fn refresh_after_leader_reap(
    child: &mut Child,
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
    deadline: Instant,
) -> io::Result<bool> {
    let reaped = child.try_wait()?.is_some();
    if reaped {
        refresh_owned_processes(child, root, tracked_processes, deadline)?;
    }
    Ok(reaped)
}

pub(super) fn refresh_owned_processes(
    child: &Child,
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
    deadline: Instant,
) -> io::Result<()> {
    track_output_pipe_holders(child, tracked_processes, deadline)?;
    let mut pending = Vec::new();
    for process in tracked_processes.iter() {
        if Instant::now() >= deadline {
            return Ok(());
        }
        if process.requires_cleanup(deadline)? {
            pending.push(process.identity());
        }
    }
    if Instant::now() < deadline && root.is_running()? {
        pending.push(root.identity);
    }

    let mut visited = HashSet::new();
    while let Some(identity) = pending.pop() {
        if Instant::now() >= deadline {
            return Ok(());
        }
        if !visited.insert(identity) || !identity.is_running()? {
            continue;
        }
        for descendant in capture_live_children(identity, deadline)? {
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

fn discover_live_descendants(
    root: ProcessIdentity,
    deadline: Instant,
) -> io::Result<Vec<ProcessHandle>> {
    let mut discovered = Vec::new();
    let mut pending = vec![root];
    let mut visited = HashSet::new();
    while let Some(identity) = pending.pop() {
        if Instant::now() >= deadline {
            return Ok(discovered);
        }
        if !visited.insert(identity) || !identity.is_running()? {
            continue;
        }
        for descendant in capture_live_children(identity, deadline)? {
            pending.push(descendant.identity);
            discovered.push(descendant);
        }
    }
    Ok(discovered)
}

fn track_output_pipe_holders(
    child: &Child,
    tracked_processes: &mut Vec<TrackedProcess>,
    deadline: Instant,
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
        if Instant::now() >= deadline {
            return Ok(());
        }
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
        let holds_pipe = match process_holds_writable_pipe(&process, &pipe_targets, deadline) {
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

pub(super) fn output_pipe_targets(child: &Child) -> io::Result<Vec<PathBuf>> {
    let mut pipe_targets = Vec::new();
    if let Some(stdout) = child.stdout.as_ref() {
        pipe_targets.push(pipe_target(stdout.as_raw_fd())?);
    }
    if let Some(stderr) = child.stderr.as_ref() {
        pipe_targets.push(pipe_target(stderr.as_raw_fd())?);
    }
    Ok(pipe_targets)
}

pub(super) fn process_holds_writable_pipe(
    process: &ProcessHandle,
    pipe_targets: &[PathBuf],
    deadline: Instant,
) -> io::Result<bool> {
    if Instant::now() >= deadline || !process.is_running()? {
        return Ok(false);
    }
    let fd_directory = PathBuf::from(format!("/proc/{}/fd", process.identity.pid));
    let entries = match fs::read_dir(&fd_directory) {
        Ok(entries) => entries,
        Err(error) if process_scan_error(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    for entry in entries {
        if Instant::now() >= deadline {
            return Ok(false);
        }
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

pub(super) fn capture_live_children(
    parent: ProcessIdentity,
    deadline: Instant,
) -> io::Result<Vec<ProcessHandle>> {
    if Instant::now() >= deadline || !parent.is_running()? {
        return Ok(Vec::new());
    }
    let child_pids = child_pids(parent.pid, deadline)?;

    let mut children = Vec::new();
    for child_pid in child_pids {
        if Instant::now() >= deadline {
            return Ok(children);
        }
        if let Some(child) = ProcessHandle::capture_child(parent, child_pid)?
            && child.is_running()?
        {
            children.push(child);
        }
    }
    Ok(children)
}

fn child_pids(parent_pid: i32, deadline: Instant) -> io::Result<Vec<i32>> {
    let task_directory = format!("/proc/{parent_pid}/task");
    let tasks = match fs::read_dir(task_directory) {
        Ok(tasks) => tasks,
        Err(error) if process_is_gone(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut child_pids = HashSet::new();
    for task in tasks {
        if Instant::now() >= deadline {
            return Ok(child_pids.into_iter().collect());
        }
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
            if Instant::now() >= deadline {
                return Ok(child_pids.into_iter().collect());
            }
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

fn process_scan_error(error: &io::Error) -> bool {
    process_is_gone(error)
        || error.kind() == io::ErrorKind::PermissionDenied
        || error.kind() == io::ErrorKind::InvalidData
}
