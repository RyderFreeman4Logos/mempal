use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Output};
use std::thread;
use std::time::{Duration, Instant};

const PROCESS_GROUP_TERM_TIMEOUT: Duration = Duration::from_millis(250);
const PROCESS_GROUP_KILL_TIMEOUT: Duration = Duration::from_millis(250);

pub(crate) struct GateChild {
    child: Option<Child>,
}

pub(crate) fn spawn_in_own_session(command: &mut Command) -> io::Result<Child> {
    // SAFETY: The post-fork closure invokes only async-signal-safe `setsid` before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command.spawn()
}

impl GateChild {
    pub(crate) fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    pub(crate) fn wait_with_timeout(&mut self, timeout: Duration) -> io::Result<Output> {
        let deadline = Instant::now() + timeout;
        loop {
            let exited = self
                .child
                .as_mut()
                .expect("gate child already reaped")
                .try_wait()?
                .is_some();
            if exited {
                return terminate_and_collect(
                    self.child.take().expect("gate child already reaped"),
                );
            }
            if Instant::now() >= deadline {
                let output =
                    terminate_and_collect(self.child.take().expect("gate child already reaped"))?;
                panic!(
                    "child did not exit within {timeout:?}; stdout={}, stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for GateChild {
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            let _ = reap_owned_child(child);
        }
    }
}

pub(crate) fn reap_owned_child(mut child: Child) -> io::Result<()> {
    terminate_owned_process_group(&mut child)?;
    child.wait_with_output()?;
    Ok(())
}

fn terminate_and_collect(mut child: Child) -> io::Result<Output> {
    terminate_owned_process_group(&mut child)?;
    child.wait_with_output()
}

fn terminate_owned_process_group(child: &mut Child) -> io::Result<()> {
    let process_group_id = child.id() as i32;
    signal_process_group(process_group_id, libc::SIGTERM)?;
    let _ = wait_for_child_exit(child, PROCESS_GROUP_TERM_TIMEOUT)?;
    if wait_for_process_group_exit(process_group_id, PROCESS_GROUP_TERM_TIMEOUT)? {
        return Ok(());
    }

    signal_process_group(process_group_id, libc::SIGKILL)?;
    let _ = wait_for_child_exit(child, PROCESS_GROUP_KILL_TIMEOUT)?;
    if wait_for_process_group_exit(process_group_id, PROCESS_GROUP_KILL_TIMEOUT)? {
        return Ok(());
    }

    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "gate process group did not exit after SIGKILL",
    ))
}

fn signal_process_group(process_group_id: i32, signal: i32) -> io::Result<()> {
    // SAFETY: Fixtures call `setsid` before exec, so this negative PID targets only the
    // owned child session's process group. `ESRCH` means the group already exited.
    unsafe {
        if libc::kill(-process_group_id, signal) == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
    }
    Ok(())
}

