use std::collections::{HashMap, HashSet};
use std::io;
use std::os::unix::process::ExitStatusExt;
use std::process::{Child, ExitStatus, Output};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use super::{
    CleanupDiagnostics, DescendantMonitor, OwnedGateChild, ProcessHandle, ProcessIdentity,
    TERMINATION_DISCOVERY_POLL_INTERVAL, TrackedProcess, capture_children, capture_live_children,
    inspect_process, terminate_and_collect_until,
};

/// A gate timeout may spend this bounded interval terminating and draining its process tree.
pub(super) const CLEANUP_TIMEOUT_MARGIN: Duration = Duration::from_millis(250);
const OUTPUT_DRAIN_RESERVE: Duration = Duration::from_millis(50);
const WAIT_NOTHREAD: i32 = 0x2000_0000;

#[derive(Default)]
struct GateChildRegistry {
    active_pids: HashSet<i32>,
    reaped_statuses: HashMap<i32, ExitStatus>,
}

fn gate_child_registry() -> &'static Mutex<GateChildRegistry> {
    static REGISTRY: OnceLock<Mutex<GateChildRegistry>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(GateChildRegistry::default()))
}

pub(super) fn subreaper_operation_guard() -> std::sync::MutexGuard<'static, ()> {
    static OPERATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    OPERATION_LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn with_gate_child_registry<T>(f: impl FnOnce(&mut GateChildRegistry) -> T) -> T {
    let mut registry = gate_child_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    f(&mut registry)
}

pub(super) fn register_gate_child(pid: i32) {
    with_gate_child_registry(|registry| {
        registry.active_pids.insert(pid);
    });
}

pub(super) fn unregister_gate_child(pid: i32) {
    with_gate_child_registry(|registry| {
        registry.active_pids.remove(&pid);
        registry.reaped_statuses.remove(&pid);
    });
}

fn gate_child_is_registered(pid: i32) -> bool {
    with_gate_child_registry(|registry| registry.active_pids.contains(&pid))
}

pub(super) fn gate_child_has_reaped_status(pid: i32) -> bool {
    with_gate_child_registry(|registry| registry.reaped_statuses.contains_key(&pid))
}

fn retain_reaped_gate_child_status(pid: i32, status: i32) {
    with_gate_child_registry(|registry| {
        if registry.active_pids.contains(&pid) {
            registry
                .reaped_statuses
                .insert(pid, ExitStatus::from_raw(status));
        }
    });
}

pub(super) fn try_wait_child(child: &mut Child) -> io::Result<Option<ExitStatus>> {
    let pid = i32::try_from(child.id()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "owned gate child PID exceeds i32 range",
        )
    })?;
    if let Some(status) = with_gate_child_registry(|registry| registry.reaped_statuses.remove(&pid))
    {
        return Ok(Some(status));
    }
    match child.try_wait() {
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
            Ok(with_gate_child_registry(|registry| {
                registry.reaped_statuses.remove(&pid)
            }))
        }
        result => result,
    }
}

pub(super) fn ensure_child_subreaper() -> io::Result<()> {
    static SUBREAPER_RESULT: OnceLock<Result<(), i32>> = OnceLock::new();
    match SUBREAPER_RESULT.get_or_init(|| {
        // SAFETY: `prctl` receives the documented integer-only PR_SET_CHILD_SUBREAPER
        // arguments and does not retain pointers. It must run before the gate child forks.
        if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } == -1 {
            Err(io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EIO))
        } else {
            Ok(())
        }
    }) {
        Ok(()) => Ok(()),
        Err(errno) => Err(io::Error::from_raw_os_error(*errno)),
    }
}

pub(super) fn capture_subreaper_baseline(
    root_pid: i32,
    deadline: Instant,
) -> io::Result<HashSet<ProcessIdentity>> {
    let parent_pid = i32::try_from(std::process::id()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "test process PID exceeds i32 range",
        )
    })?;
    let parent = ProcessHandle::capture(parent_pid)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "child subreaper process disappeared from procfs",
        )
    })?;
    Ok(capture_live_children(parent.identity, deadline)?
        .into_iter()
        .filter(|child| child.identity.pid != root_pid)
        .map(|child| child.identity)
        .collect())
}

