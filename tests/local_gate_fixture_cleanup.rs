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
