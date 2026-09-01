use std::fs;
use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(unix)]
#[allow(dead_code)]
#[path = "support/local_gate_child.rs"]
mod local_gate_child;

#[cfg(unix)]
#[allow(dead_code)]
#[path = "support/local_gate_pid_safety.rs"]
mod local_gate_pid_safety;

#[cfg(unix)]
use local_gate_child::{capture_recorded_process, spawn_in_own_session};

#[cfg(target_os = "linux")]
#[path = "support/local_gate_timeout_absent_snapshot_tests.rs"]
mod local_gate_timeout_absent_snapshot_tests;

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

#[cfg(unix)]
type PipedWrapper = (
    std::process::Child,
    Receiver<std::io::Result<Vec<u8>>>,
    Receiver<std::io::Result<Vec<u8>>>,
);

#[cfg(unix)]
fn spawn_piped_wrapper(
    script: &Path,
    command: &str,
    timeout_secs: &str,
    identity_path: &Path,
) -> PipedWrapper {
    let mut wrapper = Command::new("/bin/bash");
    wrapper
        .arg(script)
        .args(["/bin/bash", "-c", command])
        .current_dir(repo_root())
        .env("IDENTITY_FILE", identity_path)
        .env("MEMPAL_CARGO_TEST_TIMEOUT_SECS", timeout_secs)
        .env("MEMPAL_CARGO_TEST_KILL_GRACE_SECS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    let mut wrapper = wrapper.spawn().expect("spawn timeout wrapper");
    let stdout = wrapper.stdout.take().expect("capture wrapper stdout");
    let stderr = wrapper.stderr.take().expect("capture wrapper stderr");
    let stdout_done = spawn_pipe_reader(stdout);
    let stderr_done = spawn_pipe_reader(stderr);
    (wrapper, stdout_done, stderr_done)
}

#[cfg(unix)]
fn spawn_pipe_reader<R: Read + Send + 'static>(mut pipe: R) -> Receiver<std::io::Result<Vec<u8>>> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut output = Vec::new();
        let result = pipe.read_to_end(&mut output).map(|_| output);
        let _ = sender.send(result);
    });
    receiver
}