fn wait_for_process_group_exit(process_group_id: i32, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        // SAFETY: Signal zero performs no mutation and uses the same owned process group ID
        // that `signal_process_group` uses for termination.
        let exists = unsafe { libc::kill(-process_group_id, 0) == 0 };
        if !exists {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(true);
            }
            return Err(error);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_child_exit(child: &mut Child, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

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

    fn spawn_pipe_holding_child(ready_file: &Path, pid_file: &Path) -> Child {
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                r#"
                    : >"${READY_FILE:?}"
                    printf '%s\n' "${BASHPID}" >"${PID_FILE:?}"
                    ( trap '' TERM; exec /bin/sleep 60 ) &
                    while true; do sleep 0.01; done
                "#,
            ])
            .env("READY_FILE", ready_file)
            .env("PID_FILE", pid_file)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        spawn_in_own_session(&mut command).expect("spawn isolated fixture")
    }

    #[test]
    fn gate_child_wait_timeout_reaps_descendants_that_hold_pipes() {
        let fixture = tempfile::tempdir().expect("create pipe-holder fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let mut gate = GateChild::new(spawn_pipe_holding_child(&ready_file, &pid_file));
        wait_for_file(&ready_file, Duration::from_secs(2), "pipe-holder fixture");

        let started = Instant::now();
        let result = catch_unwind(AssertUnwindSafe(|| {
            let _ = gate.wait_with_timeout(Duration::from_millis(50));
        }));

        assert!(result.is_err(), "a gate timeout must report a failure");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GateChild timeout cleanup must not hang on inherited pipes"
        );
    }

    #[test]
    fn gate_child_drop_reaps_descendants_that_hold_pipes() {
        let fixture = tempfile::tempdir().expect("create drop pipe-holder fixture");
        let ready_file = fixture.path().join("ready");
        let pid_file = fixture.path().join("pid");
        let started = Instant::now();
        {
            let gate = GateChild::new(spawn_pipe_holding_child(&ready_file, &pid_file));
            wait_for_file(&ready_file, Duration::from_secs(2), "pipe-holder fixture");
            drop(gate);
        }

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "GateChild Drop cleanup must not hang on inherited pipes"
        );
    }

    #[test]
    fn rest_gate_fuser_diagnostic_cannot_outlive_lock_budget() {
        let fixture = tempfile::tempdir().expect("create fuser-timeout fixture");
        let bin_dir = fixture.path().join("bin");
        fs::create_dir(&bin_dir).expect("create fixture bin directory");
        symlink(
            repo_root().join("tests/fixtures/local-gate-command-proxy.sh"),
            bin_dir.join("fuser"),
        )
        .expect("link fuser proxy");
        let target = fixture.path().join("target");
        fs::create_dir(&target).expect("create isolated target");
        let target = fs::canonicalize(target).expect("canonical isolated target");
        let mut lock_file = target.as_os_str().to_os_string();
        lock_file.push(".lock");
        let lock_file = PathBuf::from(lock_file);
        let holder_ready_file = fixture.path().join("holder-ready");
        let mut holder_command = Command::new("/bin/bash");
        holder_command
            .args([
                "-c",
                r#"
                    exec {lock_fd}>"${LOCK_FILE:?}"
                    flock "${lock_fd}"
                    : >"${HOLDER_READY_FILE:?}"
                    exec /bin/sleep 60
                "#,
            ])
            .env("LOCK_FILE", &lock_file)
            .env("HOLDER_READY_FILE", &holder_ready_file)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let _holder = GateChild::new(
            spawn_in_own_session(&mut holder_command).expect("spawn isolated lock holder"),
        );
        wait_for_file(&holder_ready_file, Duration::from_secs(2), "lock holder");

        let inherited_path = std::env::var_os("PATH").expect("PATH is set");
        let path = std::env::join_paths(
            std::iter::once(bin_dir).chain(std::env::split_paths(&inherited_path)),
        )
        .expect("construct fixture PATH");
        let fuser_ready_file = fixture.path().join("fuser-ready");
        let fuser_pid_file = fixture.path().join("fuser.pid");
        let mut command = Command::new("/bin/bash");
        command
            .arg(repo_root().join("scripts/gates/rest-tests.sh"))
            .current_dir(repo_root())
            .env("PATH", path)
            .env("REST_GATE_DRY_RUN", "1")
            .env("REST_GATE_LOCK_TIMEOUT_SECS", "1")
            .env("REST_GATE_TARGET_DIR", &target)
            .env("REST_TEST_TARGETS_PER_BATCH", "999")
            .env("REST_GATE_FUSER_NEVER_RETURN_READY_FILE", &fuser_ready_file)
            .env("REST_GATE_FUSER_PID_FILE", &fuser_pid_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut gate =
            GateChild::new(spawn_in_own_session(&mut command).expect("spawn isolated REST gate"));
        wait_for_file(&fuser_ready_file, Duration::from_secs(2), "stalled fuser");

        let started = Instant::now();
        let output = gate
            .wait_with_timeout(Duration::from_secs(3))
            .expect("reap REST gate");

        assert_eq!(output.status.code(), Some(75));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the fuser diagnostic exceeded the advertised lock deadline: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
