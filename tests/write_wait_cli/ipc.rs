#[cfg(unix)]
use std::io::{self, Read as _, Write as _};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Child;
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use serde_json::Value;

#[cfg(unix)]
const MAX_READINESS_RESPONSE_BYTES: usize = 4096;
#[cfg(unix)]
const READINESS_IO_TIMEOUT: Duration = Duration::from_millis(250);

#[cfg(unix)]
pub(super) fn wait(child: &mut Child, home: &Path, timeout: Duration) {
    let mempal_home = home.join(".mempal");
    let deadline = Instant::now() + timeout;
    if !wait_for_path(&mempal_home.join("daemon.pid"), deadline) {
        terminate_child(child);
        panic!("timed out waiting for daemon startup path");
    }
    let socket_path = mempal_home.join("daemon-hook.sock");
    if !wait_for_path(&socket_path, deadline) {
        terminate_child(child);
        panic!("timed out waiting for daemon startup path");
    }
    while Instant::now() < deadline {
        if readiness_probe(&socket_path, child.id(), deadline) {
            return;
        }
        std::thread::sleep(
            Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
        );
    }

    terminate_child(child);
    panic!("timed out waiting for daemon IPC readiness");
}

#[cfg(unix)]
fn wait_for_path(path: &Path, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        if path.exists() {
            return true;
        }
        std::thread::sleep(
            Duration::from_millis(50).min(deadline.saturating_duration_since(Instant::now())),
        );
    }
    false
}

#[cfg(unix)]
fn readiness_io_timeout(deadline: Instant) -> Option<Duration> {
    let remaining = deadline.checked_duration_since(Instant::now())?;
    (!remaining.is_zero()).then_some(remaining.min(READINESS_IO_TIMEOUT))
}

#[cfg(unix)]
fn terminate_child(child: &mut Child) {
    let _ = child.kill();
    let _ = super::wait_for_child_exit(child, Duration::from_secs(5));
}

#[cfg(unix)]
pub(super) fn readiness_probe(socket_path: &Path, daemon_pid: u32, deadline: Instant) -> bool {
    let Ok(mut stream) = connect_with_deadline(socket_path, deadline) else {
        return false;
    };
    let Some(io_timeout) = readiness_io_timeout(deadline) else {
        return false;
    };
    if stream.set_read_timeout(Some(io_timeout)).is_err()
        || stream.set_write_timeout(Some(io_timeout)).is_err()
        || stream.write_all(b"{\"probe\":\"readiness\"}\n").is_err()
        || stream.flush().is_err()
    {
        return false;
    }

    let mut response = Vec::new();
    let mut reader = std::io::BufReader::new(stream);
    loop {
        let Some(io_timeout) = readiness_io_timeout(deadline) else {
            return false;
        };
        if reader.get_mut().set_read_timeout(Some(io_timeout)).is_err() {
            return false;
        }

        let remaining = MAX_READINESS_RESPONSE_BYTES.saturating_sub(response.len());
        if remaining == 0 {
            return false;
        }
        let mut chunk = [0; 512];
        let chunk_len = remaining.min(chunk.len());
        let Ok(read_len) = reader.read(&mut chunk[..chunk_len]) else {
            return false;
        };
        if read_len == 0 {
            return false;
        }

        let chunk = &chunk[..read_len];
        let (line_len, complete) = match chunk.iter().position(|byte| *byte == b'\n') {
            Some(position) => (position + 1, true),
            None => (chunk.len(), false),
        };
        response.extend_from_slice(&chunk[..line_len]);
        if complete {
            break;
        }
    }

    serde_json::from_slice::<Value>(&response).is_ok_and(|response| {
        response["status"] == "ready" && response["pid"].as_u64() == Some(u64::from(daemon_pid))
    })
}

#[cfg(unix)]
fn connect_with_deadline(socket_path: &Path, deadline: Instant) -> io::Result<UnixStream> {
    let path_bytes = socket_path.as_os_str().as_bytes();
    // SAFETY: all-zero bytes are a valid initial representation for sockaddr_un.
    let mut address = unsafe { std::mem::zeroed::<libc::sockaddr_un>() };
    if path_bytes.is_empty()
        || path_bytes.len() >= address.sun_path.len()
        || path_bytes.contains(&b'\0')
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Unix socket path for readiness probe",
        ));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    // SAFETY: `address.sun_path` has room for `path_bytes` plus the zeroed terminator, and both
    // pointers are valid for exactly path_bytes.len() bytes.
    unsafe {
        std::ptr::copy_nonoverlapping(
            path_bytes.as_ptr(),
            address.sun_path.as_mut_ptr().cast::<u8>(),
            path_bytes.len(),
        );
    }

    // SAFETY: socket has no pointer arguments, and the return value is checked before being
    // wrapped in OwnedFd.
    let raw_fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if raw_fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: socket returned a fresh open file descriptor owned by this function.
    let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
    set_nonblocking(fd.as_raw_fd(), true)?;

    let address_len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + path_bytes.len() + 1)
        as libc::socklen_t;
    // SAFETY: `fd` is open, and `address` remains initialized and live for the synchronous call.
    let connect_result = unsafe {
        libc::connect(
            fd.as_raw_fd(),
            (&raw const address).cast::<libc::sockaddr>(),
            address_len,
        )
    };
    if connect_result != 0 {
        let error = io::Error::last_os_error();
        let pending = error.raw_os_error().is_some_and(|code| {
            code == libc::EINPROGRESS || code == libc::EALREADY || code == libc::EWOULDBLOCK
        });
        if !pending {
            return Err(error);
        }
        wait_for_connect(fd.as_raw_fd(), deadline)?;
    }
    if Instant::now() >= deadline {
        return Err(readiness_deadline_error());
    }
    set_nonblocking(fd.as_raw_fd(), false)?;
    Ok(UnixStream::from(fd))
}

#[cfg(unix)]
fn set_nonblocking(fd: std::os::fd::RawFd, enabled: bool) -> io::Result<()> {
    // SAFETY: fd remains owned by the caller for the duration of this file-status flag query.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let flags = if enabled {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    // SAFETY: fd remains open and `flags` came from its successful F_GETFL result.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(unix)]
fn wait_for_connect(fd: std::os::fd::RawFd, deadline: Instant) -> io::Result<()> {
    loop {
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Err(readiness_deadline_error());
        };
        if remaining.is_zero() {
            return Err(readiness_deadline_error());
        }
        let timeout_ms = remaining.as_millis().min(i32::MAX as u128).max(1) as i32;
        let mut pollfd = libc::pollfd {
            fd,
            events: libc::POLLOUT,
            revents: 0,
        };
        // SAFETY: pollfd is initialized stack storage that remains live for the synchronous poll.
        let result = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if result == 0 {
            return Err(readiness_deadline_error());
        }
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if pollfd.revents & libc::POLLNVAL != 0 {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "invalid readiness probe socket",
            ));
        }

        let mut socket_error: libc::c_int = 0;
        let mut socket_error_len = std::mem::size_of_val(&socket_error) as libc::socklen_t;
        // SAFETY: fd remains open, and the initialized output pointers stay valid for this call.
        if unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_ERROR,
                (&mut socket_error as *mut libc::c_int).cast::<libc::c_void>(),
                &mut socket_error_len,
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        if socket_error == 0 {
            return Ok(());
        }
        return Err(io::Error::from_raw_os_error(socket_error));
    }
}

#[cfg(unix)]
fn readiness_deadline_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "readiness probe deadline expired")
}