#[cfg(unix)]
fn wait_for_wrapper(
    wrapper: &mut std::process::Child,
    timeout: Duration,
    description: &str,
) -> std::process::ExitStatus {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = wrapper.try_wait().expect("poll timeout wrapper") {
            return status;
        }
        assert!(
            Instant::now() < deadline,
            "{description} did not exit in time"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn recorded_process_is_running(record: &str) -> bool {
    let identity = local_gate_pid_safety::recorded_process_identity(record);
    capture_recorded_process(identity)
        .expect("re-verify fixture process identity")
        .map(|process| {
            process
                .is_running()
                .expect("inspect fixture process liveness")
        })
        .unwrap_or(false)
}

#[cfg(unix)]
fn kill_recorded_process(record: &str) {
    let identity = local_gate_pid_safety::recorded_process_identity(record);
    if let Some(process) =
        capture_recorded_process(identity).expect("re-verify fixture identity before cleanup")
    {
        if process
            .is_running()
            .expect("inspect fixture before cleanup")
        {
            process
                .send_signal(libc::SIGKILL)
                .expect("kill surviving fixture process");
        }
    }
}

#[cfg(unix)]
fn receive_pipe(
    receiver: &Receiver<std::io::Result<Vec<u8>>>,
    already_eof: Option<std::io::Result<Vec<u8>>>,
) -> Vec<u8> {
    already_eof
        .or_else(|| receiver.recv_timeout(Duration::from_secs(2)).ok())
        .expect("wrapper output did not reach EOF")
        .expect("read wrapper output")
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_names_leftover_owned_processes_on_cleanup_proof_failure() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import subprocess
import sys

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

child = subprocess.Popen(["/bin/sleep", "10"], start_new_session=True)
supervisor = module.Supervisor(child, 1)
original_poll = child.poll
supervisor.cleanup = lambda: False
child.poll = lambda: 0
try:
    assert supervisor.run(1) == 125
finally:
    child.poll = original_poll
    child.terminate()
    child.wait()
    supervisor.close()
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(&script)
        .output()
        .expect("run cleanup-proof diagnostics harness");

    assert!(
        output.status.success(),
        "cleanup-proof diagnostics harness failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("remaining owned processes:"),
        "stderr={stderr}"
    );
    assert!(stderr.contains("pid="), "stderr={stderr}");
    assert!(stderr.contains("start_time="), "stderr={stderr}");
    assert!(stderr.contains("comm='sleep'"), "stderr={stderr}");
    assert!(stderr.contains("ppid="), "stderr={stderr}");
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_adopts_reparented_comm_sccache_with_a_non_sccache_exe() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import ctypes
import importlib.util
import subprocess
import sys
import time

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

child = subprocess.Popen(["/bin/sleep", "10"], start_new_session=True)
cache = subprocess.Popen([
    "/usr/bin/python3", "-c",
    "import ctypes, time; ctypes.CDLL(None).prctl(15, b'sccache', 0, 0, 0); time.sleep(10)",
])
supervisor = module.Supervisor(child, 0.01)
original_signal_owned = supervisor.signal_owned
try:
    deadline = time.monotonic() + 1
    while module.read_snapshot(cache.pid).comm != "sccache":
        assert time.monotonic() < deadline
        time.sleep(0.01)
    snapshots = supervisor.discover()
    assert cache.pid in supervisor.owned
    assert supervisor.live_status(snapshots) == (True, False)
    child.terminate()
    child.wait()
    supervisor.signal_owned = lambda signum, snapshots: True
    assert not supervisor.cleanup()
    assert cache.poll() is None
finally:
    supervisor.signal_owned = original_signal_owned
    cache.terminate()
    cache.wait()
    child.terminate()
    child.wait()
    supervisor.close()
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(&script)
        .output()
        .expect("run escaped comm=sccache cleanup harness");

    assert!(
        output.status.success(),
        "comm=sccache must not green cleanup while its non-sccache executable is live: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_does_not_adopt_an_idle_authenticated_sccache_daemon() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import shutil
import subprocess
import sys

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

sccache = shutil.which("sccache")
assert sccache is not None
child = subprocess.Popen(["/bin/sleep", "10"], start_new_session=True)
cache = subprocess.Popen(["/usr/bin/python3", "-c", "import time; time.sleep(10)"])
supervisor = module.Supervisor(child, 1)
original_readlink = module.os.readlink
try:
    module.os.readlink = lambda path: f"{sccache} (deleted)" if path == f"/proc/{cache.pid}/exe" else original_readlink(path)
    supervisor.discover()
    assert cache.pid not in supervisor.owned
    child.terminate()
    child.wait()
    assert supervisor.cleanup()
    assert cache.poll() is None
finally:
    module.os.readlink = original_readlink
    cache.terminate()
    cache.wait()
    child.terminate()
    child.wait()
    supervisor.close()
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(&script)
        .output()
        .expect("run idle authenticated sccache cleanup harness");

    assert!(
        output.status.success(),
        "idle authenticated sccache must not block cleanup proof: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn cargo_test_wrapper_timeout_reaps_a_term_ignoring_descendant() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.sh");
    let fixture = tempfile::tempdir().expect("create timeout-tree fixture");
    let identity_file = fixture.path().join("descendant-identity");
    let identity_path = identity_file.to_str().expect("UTF-8 identity path");
    let mut command = Command::new("/bin/bash");
    command
        .arg(&script)
        .args(["/bin/bash", "-c"])
        .arg(
            r#"
                trap 'exit 0' TERM
                /bin/bash -c '
                    trap "" TERM
                    pid="$BASHPID"
                    start_time="$(awk "{print \$22}" "/proc/${pid}/stat")"
                    printf "%s %s\\n" "$pid" "$start_time" >"${IDENTITY_FILE:?}"
                    while :; do printf x; done
                ' &
                wait
            "#,
        )
        .env("IDENTITY_FILE", identity_path)
        .current_dir(repo_root())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.env("MEMPAL_CARGO_TEST_TIMEOUT_SECS", "1");
    command.env("MEMPAL_CARGO_TEST_KILL_GRACE_SECS", "1");
    let mut wrapper = command.spawn().expect("spawn timeout wrapper");
    wait_for_file(
        &identity_file,
        Duration::from_secs(2),
        "term-ignoring descendant identity",
    );
    let identity_record = fs::read_to_string(&identity_file).expect("read descendant identity");

    let deadline = Instant::now() + Duration::from_secs(4);
    let status = loop {
        if let Some(status) = wrapper.try_wait().expect("poll timeout wrapper") {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "timeout wrapper did not return within the bounded test deadline"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(status.code(), Some(124));

    let identity = local_gate_pid_safety::recorded_process_identity(&identity_record);
    let descendant = capture_recorded_process(identity)
        .expect("re-verify descendant identity after wrapper exit");
    let descendant_running = match descendant {
        Some(descendant) => {
            let running = descendant
                .is_running()
                .expect("inspect descendant liveness");
            if running {
                descendant
                    .send_signal(libc::SIGKILL)
                    .expect("clean up surviving descendant");
            }
            running
        }
        None => false,
    };
    wrapper.wait().expect("reap timeout wrapper");
    assert!(
        !descendant_running,
        "timeout wrapper returned while its owned descendant was still alive"
    );
}

#[cfg(unix)]
#[test]
fn cargo_test_wrapper_timeout_reaps_nested_setsid_descendant_and_drains_pipes() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.sh");
    let fixture = tempfile::tempdir().expect("create nested-session fixture");
    let identity_file = fixture.path().join("descendant-identity");
    let command = r#"
        setsid --wait /bin/bash -c '
            trap "" TERM
            pid="$BASHPID"
            start_time="$(awk "{print \$22}" "/proc/${pid}/stat")"
            printf "%s %s\\n" "$pid" "$start_time" >"${IDENTITY_FILE:?}"
            printf "nested stdout\\n"
            printf "nested stderr\\n" >&2
            while :; do :; done
        ' &
        wait
    "#;
    let (mut wrapper, stdout_done, stderr_done) =
        spawn_piped_wrapper(&script, command, "1", &identity_file);
    wait_for_file(
        &identity_file,
        Duration::from_secs(2),
        "nested-session descendant identity",
    );
    let identity_record = fs::read_to_string(&identity_file).expect("read nested identity");

    let status = wait_for_wrapper(
        &mut wrapper,
        Duration::from_secs(4),
        "timeout wrapper with nested session",
    );
    let running_before_cleanup = recorded_process_is_running(&identity_record);
    let stdout_before_cleanup = stdout_done.recv_timeout(Duration::from_millis(300)).ok();
    let stderr_before_cleanup = stderr_done.recv_timeout(Duration::from_millis(300)).ok();
    let pipes_drained_before_cleanup =
        stdout_before_cleanup.is_some() && stderr_before_cleanup.is_some();
    if running_before_cleanup {
        kill_recorded_process(&identity_record);
    }
    let stdout = receive_pipe(&stdout_done, stdout_before_cleanup);
    let stderr = receive_pipe(&stderr_done, stderr_before_cleanup);

    assert_eq!(status.code(), Some(124));
    assert!(
        !running_before_cleanup,
        "timeout wrapper returned while an escaped owned descendant was alive"
    );
    assert!(
        pipes_drained_before_cleanup,
        "timeout wrapper must drain inherited stdout/stderr before returning"
    );
    assert!(String::from_utf8_lossy(&stdout).contains("nested stdout"));
    assert!(String::from_utf8_lossy(&stderr).contains("nested stderr"));
}

#[cfg(unix)]
#[test]
fn cargo_test_wrapper_reaps_descendant_after_leader_exits_and_drains_pipes() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.sh");
    let fixture = tempfile::tempdir().expect("create early-exit fixture");
    let identity_file = fixture.path().join("descendant-identity");
    let command = r#"
        setsid /bin/bash -c '
            pid="$BASHPID"
            start_time="$(awk "{print \$22}" "/proc/${pid}/stat")"
            printf "%s %s\\n" "$pid" "$start_time" >"${IDENTITY_FILE:?}"
            printf "early stdout\\n"
            printf "early stderr\\n" >&2
            while :; do :; done
        ' &
        while [[ ! -s "${IDENTITY_FILE:?}" ]]; do /bin/sleep 0.01; done
        exit 0
    "#;
    let (mut wrapper, stdout_done, stderr_done) =
        spawn_piped_wrapper(&script, command, "10", &identity_file);
    wait_for_file(
        &identity_file,
        Duration::from_secs(2),
        "early-exit descendant identity",
    );
    let identity_record = fs::read_to_string(&identity_file).expect("read early-exit identity");

    let status = wait_for_wrapper(
        &mut wrapper,
        Duration::from_secs(4),
        "wrapper with an early-exiting leader",
    );
    let running_before_cleanup = recorded_process_is_running(&identity_record);
    let stdout_before_cleanup = stdout_done.recv_timeout(Duration::from_millis(300)).ok();
    let stderr_before_cleanup = stderr_done.recv_timeout(Duration::from_millis(300)).ok();
    let pipes_drained_before_cleanup =
        stdout_before_cleanup.is_some() && stderr_before_cleanup.is_some();
    if running_before_cleanup {
        kill_recorded_process(&identity_record);
    }
    let stdout = receive_pipe(&stdout_done, stdout_before_cleanup);
    let stderr = receive_pipe(&stderr_done, stderr_before_cleanup);

    assert_eq!(status.code(), Some(0));
    assert!(
        !running_before_cleanup,
        "wrapper returned after its leader but left an owned descendant alive"
    );
    assert!(
        pipes_drained_before_cleanup,
        "wrapper must drain inherited stdout/stderr before returning"
    );
    assert!(String::from_utf8_lossy(&stdout).contains("early stdout"));
    assert!(String::from_utf8_lossy(&stderr).contains("early stderr"));
}

#[cfg(unix)]
#[test]
fn cargo_test_wrapper_reaps_non_utf8_task_name_after_leader_exits() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.sh");
    let fixture = tempfile::tempdir().expect("create non-UTF-8 task-name fixture");
    let identity_file = fixture.path().join("descendant-identity");
    let command = r#"
        setsid /usr/bin/python3 -c '
import ctypes
import os
import signal
import time

ctypes.CDLL(None).prctl(15, b"\xff", 0, 0, 0)
signal.signal(signal.SIGTERM, signal.SIG_IGN)
pid = os.getpid()
fields = open(f"/proc/{pid}/stat", "rb").read().rpartition(b") ")[2].split()
open(os.environ["IDENTITY_FILE"], "wb").write(str(pid).encode() + b" " + fields[19] + b"\n")
while True:
    time.sleep(0.01)
        ' &
        while [[ ! -s "${IDENTITY_FILE:?}" ]]; do /bin/sleep 0.01; done
        exit 0
    "#;
    let (mut wrapper, _stdout_done, _stderr_done) =
        spawn_piped_wrapper(&script, command, "10", &identity_file);
    wait_for_file(
        &identity_file,
        Duration::from_secs(2),
        "non-UTF-8 task-name descendant identity",
    );
    let identity_record = fs::read_to_string(&identity_file).expect("read non-UTF-8 identity");

    let status = wait_for_wrapper(
        &mut wrapper,
        Duration::from_secs(4),
        "wrapper with a non-UTF-8 task-name descendant",
    );
    let running_before_cleanup = recorded_process_is_running(&identity_record);
    if running_before_cleanup {
        kill_recorded_process(&identity_record);
    }

    assert_eq!(status.code(), Some(0));
    assert!(
        !running_before_cleanup,
        "wrapper returned success while its non-UTF-8 owned descendant was alive"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_rejects_an_interposed_pidfd_during_descendant_adoption() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import subprocess
import sys

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

child = subprocess.Popen(["/bin/sleep", "10"], start_new_session=True)
supervisor = module.Supervisor(child, 1)
expected = module.Snapshot(module.Identity(424242, 10), supervisor.supervisor_pid, 424242, 424242, "S")
interposed = module.Snapshot(module.Identity(424242, 11), supervisor.supervisor_pid, 424242, 424242, "S")
opened = []
closed = []
original_open = module.os.pidfd_open
original_close = module.os.close
original_read_snapshot = module.read_snapshot
try:
    module.os.pidfd_open = lambda pid, flags: opened.append((pid, flags)) or 91
    module.os.close = closed.append
    module.read_snapshot = lambda pid: interposed
    supervisor._adopt(expected)
    assert 424242 not in supervisor.owned
    assert supervisor.ownership_uncertain
    assert opened == [(424242, 0)]
    assert closed == [91]
finally:
    for handle in supervisor.owned.values():
        if handle.pidfd == 91:
            handle.pidfd = None
    module.os.pidfd_open = original_open
    module.os.close = original_close
    module.read_snapshot = original_read_snapshot
    supervisor.close()
    child.terminate()
    child.wait()
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(&script)
        .output()
        .expect("run interposed-pidfd adoption harness");

    assert!(
        output.status.success(),
        "interposed pidfd must be rejected: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_rejects_reused_parent_identity_for_adoption() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import subprocess
import sys

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

child = subprocess.Popen(["/bin/sleep", "10"], start_new_session=True)
supervisor = module.Supervisor(child, 1)
stale_parent = module.Identity(100, 1)
current_parent = module.Snapshot(module.Identity(100, 2), supervisor.supervisor_pid, 100, 100, "S")
unrelated_child = module.Snapshot(module.Identity(200, 3), 100, 200, 200, "S")
supervisor.owned[100] = module.OwnedProcess(stale_parent, None, True)
supervisor.seen_identities[100] = stale_parent
original_scan = module.scan_snapshots
original_open = module.open_pidfd
try:
    module.scan_snapshots = lambda: {100: current_parent, 200: unrelated_child}
    module.open_pidfd = lambda identity: (_ for _ in ()).throw(AssertionError(identity))
    supervisor.discover()
    assert 100 not in supervisor.owned
    assert 200 not in supervisor.owned
    assert supervisor.ownership_uncertain
finally:
    module.scan_snapshots = original_scan
    module.open_pidfd = original_open
    supervisor.close()
    child.terminate()
    child.wait()
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(&script)
        .output()
        .expect("run reused-parent adoption harness");

    assert!(
        output.status.success(),
        "reused parent must not authorize adoption: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_proves_cleanup_when_a_reaped_owned_pid_is_reused() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import subprocess
import sys

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

child = subprocess.Popen(["/bin/sleep", "10"], start_new_session=True)
supervisor = module.Supervisor(child, 1)
stale = module.Identity(424242, 1)
reused = module.Snapshot(module.Identity(424242, 2), 1, 424242, 424242, "S")
supervisor.owned[424242] = module.OwnedProcess(stale, None, True)
supervisor.seen_identities[424242] = stale
original_scan = module.scan_snapshots
try:
    module.scan_snapshots = lambda: {424242: reused}
    supervisor.discover()
    assert 424242 not in supervisor.owned
    assert not supervisor.ownership_uncertain
finally:
    module.scan_snapshots = original_scan
    supervisor.close()
    child.terminate()
    child.wait()
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(&script)
        .output()
        .expect("run reused-PID cleanup-proof harness");

    assert!(
        output.status.success(),
        "a reused PID proves the owned identity exited: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_limits_proc_discovery_for_a_successful_short_child() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import subprocess
import sys

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
module.ensure_linux_subreaper()

calls = 0
scan_snapshots = module.scan_snapshots
def counted_scan_snapshots():
    global calls
    calls += 1
    return scan_snapshots()
module.scan_snapshots = counted_scan_snapshots

child = subprocess.Popen(["/bin/sleep", "0.15"], start_new_session=True)
supervisor = module.Supervisor(child, 1)
assert supervisor.run(1) == 0
print(calls)
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(&script)
        .output()
        .expect("run instrumented timeout wrapper");

    assert!(
        output.status.success(),
        "instrumented timeout wrapper failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let scans = String::from_utf8(output.stdout)
        .expect("UTF-8 scan count")
        .trim()
        .parse::<usize>()
        .expect("numeric scan count");
    assert!(
        scans <= 3,
        "successful short child performed {scans} whole-proc discovery scans"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_throttles_proc_discovery_during_cleanup_graces() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import subprocess
import sys

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

child = subprocess.Popen(["/bin/sleep", "10"], start_new_session=True)
supervisor = module.Supervisor(child, 0.03)
calls = 0
clock = [0.0]
original_monotonic = module.time.monotonic
original_sleep = module.time.sleep

def discover():
    global calls
    calls += 1
    return {}

supervisor.discover = discover
supervisor.signal_owned = lambda signum, snapshots: True
supervisor.reap_owned_children = lambda: None
supervisor.live_status = lambda snapshots: (True, False)
module.time.monotonic = lambda: clock[0]
module.time.sleep = lambda seconds: clock.__setitem__(0, clock[0] + seconds)
try:
    assert not supervisor.cleanup()
    assert calls <= 3, calls
finally:
    module.time.monotonic = original_monotonic
    module.time.sleep = original_sleep
    supervisor.close()
    child.terminate()
    child.wait()
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(&script)
        .output()
        .expect("run cleanup discovery harness");

    assert!(
        output.status.success(),
        "cleanup must throttle whole-proc discovery: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
