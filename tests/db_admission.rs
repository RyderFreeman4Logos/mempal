use std::fs;
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, ChildStderr, ChildStdout, Command, ExitStatus, Stdio};
use std::sync::{Arc, Barrier, Condvar, Mutex, mpsc};
use std::time::{Duration, Instant};

use mempal::core::AsyncDb;
use mempal::core::db::Database;
use mempal::core::db_admission::{
    DbAdmissionConfig, DbAdmissionError, DbAdmissionRequest, DbHolderClass, ProfileDbAdmission,
};

const MIB: u64 = 1024 * 1024;
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(20);
const CHILD_TERM_GRACE: Duration = Duration::from_millis(100);
const CHILD_TERMINATION_TIMEOUT: Duration = Duration::from_secs(5);
const PIPE_DRAIN_BYTES_PER_POLL: usize = 64 * 1024;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

struct DeadlineOutput {
    status: Option<ExitStatus>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    timed_out: bool,
}

impl DeadlineOutput {
    fn success(&self) -> bool {
        !self.timed_out && self.status.is_some_and(|status| status.success())
    }
}

struct DeadlineChild {
    child: Option<Child>,
    process_group: i32,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
    exit_status: Option<ExitStatus>,
}

impl DeadlineChild {
    fn spawn(command: &mut Command) -> io::Result<Self> {
        let mut child = command.process_group(0).spawn()?;
        let process_group = child.id() as i32;
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let owned = Self {
            child: Some(child),
            process_group,
            stdout,
            stderr,
            exit_status: None,
        };
        if let Some(stdout) = owned.stdout.as_ref() {
            set_nonblocking(stdout)?;
        }
        if let Some(stderr) = owned.stderr.as_ref() {
            set_nonblocking(stderr)?;
        }
        Ok(owned)
    }

    fn output(command: &mut Command, timeout: Duration) -> io::Result<DeadlineOutput> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        Self::spawn(command)?.wait_for_output(timeout)
    }

    fn status(command: &mut Command, timeout: Duration) -> io::Result<DeadlineOutput> {
        Self::spawn(command)?.wait_for_output(timeout)
    }

    fn wait_for_output(mut self, timeout: Duration) -> io::Result<DeadlineOutput> {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let deadline = Instant::now() + timeout;
        let completed = match self.poll_until(deadline, &mut stdout, &mut stderr) {
            Ok(completed) => completed,
            Err(error) => {
                self.terminate_for(CHILD_TERMINATION_TIMEOUT, &mut stdout, &mut stderr);
                return Err(error);
            }
        };
        if !completed {
            self.terminate_for(CHILD_TERMINATION_TIMEOUT, &mut stdout, &mut stderr);
        }
        Ok(DeadlineOutput {
            status: self.exit_status.take(),
            stdout,
            stderr,
            timed_out: !completed,
        })
    }

    fn terminate(&mut self) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        self.terminate_for(CHILD_TERMINATION_TIMEOUT, &mut stdout, &mut stderr);
    }

    fn exit_diagnostic(&mut self) -> Option<String> {
        match self.poll_child() {
            Ok(()) => self.exit_status.map(|status| format!("status={status}")),
            Err(error) => Some(format!("wait_error={error}")),
        }
    }

    fn write_stdin(&mut self, bytes: &[u8]) {
        let stdin = self
            .child
            .as_mut()
            .and_then(|child| child.stdin.as_mut())
            .expect("namespaced child stdin");
        stdin
            .write_all(bytes)
            .expect("write namespaced child stdin");
        stdin.flush().expect("flush namespaced child stdin");
    }

    fn poll_until(
        &mut self,
        deadline: Instant,
        stdout: &mut Vec<u8>,
        stderr: &mut Vec<u8>,
    ) -> io::Result<bool> {
        loop {
            if self.poll_resources(stdout, stderr)? {
                return Ok(true);
            }
            let now = Instant::now();
            if now >= deadline {
                return Ok(false);
            }
            std::thread::sleep(CHILD_POLL_INTERVAL.min(deadline.saturating_duration_since(now)));
        }
    }

    fn poll_resources(&mut self, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) -> io::Result<bool> {
        let stdout_eof = self
            .stdout
            .as_mut()
            .map(|pipe| drain_pipe(pipe, stdout))
            .transpose()?
            .unwrap_or(true);
        if stdout_eof {
            self.stdout.take();
        }
        let stderr_eof = self
            .stderr
            .as_mut()
            .map(|pipe| drain_pipe(pipe, stderr))
            .transpose()?
            .unwrap_or(true);
        if stderr_eof {
            self.stderr.take();
        }
        self.poll_child()?;
        Ok(self.child.is_none() && self.stdout.is_none() && self.stderr.is_none())
    }

    fn poll_child(&mut self) -> io::Result<()> {
        let Some(child) = self.child.as_mut() else {
            return Ok(());
        };
        if let Some(status) = child.try_wait()? {
            self.exit_status = Some(status);
            self.child.take();
        }
        Ok(())
    }

    fn terminate_for(&mut self, timeout: Duration, stdout: &mut Vec<u8>, stderr: &mut Vec<u8>) {
        let started = Instant::now();
        let deadline = started + timeout;
        let grace_deadline = (started + CHILD_TERM_GRACE).min(deadline);
        let _ = self.signal_process_group(libc::SIGTERM);
        let _ = self.poll_until(grace_deadline, stdout, stderr);
        let _ = self.signal_process_group(libc::SIGKILL);
        let _ = self.poll_until(deadline, stdout, stderr);
        let _ = self.poll_resources(stdout, stderr);

        // Dropping Child never waits. If SIGKILL could not make the process
        // reapable before the deadline, cleanup remains bounded by design.
        self.child.take();
        self.stdout.take();
        self.stderr.take();
    }

    fn signal_process_group(&self, signal: libc::c_int) -> io::Result<()> {
        // SAFETY: process_group is the positive PID returned for a child that
        // was spawned with process_group(0); negating it targets only that group.
        let result = unsafe { libc::kill(-self.process_group, signal) };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) {
            Ok(())
        } else {
            Err(error)
        }
    }
}

