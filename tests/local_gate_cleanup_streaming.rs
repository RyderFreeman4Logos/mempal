#![cfg(target_os = "linux")]

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const DIAGNOSTIC_TAIL_BYTES: u64 = 16 * 1024;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn tail_file_for_diagnostics(path: &Path) -> String {
    let mut file = match File::open(path) {
        Ok(file) => file,
        Err(error) => return format!("<{} unavailable: {error}>", path.display()),
    };
    let len = match file.metadata() {
        Ok(metadata) => metadata.len(),
        Err(error) => return format!("<{} metadata unavailable: {error}>", path.display()),
    };
    let start = len.saturating_sub(DIAGNOSTIC_TAIL_BYTES);
    if let Err(error) = file.seek(SeekFrom::Start(start)) {
        return format!("<{} seek failed: {error}>", path.display());
    }
    let mut bytes = Vec::new();
    if let Err(error) = file.take(DIAGNOSTIC_TAIL_BYTES).read_to_end(&mut bytes) {
        return format!("<{} read failed: {error}>", path.display());
    }
    let text = String::from_utf8_lossy(&bytes);
    if start == 0 {
        text.into_owned()
    } else {
        format!("<truncated first {start} bytes>\n{text}")
    }
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn wait_for_exit(
        mut self,
        timeout: Duration,
        stdout_path: &Path,
        stderr_path: &Path,
    ) -> ExitStatus {
        let started_at = Instant::now();
        let deadline = started_at + timeout;
        loop {
            let child = self.0.as_mut().expect("cleanup harness still owned");
            if let Some(status) = child.try_wait().expect("poll cleanup harness") {
                let _ = self.0.take();
                return status;
            }
            if Instant::now() >= deadline {
                let stdout = tail_file_for_diagnostics(stdout_path);
                let stderr = tail_file_for_diagnostics(stderr_path);
                let pid = self.0.as_ref().expect("cleanup harness still owned").id();
                panic!(
                    "fixture cleanup exceeded its externally owned deadline\n\
                     stage=outer-timeout pid={pid} elapsed={:?}\n\
                     stdout tail:\n{stdout}\nstderr tail:\n{stderr}",
                    started_at.elapsed()
                );
            }
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
stages = set()

def stage(name):
    if name not in stages:
        stages.add(name)
        os.write(2, f"stage={name}\n".encode("ascii"))

original_discover = module.Supervisor.discover
def traced_discover(self):
    stage("supervisor-scan-enter")
    result = original_discover(self)
    stage("supervisor-scan-exit")
    return result
module.Supervisor.discover = traced_discover

original_cleanup = module.Supervisor.cleanup
def traced_cleanup(self):
    stage("supervisor-cleanup-enter")
    result = original_cleanup(self)
    stage("supervisor-cleanup-exit")
    return result
module.Supervisor.cleanup = traced_cleanup

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
    stage("remove-worker-scan-enter")
    with open(worker_marker, "w", encoding="utf-8") as marker:
        stat = open(f"/proc/{os.getpid()}/stat", encoding="utf-8").read()
        start = stat.rsplit(") ", 1)[1].split()[19]
        marker.write(f"{os.getpid()} {start}\n")
        marker.flush()
        os.fsync(marker.fileno())
    while True:
        time.sleep(1)

module.fixture_tree_stays_on_mount = blocked_snapshot
original_remove_owned_root = module.remove_owned_root
def traced_remove_owned_root(identity, deadline, signal_generation):
    stage("remove-owned-root-enter")
    result = original_remove_owned_root(identity, deadline, signal_generation)
    stage("remove-owned-root-exit")
    return result
module.remove_owned_root = traced_remove_owned_root
os.environ["ROOT_MARKER"] = root_marker
os.environ["MEMPAL_CARGO_TEST_TIMEOUT_SECS"] = "30"
os.environ["MEMPAL_CARGO_TEST_KILL_GRACE_SECS"] = "1"
started = time.monotonic()
stage("main-enter")
result = module.main(["python3", "-c", child])
stage("main-exit")
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
    let stdout_path = base.path().join("harness.stdout.log");
    let stderr_path = base.path().join("harness.stderr.log");
    let stdout = File::create(&stdout_path).expect("create cleanup harness stdout");
    let stderr = File::create(&stderr_path).expect("create cleanup harness stderr");
    let child = python_harness(HARNESS)
        .env("TMPDIR", base.path())
        .env("TMP", base.path())
        .env("TEMP", base.path())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .expect("spawn deadline cleanup harness");
    let status =
        ChildGuard(Some(child)).wait_for_exit(Duration::from_secs(5), &stdout_path, &stderr_path);
    assert!(
        status.success(),
        "deadline cleanup contract failed: stdout={} stderr={}",
        tail_file_for_diagnostics(&stdout_path),
        tail_file_for_diagnostics(&stderr_path)
    );
}

#[test]
fn diagnostic_tail_is_bounded_and_retains_latest_stage() {
    let base = tempfile::tempdir().expect("diagnostic tail base");
    let path = base.path().join("diagnostic.log");
    let mut file = File::create(&path).expect("create diagnostic log");
    let oversized_prefix = vec![b'x'; DIAGNOSTIC_TAIL_BYTES as usize + 1];
    file.write_all(&oversized_prefix)
        .expect("write oversized diagnostic prefix");
    file.write_all(b"\nstage=latest\n")
        .expect("write latest diagnostic stage");

    let tail = tail_file_for_diagnostics(&path);
    assert!(tail.starts_with("<truncated first "));
    assert!(tail.ends_with("stage=latest\n"));
    assert!(tail.len() < DIAGNOSTIC_TAIL_BYTES as usize + 128);
}
