use std::collections::HashSet;
use std::fs;
use std::io;
use std::io::Read;
use std::os::fd::AsRawFd;
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
        Ok(inspect_process(self.pid)?
            .is_some_and(|process| process.identity == self && process.state != 'Z'))
    }
}

struct ProcessSnapshot {
    identity: ProcessIdentity,
    state: char,
}

pub(crate) struct GateChild {
    child: Option<Child>,
    tracked_processes: Vec<ProcessIdentity>,
}

pub(crate) fn spawn_in_own_session(command: &mut Command) -> io::Result<Child> {
    // SAFETY: The post-fork closure invokes only async-signal-safe `setsid` before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

impl GateChild {
    pub(crate) fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            tracked_processes: Vec::new(),
        }
    }

    pub(crate) fn wait_with_timeout(&mut self, timeout: Duration) -> io::Result<Output> {
        let deadline = Instant::now() + timeout;
        loop {
            self.refresh_tracked_processes()?;
            let exited = self
                .child
                .as_mut()
                .expect("gate child already reaped")
                .try_wait()?
                .is_some();
            if exited {
                return terminate_and_collect(
                    self.child.take().expect("gate child already reaped"),
                    &mut self.tracked_processes,
                );
            }
            if Instant::now() >= deadline {
                return timeout_error(
                    timeout,
                    terminate_and_collect(
                        self.child.take().expect("gate child already reaped"),
                        &mut self.tracked_processes,
                    ),
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn refresh_tracked_processes(&mut self) -> io::Result<()> {
        let child = self.child.as_ref().expect("gate child already reaped");
        refresh_owned_processes(child, &mut self.tracked_processes)
    }
}

impl Drop for GateChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let _ = terminate_and_collect(child, &mut self.tracked_processes);
        }
    }
}

pub(crate) fn reap_owned_child(child: Child) -> io::Result<()> {
    let mut tracked_processes = Vec::new();
    terminate_and_collect(child, &mut tracked_processes).map(|_| ())
}

fn timeout_error(timeout: Duration, cleanup: io::Result<Output>) -> io::Result<Output> {
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

fn terminate_and_collect(
    mut child: Child,
    tracked_processes: &mut Vec<ProcessIdentity>,
) -> io::Result<Output> {
    terminate_owned_process_tree(&mut child, tracked_processes)?;
    collect_bounded_output(&mut child)
}

fn terminate_owned_process_tree(
    child: &mut Child,
    tracked_processes: &mut Vec<ProcessIdentity>,
) -> io::Result<()> {
    refresh_owned_processes(child, tracked_processes)?;
    let process_group_id = owned_child_pid(child)?;
    let mut child_exited = child.try_wait()?.is_some();
    if !child_exited {
        signal_process_group(process_group_id, libc::SIGTERM)?;
    }
    signal_tracked_processes(tracked_processes, libc::SIGTERM)?;
    if !child_exited {
        child_exited = wait_for_child_exit(child, PROCESS_GROUP_TERM_TIMEOUT)?;
    }
    if child_exited
        && wait_for_tracked_processes_exit(tracked_processes, PROCESS_GROUP_TERM_TIMEOUT)?
    {
        return Ok(());
    }

    refresh_owned_processes(child, tracked_processes)?;
    if !child_exited {
        signal_process_group(process_group_id, libc::SIGKILL)?;
    }
    signal_tracked_processes(tracked_processes, libc::SIGKILL)?;
    if !child_exited {
        child_exited = wait_for_child_exit(child, PROCESS_GROUP_KILL_TIMEOUT)?;
    }
    if child_exited
        && wait_for_tracked_processes_exit(tracked_processes, PROCESS_GROUP_KILL_TIMEOUT)?
    {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "owned gate process tree did not exit after SIGKILL",
    ))
}

fn owned_child_pid(child: &Child) -> io::Result<i32> {
    i32::try_from(child.id()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "owned gate child PID exceeds i32 range",
        )
    })
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

fn signal_tracked_processes(processes: &[ProcessIdentity], signal: i32) -> io::Result<()> {
    for process in processes {
        if !process.is_running()? {
            continue;
        }
        // SAFETY: `is_running` validated this PID with its Linux start-time tick before the
        // signal. A later PID reuse can only occur after the original process has exited.
        unsafe {
            if libc::kill(process.pid, signal) == -1 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        }
    }
    Ok(())
}

