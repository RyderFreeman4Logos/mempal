use std::collections::BTreeMap;
use std::ffi::{CString, OsStr, OsString};
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::fd_layout::{Pipe, pipe_cloexec, relocate_fd_at_least_three, set_nonblocking};
use super::{ProcessIdentity, SetupStage};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StdioMode {
    /// stdin → /dev/null; capture stdout/stderr.
    Capture,
    /// stdin → pipe; discard stdout/stderr to /dev/null.
    PipedInput,
    /// stdin → pipe; capture stdout/stderr (CLI integration helpers).
    CaptureWithInput,
}

#[derive(Debug)]
pub struct SpawnSpec {
    executable: PathBuf,
    arguments: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    current_dir: PathBuf,
    stdio: StdioMode,
    setup_gate: Option<ChildSetupGate>,
}

impl SpawnSpec {
    pub fn new(executable: impl Into<PathBuf>) -> io::Result<Self> {
        let executable = executable.into();
        if !executable.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "supervised executable must be an absolute path",
            ));
        }
        Ok(Self {
            executable,
            arguments: Vec::new(),
            environment: std::env::vars_os().collect(),
            current_dir: std::env::current_dir()?,
            stdio: StdioMode::Capture,
            setup_gate: None,
        })
    }

    pub fn resolve(program: impl AsRef<OsStr>) -> io::Result<Self> {
        let program = program.as_ref();
        if Path::new(program).is_absolute() {
            return Self::new(PathBuf::from(program));
        }
        let path = std::env::var_os("PATH")
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "PATH is unavailable"))?;
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join(program);
            if candidate.is_file() {
                return Self::new(candidate);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("could not resolve executable {program:?}"),
        ))
    }

    pub fn arg(&mut self, argument: impl Into<OsString>) -> &mut Self {
        self.arguments.push(argument.into());
        self
    }

    pub fn args<I, S>(&mut self, arguments: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.arguments.extend(arguments.into_iter().map(Into::into));
        self
    }

    pub fn env(&mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> &mut Self {
        self.environment.insert(key.into(), value.into());
        self
    }

    pub fn env_clear(&mut self) -> &mut Self {
        self.environment.clear();
        self
    }

    pub fn current_dir(&mut self, directory: impl Into<PathBuf>) -> io::Result<&mut Self> {
        let directory = directory.into();
        if !directory.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "supervised current directory must be absolute",
            ));
        }
        self.current_dir = directory;
        Ok(self)
    }

    pub fn stdio(&mut self, stdio: StdioMode) -> &mut Self {
        self.stdio = stdio;
        self
    }

    pub fn setup_gate(&mut self, gate: ChildSetupGate) -> &mut Self {
        self.setup_gate = Some(gate);
        self
    }
}

#[derive(Debug)]
pub struct TestSetupGate {
    ready_read: OwnedFd,
    release_write: OwnedFd,
}

#[derive(Debug)]
pub struct ChildSetupGate {
    ready_write: OwnedFd,
    release_read: OwnedFd,
    inherited_parent_fds: [RawFd; 2],
}

impl TestSetupGate {
    pub fn new() -> io::Result<(Self, ChildSetupGate)> {
        let (ready_read, ready_write) = pipe_cloexec()?;
        let (release_read, release_write) = pipe_cloexec()?;
        set_nonblocking(ready_read.as_raw_fd())?;
        let inherited_parent_fds = [ready_read.as_raw_fd(), release_write.as_raw_fd()];
        Ok((
            Self {
                ready_read,
                release_write,
            },
            ChildSetupGate {
                ready_write,
                release_read,
                inherited_parent_fds,
            },
        ))
    }

