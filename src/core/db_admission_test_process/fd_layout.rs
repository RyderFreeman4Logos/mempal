use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

pub(super) struct Pipe {
    pub(super) read: OwnedFd,
    pub(super) write: OwnedFd,
}

impl Pipe {
    pub(super) fn new() -> io::Result<Self> {
        let (read, write) = pipe_cloexec()?;
        Ok(Self { read, write })
    }
}

pub(super) fn pipe_cloexec() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: `fds` is writable storage for exactly two descriptor integers as required by
    // pipe2; O_CLOEXEC does not affect Rust memory validity.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful pipe2 initialized two distinct nonnegative FDs whose ownership has not
    // yet been wrapped; each is transferred exactly once into its OwnedFd.
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    Ok((
        relocate_fd_at_least_three(read)?,
        relocate_fd_at_least_three(write)?,
    ))
}

/// Moves a pre-fork child-setup descriptor out of the standard-fd namespace.
///
/// Every source later used by child `dup2` must be above `STDERR_FILENO`: a source at 0, 1, or
/// 2 can be overwritten by an earlier mapping, and `dup2(fd, fd)` retains close-on-exec.
pub(super) fn relocate_fd_at_least_three(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() > libc::STDERR_FILENO {
        return Ok(fd);
    }
    // SAFETY: `fd` remains owned and open while fcntl duplicates it to the lowest available
    // descriptor at least 3. F_DUPFD_CLOEXEC leaves the original valid for OwnedFd to close.
    let relocated = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if relocated < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful fcntl returned a fresh descriptor not yet owned by Rust. The original
    // OwnedFd drops after this function returns, closing only its pre-relocation descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(relocated) })
}

pub(super) fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    // SAFETY: callers pass an open descriptor they retain for both fcntl calls; F_GETFL does not
    // dereference Rust memory and returns the descriptor's current flag word.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` remains open and `flags` came from its successful F_GETFL call, so OR-ing
    // O_NONBLOCK preserves all existing file-status flags while updating the same descriptor.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