pub(super) fn cleanup_deadline() -> Instant {
    Instant::now() + CLEANUP_TIMEOUT_MARGIN
}

/// Leaves a bounded output-drain reserve while every discovery pass shares one absolute deadline.
pub(super) fn discovery_deadline(cleanup_deadline: Instant) -> Instant {
    Instant::now()
        + cleanup_deadline
            .saturating_duration_since(Instant::now())
            .saturating_sub(OUTPUT_DRAIN_RESERVE)
}

pub(super) fn terminate_and_collect(
    child: &mut OwnedGateChild,
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
) -> io::Result<Output> {
    let deadline = cleanup_deadline();
    let mut descendant_monitor = DescendantMonitor::spawn(root.identity)?;
    let result = terminate_and_collect_until(
        child,
        root,
        tracked_processes,
        &mut descendant_monitor,
        deadline,
    );
    descendant_monitor.stop_and_drain(tracked_processes, deadline);
    result
}

pub(super) struct SubreaperContext {
    parent: ProcessIdentity,
    parent_session_id: i32,
    baseline_children: HashSet<ProcessIdentity>,
    reparented_children: Vec<ProcessIdentity>,
}

impl SubreaperContext {
    pub(super) fn capture(
        root_pid: i32,
        baseline_children: Option<&HashSet<ProcessIdentity>>,
        deadline: Instant,
    ) -> io::Result<Self> {
        let parent_pid = i32::try_from(std::process::id()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "test process PID exceeds i32 range",
            )
        })?;
        let parent_snapshot = inspect_process(parent_pid)?.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "child subreaper process disappeared from procfs",
            )
        })?;
        let baseline_children = match baseline_children {
            Some(children) => children.clone(),
            None => capture_subreaper_baseline(root_pid, deadline)?,
        };
        Ok(Self {
            parent: parent_snapshot.identity,
            parent_session_id: parent_snapshot.session_id,
            baseline_children,
            reparented_children: Vec::new(),
        })
    }

    fn discover_reparented_children(
        &mut self,
        root: ProcessIdentity,
        tracked_processes: &mut Vec<TrackedProcess>,
        deadline: Instant,
    ) -> io::Result<usize> {
        let mut discovered = 0;
        for child in capture_children(self.parent, deadline)? {
            if Instant::now() >= deadline
                || child.identity == root
                || self.baseline_children.contains(&child.identity)
                || gate_child_is_registered(child.identity.pid)
            {
                continue;
            }
            let Some(snapshot) = inspect_process(child.identity.pid)? else {
                continue;
            };
            // Raw `Command` children inherit the test runner's session. Gate descendants run
            // in the gate's isolated session (or a newer setsid session), so this prevents a
            // parallel same-process test command from being mistaken for an adopted escape.
            if snapshot.identity != child.identity || snapshot.session_id == self.parent_session_id
            {
                continue;
            }
            if !self.reparented_children.contains(&child.identity) {
                self.reparented_children.push(child.identity);
                discovered += 1;
            }
            if !tracked_processes
                .iter()
                .any(|tracked| tracked.identity() == child.identity)
            {
                tracked_processes.push(TrackedProcess::descendant(child));
            }
        }
        Ok(discovered)
    }

    fn reap_waitable_children(&mut self) -> io::Result<usize> {
        let mut reaped = 0;
        loop {
            let mut status = 0;
            // SAFETY: `waitpid` receives a valid status pointer. WNOHANG bounds the call, while
            // Linux __WNOTHREAD avoids consuming children owned by parallel test threads.
            let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG | WAIT_NOTHREAD) };
            if pid == 0 {
                break;
            }
            if pid == -1 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::ECHILD) => break,
                    Some(libc::EINTR) => continue,
                    _ => return Err(error),
                }
            }
            if let Some(index) = self
                .reparented_children
                .iter()
                .position(|identity| identity.pid == pid)
            {
                self.reparented_children.swap_remove(index);
            } else {
                retain_reaped_gate_child_status(pid, status);
            }
            reaped += 1;
        }

        // A reparented child can be attached to a different task in this thread group. The
        // required waitpid(-1) sweep above remains the primary collection path; targeted waits
        // safely finish any known adopted child that __WNOTHREAD cannot observe.
        let mut index = 0;
        while index < self.reparented_children.len() {
            let mut status = 0;
            let expected_pid = self.reparented_children[index].pid;
            // SAFETY: `expected_pid` was captured from procfs as our direct child and the status
            // pointer is valid for the duration of the nonblocking syscall.
            let pid = unsafe { libc::waitpid(expected_pid, &mut status, libc::WNOHANG) };
            if pid == expected_pid {
                self.reparented_children.swap_remove(index);
                reaped += 1;
                continue;
            }
            if pid == -1 {
                let error = io::Error::last_os_error();
                match error.raw_os_error() {
                    Some(libc::ECHILD) | Some(libc::ESRCH) => {
                        if inspect_process(expected_pid)?.is_none() {
                            self.reparented_children.swap_remove(index);
                            continue;
                        }
                    }
                    Some(libc::EINTR) => continue,
                    _ => return Err(error),
                }
            }
            index += 1;
        }
        Ok(reaped)
    }

    pub(super) fn signal_and_reap_until(
        &mut self,
        root: ProcessIdentity,
        tracked_processes: &mut Vec<TrackedProcess>,
        signal: i32,
        deadline: Instant,
    ) -> io::Result<bool> {
        let _operation_guard = subreaper_operation_guard();
        let mut empty_passes = 0;
        loop {
            let discovered =
                self.discover_reparented_children(root, tracked_processes, deadline)?;
            for identity in &self.reparented_children {
                if let Some(process) = tracked_processes
                    .iter()
                    .find(|tracked| tracked.identity() == *identity)
                {
                    process.send_signal(signal, deadline)?;
                }
            }
            let reaped = self.reap_waitable_children()?;
            self.reparented_children.retain(|identity| {
                inspect_process(identity.pid)
                    .ok()
                    .flatten()
                    .is_some_and(|snapshot| snapshot.identity == *identity)
            });

            if self.reparented_children.is_empty() && discovered == 0 && reaped == 0 {
                empty_passes += 1;
                if empty_passes >= 2 {
                    return Ok(true);
                }
            } else {
                empty_passes = 0;
            }
            if Instant::now() >= deadline {
                return Ok(self.reparented_children.is_empty());
            }
            thread::sleep(
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(TERMINATION_DISCOVERY_POLL_INTERVAL),
            );
        }
    }
}

