use std::ffi::{CString, OsString};
use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

pub(super) type FileSignature = (u64, u64, u64, i64, i64);

pub(super) fn file_signature(path: &Path) -> Option<FileSignature> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
    ))
}

pub(super) struct HotReloadWatcher {
    fd: OwnedFd,
    file_name: Option<OsString>,
}

impl HotReloadWatcher {
    pub(super) fn register(directory: &Path, file_name: Option<OsString>) -> io::Result<Self> {
        // SAFETY: inotify_init1 has no pointer arguments; OwnedFd takes sole ownership on success.
        let raw_fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC | libc::IN_NONBLOCK) };
        if raw_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: raw_fd was just returned by inotify_init1 and is uniquely owned here.
        let fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let directory = CString::new(directory.as_os_str().as_bytes()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "watch path contains a NUL byte",
            )
        })?;
        let mask = libc::IN_CREATE
            | libc::IN_DELETE
            | libc::IN_MODIFY
            | libc::IN_MOVED_FROM
            | libc::IN_MOVED_TO
            | libc::IN_CLOSE_WRITE;
        // SAFETY: directory is a live NUL-terminated path and fd is a live inotify descriptor.
        if unsafe { libc::inotify_add_watch(fd.as_raw_fd(), directory.as_ptr(), mask) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { fd, file_name })
    }

    pub(super) fn changed(&self) -> io::Result<bool> {
        let mut buffer = [0_u8; 4096];
        let mut changed = false;
        loop {
            // SAFETY: buffer is writable for its full length and fd is a live nonblocking descriptor.
            let read = unsafe {
                libc::read(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                )
            };
            if read < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    return Ok(changed);
                }
                return Err(error);
            }
            if read == 0 {
                return Ok(changed);
            }
            changed |= self.buffer_mentions_target(&buffer[..read as usize])?;
        }
    }

    fn buffer_mentions_target(&self, buffer: &[u8]) -> io::Result<bool> {
        let header_size = size_of::<libc::inotify_event>();
        let mut offset = 0;
        let mut changed = false;
        while offset < buffer.len() {
            if buffer.len() - offset < header_size {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated inotify event header",
                ));
            }
            // SAFETY: the size check above guarantees a full header; read_unaligned avoids alignment assumptions.
            let event = unsafe {
                std::ptr::read_unaligned(buffer[offset..].as_ptr().cast::<libc::inotify_event>())
            };
            let name_len = event.len as usize;
            let record_len = header_size.checked_add(name_len).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "oversized inotify event")
            })?;
            if record_len > buffer.len() - offset {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "truncated inotify event name",
                ));
            }
            let name = &buffer[offset + header_size..offset + record_len];
            let name = &name[..name
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(name.len())];
            changed |= event.mask & libc::IN_Q_OVERFLOW != 0
                || self
                    .file_name
                    .as_ref()
                    .is_none_or(|target| target.as_encoded_bytes() == name);
            offset += record_len;
        }
        Ok(changed)
    }
}