fn wait_for_tracked_processes_exit(
    processes: &[ProcessIdentity],
    timeout: Duration,
) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut any_running = false;
        for process in processes {
            if process.is_running()? {
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

fn refresh_owned_processes(
    child: &Child,
    tracked_processes: &mut Vec<ProcessIdentity>,
) -> io::Result<()> {
    track_output_pipe_holders(child, tracked_processes)?;
    let mut pending = Vec::new();
    for process in tracked_processes.iter().copied() {
        if process.is_running()? {
            pending.push(process);
        }
    }
    if let Some(root) = inspect_process(owned_child_pid(child)?)? {
        pending.push(root.identity);
    }

    let mut visited = HashSet::new();
    while let Some(process) = pending.pop() {
        if !visited.insert(process) || !process.is_running()? {
            continue;
        }
        if !tracked_processes.contains(&process) {
            tracked_processes.push(process);
        }
        for child_pid in child_pids(process.pid)? {
            if let Some(descendant) = inspect_process(child_pid)? {
                pending.push(descendant.identity);
            }
        }
    }
    Ok(())
}

fn track_output_pipe_holders(
    child: &Child,
    tracked_processes: &mut Vec<ProcessIdentity>,
) -> io::Result<()> {
    let mut pipe_targets = Vec::new();
    if let Some(stdout) = child.stdout.as_ref() {
        pipe_targets.push(pipe_target(stdout.as_raw_fd())?);
    }
    if let Some(stderr) = child.stderr.as_ref() {
        pipe_targets.push(pipe_target(stderr.as_raw_fd())?);
    }
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
        if pid == current_pid || !process_holds_pipe(&entry.path().join("fd"), &pipe_targets)? {
            continue;
        }
        if let Some(process) = inspect_process(pid)?
            && !tracked_processes.contains(&process.identity)
        {
            tracked_processes.push(process.identity);
        }
    }
    Ok(())
}

fn pipe_target(fd: i32) -> io::Result<PathBuf> {
    fs::read_link(format!("/proc/self/fd/{fd}"))
}

fn process_holds_pipe(
    fd_directory: &std::path::Path,
    pipe_targets: &[PathBuf],
) -> io::Result<bool> {
    let entries = match fs::read_dir(fd_directory) {
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
            return Ok(true);
        }
    }
    Ok(false)
}

fn child_pids(parent_pid: i32) -> io::Result<Vec<i32>> {
    let path = format!("/proc/{parent_pid}/task/{parent_pid}/children");
    let children = match fs::read_to_string(path) {
        Ok(children) => children,
        Err(error) if process_is_gone(&error) => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    children
        .split_whitespace()
        .map(|pid| {
            pid.parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "Linux proc children entry was not a PID",
                )
            })
        })
        .collect()
}

fn inspect_process(pid: i32) -> io::Result<Option<ProcessSnapshot>> {
    let stat = match fs::read_to_string(format!("/proc/{pid}/stat")) {
        Ok(stat) => stat,
        Err(error) if process_is_gone(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    let (_, fields) = stat.rsplit_once(')').ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "Linux proc stat was missing the command name terminator",
        )
    })?;
    let mut fields = fields.split_whitespace();
    let state = fields
        .next()
        .and_then(|state| state.chars().next())
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "Linux proc stat missing state")
        })?;
    let start_time_ticks = fields
        .nth(18)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "Linux proc stat missing start time",
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
        state,
    }))
}