    pub fn wait_ready(&self, deadline: Instant) -> io::Result<libc::pid_t> {
        let mut bytes = [0u8; std::mem::size_of::<libc::pid_t>()];
        let mut filled = 0usize;
        while filled < bytes.len() {
            // SAFETY: the gate retains ownership of ready_read for this method, and the suffix
            // of `bytes` is initialized writable storage with the exact remaining capacity.
            let result = unsafe {
                libc::read(
                    self.ready_read.as_raw_fd(),
                    bytes[filled..].as_mut_ptr().cast(),
                    bytes.len() - filled,
                )
            };
            if result > 0 {
                filled += result as usize;
                continue;
            }
            if result == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "setup gate closed before reporting group readiness",
                ));
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.kind() != io::ErrorKind::WouldBlock {
                return Err(error);
            }
            poll_fd(self.ready_read.as_raw_fd(), libc::POLLIN, deadline)?;
        }
        Ok(libc::pid_t::from_ne_bytes(bytes))
    }

    pub fn release(&self) -> io::Result<()> {
        write_all(self.release_write.as_raw_fd(), &[1])
    }
}

pub(super) struct RawSpawn {
    pub identity: ProcessIdentity,
    pub pidfd: Option<OwnedFd>,
    pub stdin: Option<OwnedFd>,
    pub stdout: Option<OwnedFd>,
    pub stderr: Option<OwnedFd>,
    pub setup_status: OwnedFd,
    pub group_ready: bool,
    pub group_error: Option<io::Error>,
}

struct PreparedSpawn {
    executable: CString,
    _argv: Vec<CString>,
    argv_pointers: Vec<*const libc::c_char>,
    _environment: Vec<CString>,
    environment_pointers: Vec<*const libc::c_char>,
    current_dir: CString,
    stdio: StdioMode,
    child_stdin: RawFd,
    child_stdout: RawFd,
    child_stderr: RawFd,
    dev_null: OwnedFd,
    stdin_pipe: Option<Pipe>,
    stdout_pipe: Option<Pipe>,
    stderr_pipe: Option<Pipe>,
    setup_pipe: Pipe,
    setup_gate: Option<ChildSetupGate>,
    close_fds: Vec<RawFd>,
    failure_record: SetupFailureRecord,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct SetupFailureRecord {
    stage: u8,
    padding: [u8; 3],
    errno: i32,
}

pub(super) const SETUP_RECORD_BYTES: usize = std::mem::size_of::<SetupFailureRecord>();

pub(super) fn decode_setup_record(bytes: [u8; SETUP_RECORD_BYTES]) -> (SetupStage, i32) {
    // SAFETY: the byte array is exactly the repr(C) record size and may lack natural alignment;
    // read_unaligned copies its initialized bytes without creating a misaligned reference.
    let record = unsafe { std::ptr::read_unaligned(bytes.as_ptr().cast::<SetupFailureRecord>()) };
    (SetupStage::from_wire(record.stage), record.errno)
}

pub(super) fn spawn_owned(spec: SpawnSpec) -> io::Result<RawSpawn> {
    let prepared = PreparedSpawn::new(spec)?;
    // SAFETY: the child branch immediately delegates to child_exec, whose post-fork path uses
    // only async-signal-safe libc operations until execve or _exit; the parent retains `prepared`.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        return Err(io::Error::last_os_error());
    }
    if pid == 0 {
        // SAFETY: this is the fork child and `prepared` remains valid in its copied address
        // space until child_exec replaces it with execve or terminates with _exit.
        unsafe { child_exec(&prepared) }
    }

    let group_result = establish_process_group(pid);
    let group_ready = group_result.is_ok();
    let group_error = group_result.err();
    let identity = ProcessIdentity {
        pid,
        start_time_ticks: read_start_time(pid).ok(),
    };
    let pidfd = open_pidfd(pid).ok();

    let PreparedSpawn {
        stdio,
        dev_null,
        stdin_pipe,
        stdout_pipe,
        stderr_pipe,
        setup_pipe,
        setup_gate,
        ..
    } = prepared;
    drop(dev_null);
    drop(setup_gate);
    drop(setup_pipe.write);

    let stdin = match (stdio, stdin_pipe) {
        (StdioMode::PipedInput | StdioMode::CaptureWithInput, Some(pipe)) => {
            drop(pipe.read);
            Some(pipe.write)
        }
        (_, pipe) => {
            drop(pipe);
            None
        }
    };
    let stdout = stdout_pipe.map(|pipe| {
        drop(pipe.write);
        pipe.read
    });
    let stderr = stderr_pipe.map(|pipe| {
        drop(pipe.write);
        pipe.read
    });

