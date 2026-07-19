use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use super::*;

const CLOSED_STDIO_CASE_ENV: &str = "MEMPAL_SUPERVISOR_CLOSED_STDIO_CASE";
const CLOSED_STDIO_FIXTURE_TEST: &str =
    "admission_supervisor::stdio_regression_tests::closed_parent_stdio_fixture";

struct BoundedFixtureProcess {
    child: Option<Child>,
}

impl BoundedFixtureProcess {
    fn spawn(closed_fd: libc::c_int, mode: &str) -> Self {
        let executable = std::env::current_exe().expect("current test executable");
        let mut command = Command::new(executable);
        command
            .args([
                "--exact",
                CLOSED_STDIO_FIXTURE_TEST,
                "--nocapture",
                "--test-threads=1",
            ])
            .env(CLOSED_STDIO_CASE_ENV, format!("{closed_fd}:{mode}"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // SAFETY: the closure executes only in the fork child and calls only async-signal-safe
        // libc functions before exec. It makes the direct child its own process-group leader so
        // timeout cleanup can fence the helper and every descendant it owns.
        unsafe {
            command.pre_exec(move || {
                if libc::setpgid(0, 0) != 0 {
                    libc::_exit(127);
                }
                Ok(())
            });
        }
        Self {
            child: Some(command.spawn().expect("spawn closed-stdio fixture")),
        }
    }

    fn wait_output(mut self, timeout: Duration) -> Output {
        let deadline = Instant::now() + timeout;
        loop {
            let child = self.child.as_mut().expect("fixture child retained");
            if child.try_wait().expect("poll bounded fixture").is_some() {
                return self
                    .child
                    .take()
                    .expect("fixture child retained")
                    .wait_with_output()
                    .expect("collect fixture output");
            }
            if Instant::now() >= deadline {
                self.kill_and_reap();
                panic!("closed-stdio fixture exceeded {timeout:?}");
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn kill_and_reap(&mut self) {
        let Some(mut child) = self.child.take() else {
            return;
        };
        if child
            .try_wait()
            .expect("poll fixture before cleanup")
            .is_none()
        {
            let pid = child.id() as libc::pid_t;
            // SAFETY: this guard retains the direct child unreaped and made it the leader of its
            // own group in pre_exec, so negative `pid` names only the fixture process group.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
        let _ = child.wait_with_output();
    }
}

impl Drop for BoundedFixtureProcess {
    fn drop(&mut self) {
        self.kill_and_reap();
    }
}

#[test]
fn closed_parent_stdio_fixture() {
    let Ok(case) = std::env::var(CLOSED_STDIO_CASE_ENV) else {
        return;
    };
    let (closed_fd, mode) = case.split_once(':').expect("closed-stdio case format");
    let closed_fd: libc::c_int = closed_fd.parse().expect("numeric closed fd");
    assert!((libc::STDIN_FILENO..=libc::STDERR_FILENO).contains(&closed_fd));
    // SAFETY: the helper's test harness is fully initialized before it closes one standard FD;
    // the close changes only this isolated process's descriptor table before SpawnSpec allocates
    // any source descriptors.
    assert_eq!(unsafe { libc::close(closed_fd) }, 0);
    // SAFETY: F_GETFD only queries the descriptor table and verifies the close above persisted
    // before the fixture constructs its actual spawn resources.
    assert_eq!(unsafe { libc::fcntl(closed_fd, libc::F_GETFD) }, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EBADF),
        "outer helper did not preserve closed fd {closed_fd} through exec"
    );
    match mode {
        "capture" => capture_fixture_with_closed_parent_stdio(),
        "piped-input" => piped_input_fixture_with_closed_parent_stdio(),
        other => panic!("unknown closed-stdio mode {other}"),
    }
}

#[test]
fn closed_parent_stdio_preserves_capture_and_piped_input_channels() {
    for closed_fd in libc::STDIN_FILENO..=libc::STDERR_FILENO {
        for mode in ["capture", "piped-input"] {
            let output =
                BoundedFixtureProcess::spawn(closed_fd, mode).wait_output(Duration::from_secs(5));
            assert!(
                output.status.success(),
                "closed fd {closed_fd}, {mode}: status={:?}, stdout={}, stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }
    }
}

fn capture_fixture_with_closed_parent_stdio() {
    let mut spec = SpawnSpec::new("/bin/sh").expect("absolute shell executable");
    spec.args([
        "-c",
        "set -e; cat >/dev/null; printf capture-stdout; printf capture-stderr >&2",
    ]);
    let output = DeadlineChild::output(spec, Duration::from_secs(2)).expect("capture output");
    assert!(output.success(), "capture fixture status: {output:?}");
    assert!(!output.timed_out, "capture fixture timed out");
    assert_eq!(output.stdout, b"capture-stdout");
    assert_eq!(output.stderr, b"capture-stderr");
    assert!(output.cleanup.errors.is_empty(), "{:#?}", output.cleanup);
}

fn piped_input_fixture_with_closed_parent_stdio() {
    let mut spec = SpawnSpec::new("/bin/sh").expect("absolute shell executable");
    spec.args([
        "-c",
        "set -e; IFS= read -r line; test \"$line\" = piped-input; if IFS= read -r extra; then exit 31; fi; printf hidden-stdout; printf hidden-stderr >&2",
    ])
    .stdio(StdioMode::PipedInput);
    let mut child = DeadlineChild::spawn(spec, Duration::from_secs(2)).expect("spawn piped-input");
    child
        .write_stdin(b"piped-input\n", Duration::from_secs(1))
        .expect("write piped input");
    child.close_stdin();
    let diagnostic = child
        .wait_for_exit_diagnostic(Duration::from_secs(2))
        .expect("observe piped-input exit");
    let cleanup = child.force_kill().expect_complete("reap piped-input child");
    assert!(
        diagnostic.contains("status=0"),
        "piped input or EOF mapping failed: {diagnostic}"
    );
    assert!(cleanup.kill_fence_sent);
    assert!(cleanup.errors.is_empty(), "{cleanup:#?}");
}
