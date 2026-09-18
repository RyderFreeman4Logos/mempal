#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn wait_with_output_timeout(
        &mut self,
        timeout: Duration,
        description: &str,
    ) -> std::process::Output {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.0.as_mut().expect("harness still owned");
            if child.try_wait().expect("poll harness").is_some() {
                return self
                    .0
                    .take()
                    .expect("completed harness")
                    .wait_with_output()
                    .expect("collect harness output");
            }
            assert!(
                Instant::now() < deadline,
                "{description} did not exit in time"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[test]
fn test_cleanup_retains_fixture_when_sigterm_handler_forks_after_last_scan() {
    const HARNESS: &str = r#"
import importlib.util
import os
import signal
import sys
import threading

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

base = os.environ["TMPDIR"]
root_marker = os.path.join(base, "root-marker")
ready_fifo = os.path.join(base, "ready.fifo")
forked_fifo = os.path.join(base, "forked.fifo")
grandchild_path = os.path.join(base, "grandchild-identity")
os.mkfifo(ready_fifo, 0o600)
os.mkfifo(forked_fifo, 0o600)

child = r'''
import os
import signal

def handler(_signum, _frame):
    pid = os.fork()
    if pid == 0:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        stat = open(f"/proc/{os.getpid()}/stat", "rb").read()
        start = stat.rpartition(b") ")[2].split()[19]
        fd = os.open(os.environ["GRANDCHILD_IDENTITY"], os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
        os.write(fd, str(os.getpid()).encode() + b" " + start + b"\n")
        os.fsync(fd)
        os.close(fd)
        wake = os.open(os.environ["FORKED_FIFO"], os.O_WRONLY)
        os.close(wake)
        while True:
            signal.pause()
    os._exit(0)

signal.signal(signal.SIGTERM, handler)
root = os.environ["TMPDIR"]
open(os.path.join(root, "keep-me"), "wb").write(b"x")
with open(os.environ["ROOT_MARKER"], "w", encoding="utf-8") as marker:
    marker.write(root)
    marker.flush()
    os.fsync(marker.fileno())
ready = os.open(os.environ["READY_FIFO"], os.O_WRONLY)
os.close(ready)
signal.pause()
'''

fork_published = [False]
post_fork_owned = []
original_discover = module.Supervisor.discover

def wrapped_discover(self):
    snapshots = original_discover(self)
    if fork_published[0]:
        post_fork_owned.append([
            (handle.identity.pid, handle.identity.start_time)
            for handle in self.owned.values()
            if handle.identity is not None and not handle.exited
        ])
    return snapshots

original_signal_owned = module.Supervisor.signal_owned

def wrapped_signal_owned(self, signum, snapshots):
    proved = original_signal_owned(self, signum, snapshots)
    if signum == signal.SIGTERM and not fork_published[0]:
        wake = os.open(forked_fifo, os.O_RDONLY)
        os.close(wake)
        fork_published[0] = True
    return proved

module.Supervisor.discover = wrapped_discover
module.Supervisor.signal_owned = wrapped_signal_owned

def request_cleanup():
    ready = os.open(ready_fifo, os.O_RDONLY)
    os.close(ready)
    module._pending_signal = signal.SIGTERM

controller = threading.Thread(target=request_cleanup)
controller.start()
os.environ["ROOT_MARKER"] = root_marker
os.environ["GRANDCHILD_IDENTITY"] = grandchild_path
os.environ["FORKED_FIFO"] = forked_fifo
os.environ["READY_FIFO"] = ready_fifo
os.environ["MEMPAL_CARGO_TEST_TIMEOUT_SECS"] = "30"
os.environ["MEMPAL_CARGO_TEST_KILL_GRACE_SECS"] = "1"
try:
    result = module.main(["python3", "-c", child])
finally:
    controller.join(5)
    assert not controller.is_alive(), "cleanup trigger did not stop"

root = open(root_marker, encoding="utf-8").read()
pid_text, start_text = open(grandchild_path, encoding="utf-8").read().split()
pid = int(pid_text)
start = int(start_text)
live = False
try:
    stat = open(f"/proc/{pid}/stat", encoding="utf-8").read()
except FileNotFoundError:
    pass
else:
    fields = stat.rsplit(") ", 1)[1].split()
    live = int(fields[19]) == start
identity = (pid, start)
saw = any(identity in owned for owned in post_fork_owned)
cleared = saw and not live
try:
    if not cleared:
        assert os.path.isdir(root), (result, root, post_fork_owned)
        assert os.path.exists(os.path.join(root, "keep-me")), root
        assert live, (pid, start, result)
        assert result == 125, result
finally:
    if live:
        os.kill(pid, signal.SIGKILL)
        try:
            os.waitpid(pid, 0)
        except ChildProcessError:
            pass
"#;

    let base = tempfile::tempdir().expect("sigterm-fork cleanup harness base");
    let mut command = Command::new("python3");
    command
        .args(["-c", HARNESS])
        .arg(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("scripts/gates/cargo-test-with-timeout.py"),
        )
        .env("TMPDIR", base.path())
        .env("TMP", base.path())
        .env("TEMP", base.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = ChildGuard(Some(
        command.spawn().expect("spawn sigterm-fork cleanup harness"),
    ));
    let output = child.wait_with_output_timeout(Duration::from_secs(20), "sigterm-fork cleanup");
    assert!(
        output.status.success(),
        "sigterm-fork cleanup harness failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