impl Drop for DeadlineChild {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn set_nonblocking(stream: &impl AsRawFd) -> io::Result<()> {
    let fd = stream.as_raw_fd();
    // SAFETY: fd belongs to a live child pipe for the duration of this call;
    // F_GETFL only reads its file status flags.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: fd remains live and F_SETFL preserves all existing flags while
    // adding O_NONBLOCK, which is valid for pipe file descriptions.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain_pipe(pipe: &mut impl Read, output: &mut Vec<u8>) -> io::Result<bool> {
    let mut drained = 0usize;
    let mut chunk = [0_u8; 8192];
    while drained < PIPE_DRAIN_BYTES_PER_POLL {
        match pipe.read(&mut chunk) {
            Ok(0) => return Ok(true),
            Ok(read) => {
                drained = drained.saturating_add(read);
                let retained = read.min(MAX_CAPTURE_BYTES.saturating_sub(output.len()));
                output.extend_from_slice(&chunk[..retained]);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn request(class: DbHolderClass, cache_mib: u64) -> DbAdmissionRequest {
    DbAdmissionRequest::new(class, 1, cache_mib * MIB)
}

#[test]
fn exact_profile_budget_is_admitted_and_excess_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config = DbAdmissionConfig::new(2, 64 * MIB);

    let first = ProfileDbAdmission::acquire_with_config(
        &db_path,
        request(DbHolderClass::Daemon, 32),
        config,
    )
    .expect("first holder");
    let second =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Mcp, 32), config)
            .expect("exact holder/cache budget");

    let error =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Cli, 1), config)
            .expect_err("holder over budget must fail");
    assert!(matches!(
        error,
        DbAdmissionError::BudgetExceeded {
            active_holders: 2,
            requested_cache_bytes: MIB,
            ..
        }
    ));

    let snapshot =
        ProfileDbAdmission::snapshot_with_config(&db_path, config).expect("admission snapshot");
    assert_eq!(snapshot.active_holders, 2);
    assert_eq!(snapshot.configured_cache_bytes, 64 * MIB);
    assert_eq!(snapshot.holders[0].generation, first.generation());
    assert_eq!(snapshot.holders[1].generation, second.generation());
}

#[test]
fn dropping_holder_returns_profile_capacity() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let config = DbAdmissionConfig::new(1, 16 * MIB);

    let first =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Api, 16), config)
            .expect("first holder");
    let first_generation = first.generation();
    drop(first);

    let replacement =
        ProfileDbAdmission::acquire_with_config(&db_path, request(DbHolderClass::Hook, 16), config)
            .expect("capacity after release");
    assert!(replacement.generation() > first_generation);
}

#[test]
fn concurrent_registration_never_oversubscribes_profile_budget() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = Arc::new(tmp.path().join("palace.db"));
    let start = Arc::new(Barrier::new(5));
    let release = Arc::new((Mutex::new(false), Condvar::new()));
    let (admitted_tx, admitted_rx) = mpsc::channel();
    let config = DbAdmissionConfig::new(2, 32 * MIB);
    let mut workers = Vec::new();

    for _ in 0..4 {
        let db_path = Arc::clone(&db_path);
        let start = Arc::clone(&start);
        let release = Arc::clone(&release);
        let admitted_tx = admitted_tx.clone();
        workers.push(std::thread::spawn(move || {
            start.wait();
            let admission = ProfileDbAdmission::acquire_with_config(
                &db_path,
                request(DbHolderClass::Cli, 16),
                config,
            );
            admitted_tx
                .send(admission.is_ok())
                .expect("report admission");
            if admission.is_ok() {
                let (released, signal) = &*release;
                let mut released = released.lock().expect("release lock");
                while !*released {
                    released = signal.wait(released).expect("release signal");
                }
            }
            admission.is_ok()
        }));
    }
    drop(admitted_tx);

    start.wait();
    let reported = (0..4)
        .map(|_| {
            admitted_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("worker admission result")
        })
        .filter(|admitted| *admitted)
        .count();
    let (released, signal) = &*release;
    *released.lock().expect("release lock") = true;
    signal.notify_all();
    let admitted = workers
        .into_iter()
        .map(|worker| worker.join().expect("worker"))
        .filter(|admitted| *admitted)
        .count();
    assert_eq!(reported, 2);
    assert_eq!(admitted, 2);
}