    Ok(RawSpawn {
        identity,
        pidfd,
        stdin,
        stdout,
        stderr,
        setup_status: setup_pipe.read,
        group_ready,
        group_error,
    })
}

impl PreparedSpawn {
    fn new(spec: SpawnSpec) -> io::Result<Self> {
        let executable = path_cstring(&spec.executable)?;
        let current_dir = path_cstring(&spec.current_dir)?;
        let mut argv = Vec::with_capacity(spec.arguments.len() + 1);
        argv.push(executable.clone());
        for argument in spec.arguments {
            argv.push(os_cstring(&argument, "argument")?);
        }
        let mut argv_pointers: Vec<_> = argv.iter().map(|value| value.as_ptr()).collect();
        argv_pointers.push(std::ptr::null());

        let mut environment = Vec::with_capacity(spec.environment.len());
        for (key, value) in spec.environment {
            let mut pair = key;
            pair.push("=");
            pair.push(value);
            environment.push(os_cstring(&pair, "environment entry")?);
        }
        let mut environment_pointers: Vec<_> =
            environment.iter().map(|value| value.as_ptr()).collect();
        environment_pointers.push(std::ptr::null());

        let dev_null_file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")?;
        let dev_null: OwnedFd = relocate_fd_at_least_three(dev_null_file.into())?;
        let (stdin_pipe, stdout_pipe, stderr_pipe) = match spec.stdio {
            StdioMode::Capture => (None, Some(Pipe::new()?), Some(Pipe::new()?)),
            StdioMode::PipedInput => (Some(Pipe::new()?), None, None),
            StdioMode::CaptureWithInput => {
                (Some(Pipe::new()?), Some(Pipe::new()?), Some(Pipe::new()?))
            }
        };
        let setup_pipe = Pipe::new()?;

        if let Some(pipe) = &stdin_pipe {
            set_nonblocking(pipe.write.as_raw_fd())?;
        }
        if let Some(pipe) = &stdout_pipe {
            set_nonblocking(pipe.read.as_raw_fd())?;
        }
        if let Some(pipe) = &stderr_pipe {
            set_nonblocking(pipe.read.as_raw_fd())?;
        }
        set_nonblocking(setup_pipe.read.as_raw_fd())?;

        let mut close_fds = vec![dev_null.as_raw_fd()];
        for pipe in [&stdin_pipe, &stdout_pipe, &stderr_pipe]
            .into_iter()
            .flatten()
        {
            close_fds.push(pipe.read.as_raw_fd());
            close_fds.push(pipe.write.as_raw_fd());
        }
        close_fds.push(setup_pipe.read.as_raw_fd());
        close_fds.push(setup_pipe.write.as_raw_fd());
        if let Some(gate) = &spec.setup_gate {
            close_fds.push(gate.ready_write.as_raw_fd());
            close_fds.push(gate.release_read.as_raw_fd());
            close_fds.extend(gate.inherited_parent_fds);
        }
        let (child_stdin, child_stdout, child_stderr) = match spec.stdio {
            StdioMode::Capture => {
                let stdout = stdout_pipe.as_ref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "missing capture stdout pipe")
                })?;
                let stderr = stderr_pipe.as_ref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "missing capture stderr pipe")
                })?;
                (
                    dev_null.as_raw_fd(),
                    stdout.write.as_raw_fd(),
                    stderr.write.as_raw_fd(),
                )
            }
            StdioMode::PipedInput => {
                let stdin = stdin_pipe.as_ref().ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "missing piped stdin")
                })?;
                (
                    stdin.read.as_raw_fd(),
                    dev_null.as_raw_fd(),
                    dev_null.as_raw_fd(),
                )
            }
            StdioMode::CaptureWithInput => {
                let stdin = stdin_pipe.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "missing capture-with-input stdin",
                    )
                })?;
                let stdout = stdout_pipe.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "missing capture-with-input stdout pipe",
                    )
                })?;
                let stderr = stderr_pipe.as_ref().ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "missing capture-with-input stderr pipe",
                    )
                })?;
                (
                    stdin.read.as_raw_fd(),
                    stdout.write.as_raw_fd(),
                    stderr.write.as_raw_fd(),
                )
            }
        };

        Ok(Self {
            executable,
            _argv: argv,
            argv_pointers,
            _environment: environment,
            environment_pointers,
            current_dir,
            stdio: spec.stdio,
            child_stdin,
            child_stdout,
            child_stderr,
            dev_null,
            stdin_pipe,
            stdout_pipe,
            stderr_pipe,
            setup_pipe,
            setup_gate: spec.setup_gate,
            close_fds,
            failure_record: SetupFailureRecord {
                stage: 0,
                padding: [0; 3],
                errno: 0,
            },
        })
    }
}

