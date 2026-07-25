#[cfg(unix)]
use std::io::{Read as _, Write as _};
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
fn readiness_probe(socket_path: &Path, daemon_pid: u32, deadline: Instant) -> bool {
    let Some(io_timeout) = readiness_io_timeout(deadline) else {
        return false;
    };
    let Ok(mut stream) = UnixStream::connect(socket_path) else {
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
