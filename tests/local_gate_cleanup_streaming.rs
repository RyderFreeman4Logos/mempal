#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn wait_with_output(mut self, timeout: Duration) -> std::process::Output {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.0.as_mut().expect("cleanup harness still owned");
            if child.try_wait().expect("poll cleanup harness").is_some() {
                return self
                    .0
                    .take()
                    .expect("completed cleanup harness")
                    .wait_with_output()
                    .expect("collect cleanup harness output");
            }
            assert!(
                Instant::now() < deadline,
                "fixture cleanup exceeded its externally owned deadline"
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

fn python_harness(source: &str) -> Command {
    let mut command = Command::new("python3");
    command
        .args(["-c", source])
        .arg(repo_root().join("scripts/gates/cargo-test-with-timeout.py"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

#[test]
fn fixture_cleanup_streams_entries_before_enforcing_bounds() {
    const HARNESS: &str = r#"
import importlib.util
import os
import sys
import tempfile
import time

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
identity_module = sys.modules["fixture_cleanup_identity"]

with tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"]) as base:
    root = os.path.join(base, "owned")
    os.mkdir(root, 0o700)
    for name in ("a", "b"):
        open(os.path.join(root, name), "wb").close()
    identity = module.capture_fixture_identity(root)
    checks = [0]
    def check_cleanup():
        checks[0] += 1
    def forbidden_listdir(_directory_fd):
        raise AssertionError("cleanup materialized a complete directory entry list")
    module.os.listdir = forbidden_listdir
    identity_module.os.listdir = forbidden_listdir
    identity_module.MAX_FIXTURE_ENTRIES = 1
    root_fd = os.open(root, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)
    try:
        snapshot = module.fixture_tree_stays_on_mount(
            root_fd, identity.mount_id, check_cleanup
        )
    finally:
        os.close(root_fd)
    assert snapshot is None
    assert checks[0] >= 3, checks
    identity_module.MAX_FIXTURE_ENTRIES = 100_000
    assert module.remove_owned_root(identity, time.monotonic() + 5, 0)
    assert not os.path.exists(root)
"#;
    let base = tempfile::tempdir().expect("streaming cleanup harness base");
    let output = python_harness(HARNESS)
        .env("TMPDIR", base.path())
        .env("TMP", base.path())
        .env("TEMP", base.path())
        .output()
        .expect("run streaming cleanup harness");
    assert!(
        output.status.success(),
        "streaming cleanup contract failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn fixture_cleanup_deadline_owns_blocked_filesystem_worker() {
    const HARNESS: &str = r#"
import importlib.util
import os
import sys
import time

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
base = os.environ["TMPDIR"]
root_marker = os.path.join(base, "root-marker")
worker_marker = os.path.join(base, "worker-marker")

child = r'''
import os
root = os.environ["TMPDIR"]
open(os.path.join(root, "leaf"), "wb").close()
with open(os.environ["ROOT_MARKER"], "w", encoding="utf-8") as marker:
    marker.write(root)
    marker.flush()
    os.fsync(marker.fileno())
'''

def blocked_snapshot(*_args):
    with open(worker_marker, "w", encoding="utf-8") as marker:
        stat = open(f"/proc/{os.getpid()}/stat", encoding="utf-8").read()
        start = stat.rsplit(") ", 1)[1].split()[19]
        marker.write(f"{os.getpid()} {start}\n")
        marker.flush()
        os.fsync(marker.fileno())
    while True:
        time.sleep(1)

module.fixture_tree_stays_on_mount = blocked_snapshot
os.environ["ROOT_MARKER"] = root_marker
os.environ["MEMPAL_CARGO_TEST_TIMEOUT_SECS"] = "30"
os.environ["MEMPAL_CARGO_TEST_KILL_GRACE_SECS"] = "1"
started = time.monotonic()
result = module.main(["python3", "-c", child])
elapsed = time.monotonic() - started
assert result == 125, result
assert elapsed < 4, elapsed
root = open(root_marker, encoding="utf-8").read()
assert os.path.isdir(root), root
pid_text, start = open(worker_marker, encoding="utf-8").read().split()
pid = int(pid_text)
assert pid != os.getpid(), "blocking filesystem work remained in the supervisor"
try:
    stat = open(f"/proc/{pid}/stat", encoding="utf-8").read()
except FileNotFoundError:
    pass
else:
    fields = stat.rsplit(") ", 1)[1].split()
    assert fields[19] != start, (pid, start, fields[0])
"#;
    let base = tempfile::tempdir().expect("deadline cleanup harness base");
    let child = python_harness(HARNESS)
        .env("TMPDIR", base.path())
        .env("TMP", base.path())
        .env("TEMP", base.path())
        .spawn()
        .expect("spawn deadline cleanup harness");
    let output = ChildGuard(Some(child)).wait_with_output(Duration::from_secs(5));
    assert!(
        output.status.success(),
        "deadline cleanup contract failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