pub(super) fn refresh_reparented_children_after_leader_reap(
    root: &ProcessHandle,
    tracked_processes: &mut Vec<TrackedProcess>,
    deadline: Instant,
) -> io::Result<()> {
    let _operation_guard = subreaper_operation_guard();
    let baseline = HashSet::new();
    let mut context = SubreaperContext::capture(root.identity.pid, Some(&baseline), deadline)?;
    context.discover_reparented_children(root.identity, tracked_processes, deadline)?;
    Ok(())
}

pub(super) fn signal_process_group(process_group_id: i32, signal: i32) -> io::Result<()> {
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

pub(super) fn signal_root_process_tree(
    root: &ProcessHandle,
    leader_alive: bool,
    signal: i32,
) -> io::Result<()> {
    let mut diagnostics = CleanupDiagnostics::default();
    let _ = diagnostics.capture(root.send_signal(signal));
    if leader_alive {
        let _ = diagnostics.capture(signal_process_group(root.identity.pid, signal));
    }
    diagnostics.finish()
}

pub(super) fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> io::Result<bool> {
    wait_for_child_exit_until(child, Instant::now() + timeout)
}

pub(super) fn wait_for_child_exit_until(child: &mut Child, deadline: Instant) -> io::Result<bool> {
    loop {
        if try_wait_child(child)?.is_some() {
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

pub(super) fn process_is_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}