// SAFETY: call only in the fork child; `prepared` must remain valid and the implementation may
// perform only async-signal-safe operations until it reaches execve or _exit.
unsafe fn child_exec(prepared: &PreparedSpawn) -> ! {
    // SAFETY: this entire branch executes only in the child after fork; all raw FDs, C strings,
    // pointer arrays, and SetupFailureRecord storage were prepared before fork and stay valid;
    // every libc call here is async-signal-safe until execve or _exit.
    unsafe {
        if libc::setpgid(0, 0) != 0 {
            child_fail(prepared, SetupStage::SetProcessGroup)
        }

        if let Some(gate) = &prepared.setup_gate {
            libc::close(gate.inherited_parent_fds[0]);
            libc::close(gate.inherited_parent_fds[1]);
            let pid = libc::getpid();
            let pid_bytes = std::slice::from_raw_parts(
                (&pid as *const libc::pid_t).cast::<u8>(),
                std::mem::size_of::<libc::pid_t>(),
            );
            if !child_write_all(gate.ready_write.as_raw_fd(), pid_bytes) {
                child_fail(prepared, SetupStage::ReadyHandshake)
            }
            let mut release = 0u8;
            loop {
                let result = libc::read(
                    gate.release_read.as_raw_fd(),
                    (&mut release as *mut u8).cast(),
                    1,
                );
                if result == 1 {
                    break;
                }
                if result == 0 {
                    child_fail_with_errno(prepared, SetupStage::SetupGate, libc::EPIPE)
                }
                if *libc::__errno_location() != libc::EINTR {
                    child_fail(prepared, SetupStage::SetupGate)
                }
            }
            libc::close(gate.ready_write.as_raw_fd());
            libc::close(gate.release_read.as_raw_fd());
        }

        if libc::chdir(prepared.current_dir.as_ptr()) != 0 {
            child_fail(prepared, SetupStage::ChangeDirectory)
        }
        if libc::dup2(prepared.child_stdin, libc::STDIN_FILENO) < 0 {
            child_fail(prepared, SetupStage::StandardInput)
        }
        if libc::dup2(prepared.child_stdout, libc::STDOUT_FILENO) < 0 {
            child_fail(prepared, SetupStage::StandardOutput)
        }
        if libc::dup2(prepared.child_stderr, libc::STDERR_FILENO) < 0 {
            child_fail(prepared, SetupStage::StandardError)
        }

        let close_pointer = prepared.close_fds.as_ptr();
        let close_len = prepared.close_fds.len();
        let setup_write = prepared.setup_pipe.write.as_raw_fd();
        let mut index = 0usize;
        while index < close_len {
            let fd = *close_pointer.add(index);
            if fd != setup_write && fd > libc::STDERR_FILENO {
                libc::close(fd);
            }
            index += 1;
        }

        libc::execve(
            prepared.executable.as_ptr(),
            prepared.argv_pointers.as_ptr(),
            prepared.environment_pointers.as_ptr(),
        );
        child_fail(prepared, SetupStage::Exec)
    }
}

// SAFETY: call only from the post-fork child with a valid pre-fork `prepared` allocation; it
// reports a fixed-size failure record using async-signal-safe operations and then terminates.
unsafe fn child_fail(prepared: &PreparedSpawn, stage: SetupStage) -> ! {
    // SAFETY: the function runs only in the post-fork child, where reading thread-local errno
    // and delegating to the _exit failure path preserves the async-signal-safe contract.
    unsafe {
        let errno = *libc::__errno_location();
        child_fail_with_errno(prepared, stage, errno)
    }
}