#[test]
fn async_pool_holds_admission_for_its_full_lifetime() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let db_path = tmp.path().join("palace.db");
    let pool = AsyncDb::open_for(&db_path, 2, DbHolderClass::Mcp).expect("async pool");

    let active = ProfileDbAdmission::snapshot(&db_path).expect("active snapshot");
    assert_eq!(
        active.active_holders,
        1,
        "pool={:?}",
        pool.resource_snapshot()
    );
    assert_eq!(active.holders[0].holder_class, DbHolderClass::Mcp);

    drop(pool);
    assert_eq!(
        ProfileDbAdmission::snapshot(&db_path)
            .expect("released snapshot")
            .active_holders,
        0
    );
}

#[cfg(target_os = "linux")]
#[test]
fn deadline_child_bounds_non_exiting_fixture_with_inherited_pipes() {
    let started = Instant::now();
    let mut command = Command::new("sh");
    command.args([
        "-c",
        "trap '' TERM; printf ready; while :; do sleep 60; done",
    ]);
    let output = DeadlineChild::output(&mut command, Duration::from_secs(1))
        .expect("run non-exiting inherited-pipe fixture");

    assert_eq!(output.stdout, b"ready");
    assert!(output.timed_out, "non-exiting fixture must reach deadline");
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "subprocess cleanup exceeded its deadline"
    );
}

#[test]
fn status_remains_available_when_holder_budget_is_exhausted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    drop(Database::open(&db_path).expect("initialize database"));
    let holders = (0..16)
        .map(|_| {
            ProfileDbAdmission::acquire(&db_path, DbAdmissionRequest::new(DbHolderClass::Mcp, 1, 1))
                .expect("fill holder budget")
        })
        .collect::<Vec<_>>();

    let mut command = Command::new(env!("CARGO_BIN_EXE_mempal"));
    command.arg("status").env("HOME", &home).current_dir(&home);
    let output = DeadlineChild::output(&mut command, Duration::from_secs(5))
        .expect("run status at holder cap");

    assert!(
        output.success(),
        "status must remain diagnostic at holder cap: stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("status stdout UTF-8");
    assert!(
        stdout.contains("holders: 16/16"),
        "status must report the exhausted admission budget: {stdout}"
    );
    assert!(
        stdout.contains("reaped_stale_holders: 0") && stdout.contains("unknown_holders: 0"),
        "status must expose stale and unknown holder diagnostics: {stdout}"
    );

    drop(holders);
}

#[cfg(target_os = "linux")]
#[test]
fn pid_namespace_mcp_holder_is_reaped_after_forced_exit_when_supported() {
    let mut support_command = Command::new("unshare");
    support_command
        .args([
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--kill-child=SIGKILL",
            "--mount-proc",
            "true",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let support = DeadlineChild::status(&mut support_command, Duration::from_secs(5));
    if !support.is_ok_and(|output| output.success()) {
        eprintln!("skipping PID namespace integration probe: unshare is unavailable");
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    let mempal_home = home.join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    drop(Database::open(&db_path).expect("initialize database"));

    let mut command = Command::new("unshare");
    command
        .args([
            "--user",
            "--map-root-user",
            "--pid",
            "--fork",
            "--kill-child=SIGKILL",
            "--mount-proc",
        ])
        .arg(env!("CARGO_BIN_EXE_mempal"))
        .args(["serve", "--mcp"])
        .env("HOME", &home)
        .current_dir(&home)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = DeadlineChild::spawn(&mut command).expect("spawn namespaced MCP fixture");
    child.write_stdin(
        br#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"pid-namespace-test","version":"0.0.0"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"mempal_status","arguments":{}}}
"#,
    );
    let live_deadline = Instant::now() + Duration::from_secs(10);
    let live_snapshot = loop {
        if let Some(diagnostic) = child.exit_diagnostic() {
            panic!("namespaced MCP fixture exited before registration: {diagnostic}");
        }
        if let Ok(snapshot) = ProfileDbAdmission::snapshot(&db_path)
            && snapshot.active_holders > 0
        {
            break snapshot;
        }
        assert!(
            Instant::now() < live_deadline,
            "namespaced MCP fixture did not register before deadline"
        );
        std::thread::sleep(Duration::from_millis(25));
    };
    assert_eq!(live_snapshot.reaped_stale_holders, 0);
    assert_eq!(live_snapshot.unknown_holders, 0);

    child.terminate();

    let reap_deadline = Instant::now() + Duration::from_secs(10);
    let mut reaped_total = 0usize;
    loop {
        if let Ok(snapshot) = ProfileDbAdmission::snapshot(&db_path) {
            reaped_total = reaped_total.saturating_add(snapshot.reaped_stale_holders);
            if snapshot.active_holders == 0 {
                break;
            }
        }
        assert!(
            Instant::now() < reap_deadline,
            "namespaced MCP holder was not reaped before deadline"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        reaped_total > 0,
        "forced child exit must reap its holder lease"
    );
}
