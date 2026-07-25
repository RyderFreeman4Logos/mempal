#[cfg(unix)]
use std::io::{BufRead as _, Write as _};
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
pub(super) fn wait(child: &mut Child, home: &Path, timeout: Duration) {
    let mempal_home = home.join(".mempal");
    wait_for_path(&mempal_home.join("daemon.pid"), timeout);
    let socket_path = mempal_home.join("daemon-hook.sock");
    wait_for_path(&socket_path, timeout);
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if readiness_probe(&socket_path, child.id()) {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let _ = child.kill();
    let _ = super::wait_for_child_exit(child, Duration::from_secs(5));
    panic!("timed out waiting for daemon IPC readiness");
}

#[cfg(unix)]
fn wait_for_path(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("timed out waiting for daemon startup path");
}

#[cfg(unix)]
fn readiness_probe(socket_path: &Path, daemon_pid: u32) -> bool {
    let Ok(mut stream) = UnixStream::connect(socket_path) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(Duration::from_millis(250)))
        .is_err()
        || stream
            .set_write_timeout(Some(Duration::from_millis(250)))
            .is_err()
        || stream.write_all(b"{\"probe\":\"readiness\"}\n").is_err()
        || stream.flush().is_err()
    {
        return false;
    }

    let mut response = Vec::new();
    let Ok(response_len) = std::io::BufReader::new(stream).read_until(b'\n', &mut response) else {
        return false;
    };
    response_len > 0
        && serde_json::from_slice::<Value>(&response).is_ok_and(|response| {
            response["status"] == "ready" && response["pid"].as_u64() == Some(u64::from(daemon_pid))
        })
}