// SAFETY: call only from the post-fork child with a valid setup write FD in `prepared`; this
// function writes the fixed record with async-signal-safe operations and always calls _exit.
unsafe fn child_fail_with_errno(prepared: &PreparedSpawn, stage: SetupStage, errno: i32) -> ! {
    // SAFETY: `prepared` and its record/FD storage were initialized before fork and remain valid
    // in the child copy; the record byte slice has exactly SETUP_RECORD_BYTES before _exit.
    unsafe {
        let mut record = prepared.failure_record;
        record.stage = stage.to_wire();
        record.errno = errno;
        let bytes = std::slice::from_raw_parts(
            (&record as *const SetupFailureRecord).cast::<u8>(),
            SETUP_RECORD_BYTES,
        );
        let _ = child_write_all(prepared.setup_pipe.write.as_raw_fd(), bytes);
        libc::_exit(127)
    }
}

fn path_cstring(path: &Path) -> io::Result<CString> {
    os_cstring(path.as_os_str(), "path")
}

fn os_cstring(value: &OsStr, field: &str) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{field} contains an interior NUL byte"),
        )
    })
}

fn establish_process_group(pid: libc::pid_t) -> io::Result<()> {
    loop {
        // SAFETY: `pid` is the just-forked direct child owned by this supervisor; using it for
        // both pid and pgid establishes an isolated group before the child can be reaped.
        if unsafe { libc::setpgid(pid, pid) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        // SAFETY: this checks only the still-owned child PID after setpgid reported the race
        // errno; it verifies the child already became the expected group leader.
        if matches!(error.raw_os_error(), Some(libc::EACCES | libc::EPERM))
            && unsafe { libc::getpgid(pid) } == pid
        {
            return Ok(());
        }
        return Err(error);
    }
}

fn open_pidfd(pid: libc::pid_t) -> io::Result<OwnedFd> {
    // SAFETY: pid is the just-forked direct child, and SYS_pidfd_open is invoked with flags 0;
    // the returned integer is checked before it is treated as an owned descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as RawFd;
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pidfd_open returned a fresh nonnegative FD that has not been wrapped.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

fn read_start_time(pid: libc::pid_t) -> io::Result<u64> {
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

fn poll_fd(fd: RawFd, events: libc::c_short, deadline: Instant) -> io::Result<()> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "deadline expired"));
        }
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128).max(1) as i32;
        let mut pollfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        // SAFETY: `pollfd` is initialized stack storage and its address stays valid for this
        // synchronous single-entry poll call.
        let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if result > 0 {
            return Ok(());
        }
        if result == 0 {
            continue;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn write_all(fd: RawFd, bytes: &[u8]) -> io::Result<()> {
    let mut written = 0usize;
    while written < bytes.len() {
        // SAFETY: callers retain the open destination FD; the slice is live and the offset is
        // bounded by bytes.len(), so its pointer and remaining length are valid for write.
        let result = unsafe {
            libc::write(
                fd,
                bytes.as_ptr().add(written).cast(),
                bytes.len() - written,
            )
        };
        if result > 0 {
            written += result as usize;
            continue;
        }
        if result < 0 && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

// SAFETY: call only in the post-fork child with an open FD and a live byte slice prepared before
// fork; it performs only async-signal-safe write/errno operations and never allocates.
unsafe fn child_write_all(fd: RawFd, bytes: &[u8]) -> bool {
    // SAFETY: the post-fork caller guarantees that `fd` is open and `bytes` remains valid; each
    // offset stays within the slice, and the loop uses only write and errno before execve/_exit.
    unsafe {
        let mut written = 0usize;
        while written < bytes.len() {
            let result = libc::write(
                fd,
                bytes.as_ptr().add(written).cast(),
                bytes.len() - written,
            );
            if result > 0 {
                written += result as usize;
                continue;
            }
            if result < 0 && *libc::__errno_location() == libc::EINTR {
                continue;
            }
            return false;
        }
        true
    }
}