fn process_is_gone(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn process_scan_error(error: &io::Error) -> bool {
    process_is_gone(error) || error.kind() == io::ErrorKind::PermissionDenied
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn wait_for_file(path: &Path, timeout: Duration, description: &str) {
        let deadline = Instant::now() + timeout;
        while !path.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(path.exists(), "{description} did not become ready");
    }

    fn spawn_pipe_holding_child(ready_file: &Path, pid_file: &Path) -> Child {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    : >"${READY_FILE:?}"
                    printf '%s\n' "${BASHPID}" >"${PID_FILE:?}"
                    ( trap '' TERM; exec /bin/sleep 60 ) &
                    while true; do sleep 0.01; done
                "#,
            ])
            .env("READY_FILE", ready_file)
            .env("PID_FILE", pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn isolated fixture")
    }

    fn spawn_setsid_pipe_holding_child(
        ready_file: &Path,
        pid_file: &Path,
        exit_after_ready: bool,
    ) -> Child {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    setsid /bin/bash -c '
                        trap "" TERM
                        pid="${BASHPID}"
                        start_time="$(awk "{print \$22}" "/proc/${pid}/stat")"
                        printf "%s %s\n" "${pid}" "${start_time}" >"${PID_FILE:?}"
                        : >"${READY_FILE:?}"
                        while true; do
                            printf "escaped stdout\n"
                            printf "escaped stderr\n" >&2
                            /bin/sleep 60
                        done
                    ' &
                    if [[ "${EXIT_AFTER_READY:?}" == "1" ]]; then
                        while [[ ! -e "${READY_FILE:?}" ]]; do /bin/sleep 0.01; done
                        exit 0
                    fi
                    while true; do /bin/sleep 0.01; done
                "#,
            ])
            .env("READY_FILE", ready_file)
            .env("PID_FILE", pid_file)
            .env("EXIT_AFTER_READY", if exit_after_ready { "1" } else { "0" })
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn isolated fixture")
    }

    fn process_identity_from_pid_file(pid_file: &Path) -> ProcessIdentity {
        let pid_file = fs::read_to_string(pid_file).expect("read escaped process identity");
        let mut fields = pid_file.split_whitespace();
        let pid = fields
            .next()
            .expect("escaped process PID")
            .parse()
            .expect("numeric escaped process PID");
        let start_time_ticks = fields
            .next()
            .expect("escaped process start time")
            .parse()
            .expect("numeric escaped process start time");
        assert!(
            fields.next().is_none(),
            "unexpected escaped process identity"
        );
        ProcessIdentity {
            pid,
            start_time_ticks,
        }
    }

    fn assert_process_identity_gone(identity: ProcessIdentity) {
        assert!(
            !identity
                .is_running()
                .expect("inspect escaped process identity"),
            "escaped process remained alive: {identity:?}"
        );
    }

    #[test]
    fn procfs_exit_errors_are_treated_as_absent_processes() {
        assert!(process_is_gone(&io::Error::from(io::ErrorKind::NotFound)));
        assert!(process_is_gone(&io::Error::from_raw_os_error(libc::ESRCH)));
    }

    #[test]
    fn gate_child_wait_timeout_reaps_descendants_that_hold_pipes() {
        let fixture = tempfile::tempdir().expect("create pipe-holder fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut gate = GateChild::new(spawn_pipe_holding_child(&ready_file, &pid_file));
        wait_for_file(&ready_file, Duration::from_secs(2), "pipe-holder fixture");

        let started = Instant::now();
        let error = gate
            .wait_with_timeout(Duration::from_millis(50))
            .expect_err("a gate timeout must return an error");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GateChild timeout cleanup must not hang on inherited pipes"
        );
    }

    #[test]
    fn gate_child_drop_reaps_descendants_that_hold_pipes() {
        let fixture = tempfile::tempdir().expect("create drop pipe-holder fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let started = Instant::now();
        {
            let gate = GateChild::new(spawn_pipe_holding_child(&ready_file, &pid_file));
            wait_for_file(&ready_file, Duration::from_secs(2), "pipe-holder fixture");
            drop(gate);
        }

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GateChild Drop cleanup must not hang on inherited pipes"
        );
    }

    #[test]
    fn gate_child_timeout_terminates_setsid_escape_without_hanging() {
        let fixture = tempfile::tempdir().expect("create setsid pipe-holder fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut gate = GateChild::new(spawn_setsid_pipe_holding_child(
            &ready_file,
            &pid_file,
            false,
        ));
        wait_for_file(
            &ready_file,
            Duration::from_secs(2),
            "setsid pipe-holder fixture",
        );
        let escaped = process_identity_from_pid_file(&pid_file);

        let started = Instant::now();
        let error = gate
            .wait_with_timeout(Duration::from_millis(50))
            .expect_err("a gate timeout must return an error");

        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GateChild timeout cleanup must not hang on setsid descendants"
        );
        assert_process_identity_gone(escaped);
    }

    #[test]
    fn gate_child_drop_terminates_setsid_escape_without_hanging() {
        let fixture = tempfile::tempdir().expect("create setsid drop fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let escaped;
        let started = Instant::now();
        {
            let gate = GateChild::new(spawn_setsid_pipe_holding_child(
                &ready_file,
                &pid_file,
                false,
            ));
            wait_for_file(
                &ready_file,
                Duration::from_secs(2),
                "setsid pipe-holder fixture",
            );
            escaped = process_identity_from_pid_file(&pid_file);
            drop(gate);
        }

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GateChild Drop cleanup must not hang on setsid descendants"
        );
        assert_process_identity_gone(escaped);
    }

    #[test]
    fn gate_child_reaps_setsid_escape_after_leader_exits() {
        let fixture = tempfile::tempdir().expect("create exited-leader fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut child = spawn_setsid_pipe_holding_child(&ready_file, &pid_file, true);
        wait_for_file(
            &ready_file,
            Duration::from_secs(2),
            "setsid pipe-holder fixture",
        );
        let escaped = process_identity_from_pid_file(&pid_file);
        assert!(
            wait_for_child_exit(&mut child, Duration::from_secs(2))
                .expect("wait for exited leader"),
            "the fixture leader did not exit"
        );

        let started = Instant::now();
        let mut tracked_processes = Vec::new();
        let output = terminate_and_collect(child, &mut tracked_processes)
            .expect("reap exited leader and escaped descendant");

        assert!(
            output.status.success(),
            "fixture leader status: {}",
            output.status
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cleanup must not depend on the escaped process retaining its parent"
        );
        assert_process_identity_gone(escaped);
    }

    #[test]
    fn rest_gate_fuser_diagnostic_cannot_outlive_lock_budget() {
        let fixture = tempfile::tempdir().expect("create fuser-timeout fixture");
        let bin_dir = fixture.path().join("bin");
        fs::create_dir(&bin_dir).expect("create fixture bin directory");
        symlink(
            repo_root().join("tests/fixtures/local-gate-command-proxy.sh"),
            bin_dir.join("fuser"),
        )
        .expect("link fuser proxy");
        let target = fixture.path().join("target");
        fs::create_dir(&target).expect("create isolated target");
        let target = fs::canonicalize(target).expect("canonical isolated target");
        let mut lock_file = target.as_os_str().to_os_string();
        lock_file.push(".lock");
        let lock_file = PathBuf::from(lock_file);
        let holder_ready_file = fixture.path().join("holder-ready");
        let mut holder_command = Command::new("/bin/bash");
        holder_command
            .args([
                "-c",
                r#"
                    exec {lock_fd}>"${LOCK_FILE:?}"
                    flock "${lock_fd}"
                    : >"${HOLDER_READY_FILE:?}"
                    exec /bin/sleep 60
                "#,
            ])
            .env("LOCK_FILE", &lock_file)
            .env("HOLDER_READY_FILE", &holder_ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _holder = GateChild::new(
            spawn_in_own_session(&mut holder_command).expect("spawn isolated lock holder"),
        );
        wait_for_file(&holder_ready_file, Duration::from_secs(2), "lock holder");

        let inherited_path = std::env::var_os("PATH").expect("PATH is set");
        let path = std::env::join_paths(
            std::iter::once(bin_dir).chain(std::env::split_paths(&inherited_path)),
        )
        .expect("construct fixture PATH");
        let fuser_ready_file = fixture.path().join("fuser-ready");
        let fuser_pid_file = fixture.path().join("fuser.pid");
        let mut command = Command::new("/bin/bash");
        command
            .arg(repo_root().join("scripts/gates/rest-tests.sh"))
            .current_dir(repo_root())
            .env("PATH", path)
            .env("REST_GATE_DRY_RUN", "1")
            .env("REST_GATE_LOCK_TIMEOUT_SECS", "1")
            .env("REST_GATE_TARGET_DIR", &target)
            .env("REST_TEST_TARGETS_PER_BATCH", "999")
            .env("REST_GATE_FUSER_NEVER_RETURN_READY_FILE", &fuser_ready_file)
            .env("REST_GATE_FUSER_PID_FILE", &fuser_pid_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut gate =
            GateChild::new(spawn_in_own_session(&mut command).expect("spawn isolated REST gate"));
        wait_for_file(&fuser_ready_file, Duration::from_secs(2), "stalled fuser");

        let started = Instant::now();
        let output = gate
            .wait_with_timeout(Duration::from_secs(3))
            .expect("reap REST gate");

        assert_eq!(output.status.code(), Some(75));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the fuser diagnostic exceeded the advertised lock deadline: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
