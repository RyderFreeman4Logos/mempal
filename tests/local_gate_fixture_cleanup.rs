use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const ALLOC_BYTES: u64 = 314_572_800;
const WRAPPER_WAIT: Duration = Duration::from_secs(20);

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

struct ChildGuard(Option<Child>);

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self(Some(child))
    }

    fn wait_timeout(&mut self, timeout: Duration, description: &str) -> std::process::ExitStatus {
        let deadline = Instant::now() + timeout;
        let child = self.0.as_mut().expect("wrapper still owned");
        loop {
            if let Some(status) = child.try_wait().expect("poll wrapper") {
                let _ = self.0.take();
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "{description} did not exit in time"
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn wait_with_output_timeout(
        &mut self,
        timeout: Duration,
        description: &str,
    ) -> std::process::Output {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.0.as_mut().expect("wrapper still owned");
            if child.try_wait().expect("poll wrapper").is_some() {
                return self
                    .0
                    .take()
                    .expect("completed wrapper")
                    .wait_with_output()
                    .expect("collect wrapper output");
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

fn wait_for_file(path: &Path, timeout: Duration, description: &str) {
    let deadline = Instant::now() + timeout;
    while !path.exists() && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(path.exists(), "{description} did not become ready");
}

fn child_script() -> &'static str {
    r#"
import os
import signal
import sys
import time

root = os.environ["TMPDIR"]
blob = os.path.join(root, "blob")
fd = os.open(blob, os.O_CREAT | os.O_RDWR, 0o600)
os.posix_fallocate(fd, 0, 314572800)
os.close(fd)
st = os.stat(blob)
marker = os.environ["MEMPAL_FIXTURE_MARKER"]
with open(marker, "w", encoding="utf-8") as handle:
    handle.write(f"{root}\n{st.st_blocks * 512}\n")
    handle.flush()
    os.fsync(handle.fileno())
mode = sys.argv[1]
if mode == "normal":
    raise SystemExit(0)
if mode == "error":
    raise SystemExit(23)
if mode == "timeout":
    time.sleep(60)
    raise SystemExit(0)
if mode == "cancel":
    time.sleep(60)
    raise SystemExit(0)
if mode == "child-kill":
    os.kill(os.getpid(), signal.SIGKILL)
raise SystemExit(2)
"#
}

struct Case {
    name: &'static str,
    mode: &'static str,
    expected: i32,
    timeout_secs: &'static str,
    cancel: bool,
    assert_blocks: bool,
}

fn physical_bytes(path: &Path) -> u64 {
    let meta = fs::symlink_metadata(path).expect("stat leftover path");
    meta.blocks() * 512
}

fn run_case(base: &Path, sentinel: &Path, case: &Case) {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let marker = base.join(format!("{}.marker", case.name));
    let _ = fs::remove_file(&marker);
    let mut command = Command::new("python3");
    command
        .arg(&script)
        .args(["python3", "-c", child_script(), case.mode])
        .current_dir(repo_root())
        .env("TMPDIR", base)
        .env("TMP", base)
        .env("TEMP", base)
        .env("MEMPAL_FIXTURE_MARKER", &marker)
        .env("MEMPAL_CARGO_TEST_TIMEOUT_SECS", case.timeout_secs)
        .env("MEMPAL_CARGO_TEST_KILL_GRACE_SECS", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut wrapper = ChildGuard::new(command.spawn().expect("spawn timeout wrapper"));
    wait_for_file(
        &marker,
        Duration::from_secs(10),
        &format!("{} marker", case.name),
    );
    if case.cancel {
        let pid = i32::try_from(wrapper.0.as_ref().expect("wrapper pid").id())
            .expect("wrapper pid fits i32");
        // SAFETY: pid is this test's live wrapper from spawn(); SIGTERM is catchable.
        let _ = unsafe { libc::kill(pid, libc::SIGTERM) };
    }
    let status = wrapper.wait_timeout(WRAPPER_WAIT, case.name);
    let marker_text = fs::read_to_string(&marker).expect("read marker");
    let mut lines = marker_text.lines();
    let reported_root = PathBuf::from(lines.next().expect("marker root"));
    let peak: u64 = lines
        .next()
        .expect("marker peak")
        .parse()
        .expect("numeric peak");
    let leftover = if reported_root.exists() {
        physical_bytes(&reported_root)
    } else {
        0
    };
    eprintln!(
        "{} status={:?} peak={peak} end={leftover} root={}",
        case.name,
        status.code(),
        reported_root.display()
    );
    assert_eq!(status.code(), Some(case.expected), "{}", case.name);
    assert_eq!(
        reported_root.parent().expect("owned root parent"),
        base,
        "{} root must be a direct child of the configured base",
        case.name
    );
    assert_ne!(reported_root, base, "{} must not reuse the base", case.name);
    assert!(
        sentinel.exists(),
        "{} sibling sentinel must survive",
        case.name
    );
    assert!(
        !reported_root.exists(),
        "{} exact root leftover peak={peak} leftover_bytes={leftover}",
        case.name
    );
    assert_eq!(leftover, 0, "{} post-cleanup allocated bytes", case.name);
    if case.assert_blocks {
        assert!(
            peak >= ALLOC_BYTES,
            "{} physical peak {peak} below {ALLOC_BYTES}",
            case.name
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_owns_and_cleans_real_fixture_root() {
    let base = tempfile::tempdir().expect("parent-owned wrapper base");
    let sentinel = base.path().join("sibling-sentinel");
    File::create(&sentinel)
        .expect("sibling sentinel")
        .write_all(b"keep")
        .expect("write sentinel");

    let cases = [
        Case {
            name: "normal",
            mode: "normal",
            expected: 0,
            timeout_secs: "30",
            cancel: false,
            assert_blocks: true,
        },
        Case {
            name: "error",
            mode: "error",
            expected: 23,
            timeout_secs: "30",
            cancel: false,
            assert_blocks: true,
        },
        Case {
            name: "timeout",
            mode: "timeout",
            expected: 124,
            timeout_secs: "2",
            cancel: false,
            assert_blocks: true,
        },
        Case {
            name: "cancel",
            mode: "cancel",
            expected: 143,
            timeout_secs: "30",
            cancel: true,
            assert_blocks: false,
        },
        Case {
            name: "child-kill",
            mode: "child-kill",
            expected: 137,
            timeout_secs: "30",
            cancel: false,
            assert_blocks: false,
        },
    ];
    for case in cases {
        run_case(base.path(), &sentinel, &case);
        assert!(
            sentinel.exists(),
            "sentinel must remain between sequential cases"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_accepts_symlinked_tmpdir() {
    let base = tempfile::tempdir().expect("symlink fixture base");
    let target = base.path().join("target");
    let alias = base.path().join("alias");
    fs::create_dir(&target).expect("create target tmpdir");
    std::os::unix::fs::symlink(&target, &alias).expect("create tmpdir symlink");

    let output = Command::new("python3")
        .arg(repo_root().join("scripts/gates/cargo-test-with-timeout.py"))
        .args(["python3", "-c", "raise SystemExit(0)"])
        .env("TMPDIR", &alias)
        .env("TMP", &alias)
        .env("TEMP", &alias)
        .output()
        .expect("run wrapper with symlinked tmpdir");

    assert!(
        output.status.success(),
        "symlinked TMPDIR rejected: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(alias.is_symlink(), "configured TMPDIR symlink must survive");
    assert_eq!(
        fs::read_dir(&target).expect("read target tmpdir").count(),
        0,
        "owned fixture root must be removed from the symlink target"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_cleans_intra_tree_hardlinks_and_preserves_external_alias() {
    let base = tempfile::tempdir().expect("hardlink wrapper base");
    let external = base.path().join("external");
    fs::create_dir(&external).expect("create external directory");
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let child = r#"
import os
import sys

root = os.environ["TMPDIR"]
source = os.environ["MEMPAL_HARDLINK_SOURCE"]
marker = os.environ["MEMPAL_FIXTURE_MARKER"]
with open(source, "wb") as handle:
    handle.write(b"external survives")
if sys.argv[1] == "same-directory":
    os.link(source, os.path.join(root, "alias-a"))
    os.link(source, os.path.join(root, "alias-b"))
else:
    nested = os.path.join(root, "nested")
    os.mkdir(nested, 0o700)
    os.link(source, os.path.join(root, "alias-a"))
    os.link(source, os.path.join(nested, "alias-b"))
with open(marker, "w", encoding="utf-8") as handle:
    handle.write(root)
    handle.flush()
    os.fsync(handle.fileno())
"#;

    for mode in ["same-directory", "cross-directory"] {
        let source = external.join(mode);
        let marker = base.path().join(format!("{mode}.marker"));
        let output = Command::new("python3")
            .arg(&script)
            .args(["python3", "-c", child, mode])
            .env("TMPDIR", base.path())
            .env("TMP", base.path())
            .env("TEMP", base.path())
            .env("MEMPAL_HARDLINK_SOURCE", &source)
            .env("MEMPAL_FIXTURE_MARKER", &marker)
            .output()
            .expect("run hardlink cleanup case");

        let owned_root = PathBuf::from(fs::read_to_string(&marker).expect("read root marker"));
        assert!(
            output.status.success(),
            "{mode} cleanup failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!owned_root.exists(), "{mode} owned root must be removed");
        assert_eq!(
            fs::read(&source).expect("read external hardlink"),
            b"external survives",
            "{mode} external alias contents"
        );
        assert_eq!(
            fs::metadata(&source)
                .expect("stat external hardlink")
                .nlink(),
            1,
            "{mode} cleanup must remove only in-tree aliases"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_rejects_external_link_count_drift() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import os
import sys
import tempfile

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

with tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"]) as base:
    root = os.path.join(base, "owned")
    leaf = os.path.join(root, "leaf")
    external = os.path.join(base, "external")
    os.mkdir(root, 0o700)
    with open(leaf, "wb") as handle:
        handle.write(b"preserve both aliases")
    identity = module.capture_fixture_identity(root)
    real_check = module.fixture_tree_stays_on_mount

    def check_then_link(root_fd, mount_id, check_cleanup):
        snapshot = real_check(root_fd, mount_id, check_cleanup)
        os.link(leaf, external)
        return snapshot

    module.fixture_tree_stays_on_mount = check_then_link
    try:
        removed = module.remove_owned_root(identity, module.time.monotonic() + 5, 0)
    finally:
        module.fixture_tree_stays_on_mount = real_check

    assert not removed
    assert os.path.exists(leaf)
    assert os.path.exists(external)
    assert os.stat(leaf).st_nlink == 2
    with open(external, "rb") as handle:
        assert handle.read() == b"preserve both aliases"
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(script)
        .env("TMPDIR", std::env::temp_dir())
        .env("TMP", std::env::temp_dir())
        .env("TEMP", std::env::temp_dir())
        .output()
        .expect("run external hardlink drift harness");

    assert!(
        output.status.success(),
        "external hardlink drift was not rejected: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_preserves_same_filesystem_bind_mount_contents() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import os
import subprocess
import sys
import tempfile

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

with tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"]) as base:
    root = os.path.join(base, "owned")
    child = os.path.join(root, "child")
    source = os.path.join(base, "external")
    sentinel = os.path.join(source, "sentinel")
    os.mkdir(root, 0o700)
    os.mkdir(child, 0o700)
    os.mkdir(source, 0o700)
    with open(sentinel, "w", encoding="utf-8") as handle:
        handle.write("keep")
    identity = module.capture_fixture_identity(root)
    subprocess.run(["mount", "--bind", source, child], check=True)
    try:
        assert os.stat(child).st_dev == os.stat(root).st_dev
        assert not module.remove_owned_root(identity, module.time.monotonic() + 5, 0)
        assert os.path.exists(sentinel), "cleanup crossed the bind mount"
    finally:
        subprocess.run(["umount", child], check=True)
"#;
    let output = Command::new("unshare")
        .args([
            "--user",
            "--map-root-user",
            "--mount",
            "python3",
            "-c",
            harness,
        ])
        .arg(script)
        .env("TMPDIR", std::env::temp_dir())
        .env("TMP", std::env::temp_dir())
        .env("TEMP", std::env::temp_dir())
        .output()
        .expect("run isolated bind-mount harness");

    assert!(
        output.status.success(),
        "bind-mount harness failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_preserves_exchanged_fixture_trees() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import os
import sys
import tempfile

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

def populated(path, marker):
    os.mkdir(path, 0o700)
    with open(os.path.join(path, marker), "w", encoding="utf-8") as handle:
        handle.write(marker)

def run_case(exchange_descendant):
    with tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"]) as base:
        root = os.path.join(base, "owned")
        held = os.path.join(base, "held")
        victim = os.path.join(base, "victim")
        exchange_marker = os.path.join(base, "exchange-complete")
        exchange_token = "descendant" if exchange_descendant else "root"
        assert not os.path.lexists(exchange_marker)
        populated(root, "owned-content")
        if exchange_descendant:
            os.mkdir(os.path.join(root, "child"), 0o700)
            with open(os.path.join(root, "child", "owned-child"), "w", encoding="utf-8") as handle:
                handle.write("owned")
        populated(victim, "victim-content")
        identity = module.capture_fixture_identity(root)
        real_check = module.fixture_tree_stays_on_mount

        def check_then_exchange(path, dev, check_cleanup):
            result = real_check(path, dev, check_cleanup)
            if exchange_descendant:
                os.rename(os.path.join(root, "child"), held)
                os.rename(victim, os.path.join(root, "child"))
            else:
                os.rename(root, held)
                os.rename(victim, root)
            with open(exchange_marker, "x", encoding="utf-8") as marker:
                marker.write(exchange_token)
                marker.flush()
                os.fsync(marker.fileno())
            return result

        module.fixture_tree_stays_on_mount = check_then_exchange
        try:
            removed = module.remove_owned_root(identity, module.time.monotonic() + 5, 0)
        finally:
            module.fixture_tree_stays_on_mount = real_check

        substitute = os.path.join(root, "child") if exchange_descendant else root
        return (
            open(exchange_marker, encoding="utf-8").read() == exchange_token,
            not removed,
            os.path.exists(os.path.join(substitute, "victim-content")),
            os.path.exists(held),
        )

results = [run_case(False), run_case(True)]
assert all(all(checks) for checks in results), results
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(script)
        .env("TMPDIR", std::env::temp_dir())
        .env("TMP", std::env::temp_dir())
        .env("TEMP", std::env::temp_dir())
        .output()
        .expect("run fixture exchange harness");
    assert!(
        output.status.success(),
        "fixture exchange harness failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_documents_unsupported_same_uid_final_exchanges() {
    let script = repo_root().join("scripts/gates/cargo-test-with-timeout.py");
    let harness = r#"
import importlib.util
import os
import sys
import tempfile

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)

def open_directory(path):
    return os.open(path, os.O_RDONLY | os.O_DIRECTORY | os.O_CLOEXEC)

def run_case(kind):
    with tempfile.TemporaryDirectory(dir=os.environ["TMPDIR"]) as base:
        root = os.path.join(base, "owned")
        held = os.path.join(base, "held")
        victim = os.path.join(base, "victim")
        exchange_marker = os.path.join(base, "exchange-complete")
        assert not os.path.lexists(exchange_marker)
        os.mkdir(root, 0o700)
        identity = module.capture_fixture_identity(root)
        exchanged = False

        def record_exchange():
            with open(exchange_marker, "x", encoding="utf-8") as marker:
                marker.write(kind)
                marker.flush()
                os.fsync(marker.fileno())

        if kind == "file":
            owned = os.path.join(root, "leaf")
            with open(owned, "wb") as handle:
                handle.write(b"owned")
            with open(victim, "wb") as handle:
                handle.write(b"victim")
            owned_fd = os.open(owned, os.O_PATH | os.O_CLOEXEC)
            victim_fd = os.open(victim, os.O_PATH | os.O_CLOEXEC)
            real_remove = module.os.unlink

            def exchange_then_remove(name, *, dir_fd):
                nonlocal exchanged
                if not exchanged:
                    os.rename(owned, held)
                    os.rename(victim, owned)
                    record_exchange()
                    exchanged = True
                return real_remove(name, dir_fd=dir_fd)

            module.os.unlink = exchange_then_remove
        else:
            owned = root if kind == "root" else os.path.join(root, "child")
            if kind == "child":
                os.mkdir(owned, 0o700)
            os.mkdir(victim, 0o700)
            owned_fd = open_directory(owned)
            victim_fd = open_directory(victim)
            real_remove = module.os.rmdir

            def exchange_then_remove(name, *, dir_fd):
                nonlocal exchanged
                final_name = "owned" if kind == "root" else "child"
                if not exchanged and name == final_name:
                    os.rename(owned, held)
                    os.rename(victim, owned)
                    record_exchange()
                    exchanged = True
                return real_remove(name, dir_fd=dir_fd)

            module.os.rmdir = exchange_then_remove

        try:
            removed = module.remove_owned_root(identity, module.time.monotonic() + 5, 0)
            owned_links = os.fstat(owned_fd).st_nlink
            victim_links = os.fstat(victim_fd).st_nlink
            exchange_observed = open(exchange_marker, encoding="utf-8").read() == kind
        finally:
            if kind == "file":
                module.os.unlink = real_remove
            else:
                module.os.rmdir = real_remove
            os.close(owned_fd)
            os.close(victim_fd)
    assert not os.path.lexists(base), base
    return exchange_observed, removed, owned_links, victim_links

# These are counterexamples for the explicitly unsupported adversarial same-UID
# threat, not safety-pass assertions. The original inode survives and the
# substituted victim is removed at each final pathname mutation.
results = {kind: run_case(kind) for kind in ("file", "child", "root")}
for kind, expected_removed in (("file", False), ("child", True), ("root", False)):
    exchanged, removed, owned_links, victim_links = results[kind]
    assert exchanged and removed is expected_removed, results
    assert owned_links > 0 and victim_links == 0, results
"#;
    let output = Command::new("python3")
        .args(["-c", harness])
        .arg(script)
        .env("TMPDIR", std::env::temp_dir())
        .env("TMP", std::env::temp_dir())
        .env("TEMP", std::env::temp_dir())
        .output()
        .expect("run final exchange counterexamples");
    assert!(
        output.status.success(),
        "final exchange counterexamples failed: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_test_wrapper_bounds_fixture_cleanup_depth_entries_deadline_and_signal() {
    const HARNESS: &str = r#"
import importlib.util
import os
import signal
import sys
import threading
import time

spec = importlib.util.spec_from_file_location("timeout_wrapper", sys.argv[1])
module = importlib.util.module_from_spec(spec)
sys.modules[spec.name] = module
spec.loader.exec_module(module)
mode = sys.argv[2]
base = os.environ["TMPDIR"]
root_marker = os.path.join(base, "root-marker")
ready = os.path.join(base, "cleanup-ready")
release = os.path.join(base, "cleanup-release")

child = r'''
import os
root = os.environ["TMPDIR"]
with open(os.environ["ROOT_MARKER"], "w", encoding="utf-8") as marker:
    marker.write(root)
mode = os.environ["CLEANUP_CASE"]
if mode == "deep":
    os.chdir(root)
    for _ in range(1100):
        os.mkdir("d", 0o700)
        os.chdir("d")
elif mode == "wide":
    for index in range(9):
        open(os.path.join(root, str(index)), "wb").close()
else:
    open(os.path.join(root, "leaf"), "wb").close()
'''

identity_module = sys.modules["fixture_cleanup_identity"]
cleanup_worker_module = sys.modules["fixture_cleanup_worker"]
if mode == "wide":
    identity_module.MAX_FIXTURE_ENTRIES = 8
elif mode == "deadline":
    real_snapshot = module.fixture_tree_stays_on_mount
    def delayed_snapshot(*args):
        time.sleep(2.1)
        return real_snapshot(*args)
    module.fixture_tree_stays_on_mount = delayed_snapshot
elif mode == "signal":
    real_remove = cleanup_worker_module._remove_snapshot
    def blocked_remove(*args):
        open(ready, "wb").close()
        while not os.path.exists(release):
            time.sleep(0.01)
        return real_remove(*args)
    cleanup_worker_module._remove_snapshot = blocked_remove
    def interrupt_cleanup():
        deadline = time.monotonic() + 5
        while not os.path.exists(ready) and time.monotonic() < deadline:
            time.sleep(0.01)
        assert os.path.exists(ready), "fixture removal did not reach barrier"
        os.kill(os.getpid(), signal.SIGTERM)
        open(release, "wb").close()
    controller = threading.Thread(target=interrupt_cleanup)
    controller.start()

os.environ["ROOT_MARKER"] = root_marker
os.environ["CLEANUP_CASE"] = mode
os.environ["MEMPAL_CARGO_TEST_KILL_GRACE_SECS"] = "1"
result = module.main(["python3", "-c", child])
if mode == "signal":
    controller.join(5)
    assert not controller.is_alive(), "signal controller did not stop"
expected = 143 if mode == "signal" else 125
assert result == expected, (mode, result)
root = open(root_marker, encoding="utf-8").read()
assert os.path.isdir(root), (mode, root)
before = os.listdir(root)
time.sleep(0.05)
assert os.listdir(root) == before, "cleanup continued after returning"
"#;

    for mode in ["deep", "wide", "deadline", "signal"] {
        let base = tempfile::tempdir().expect("bounded cleanup harness base");
        let mut command = Command::new("python3");
        command
            .args(["-c", HARNESS])
            .arg(repo_root().join("scripts/gates/cargo-test-with-timeout.py"))
            .arg(mode)
            .env("TMPDIR", base.path())
            .env("TMP", base.path())
            .env("TEMP", base.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = ChildGuard::new(command.spawn().expect("spawn cleanup harness"));
        let output = child.wait_with_output_timeout(Duration::from_secs(15), mode);
        assert!(
            output.status.success(),
            "{mode} cleanup harness failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
