#[cfg(test)]
mod fuser_diagnostic_tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    fn repo_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    fn wait_for_file(path: &Path, timeout: Duration, description: &str) {
        let deadline = Instant::now() + timeout;
        while !path.exists() && Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            thread::sleep(Duration::from_millis(25).min(remaining));
        }
        assert!(path.exists(), "{description} did not become ready");
    }

    #[derive(Debug, Eq, PartialEq)]
    enum GateMilestone {
        ProxyReady,
        ChildExited,
    }

    fn bounded_fixture_diagnostic(bytes: &[u8]) -> String {
        const MAX_DIAGNOSTIC_BYTES: usize = 4 * 1024;

        let start = bytes.len().saturating_sub(MAX_DIAGNOSTIC_BYTES);
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }

    fn wait_for_proxy_ready_or_gate_exit(
        gate: &mut GateChild,
        entry_file: &Path,
        ready_file: &Path,
        timeout: Duration,
        description: &str,
    ) -> Result<GateMilestone, String> {
        let started = Instant::now();
        let deadline = started + timeout;
        let mut entry_elapsed_ms = None;
        loop {
            if entry_elapsed_ms.is_none() && entry_file.exists() {
                entry_elapsed_ms = Some(started.elapsed().as_millis());
            }
            if ready_file.exists() {
                return Ok(GateMilestone::ProxyReady);
            }
            if child_exit_state(gate.child.child()).map_err(|error| error.to_string())?
                != ChildExitState::Running
            {
                return Ok(GateMilestone::ChildExited);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                let cleanup = match gate.wait_with_timeout(Duration::ZERO) {
                    Ok(output) => format!(
                        "gate_exit={}; stderr={}",
                        output.status,
                        bounded_fixture_diagnostic(&output.stderr)
                    ),
                    Err(error) => format!(
                        "gate_cleanup={}",
                        bounded_fixture_diagnostic(error.to_string().as_bytes())
                    ),
                };
                return Err(format!(
                    "{description} produced neither readiness nor child exit within {timeout:?}; proxy_entry_ms={}; ready=false; child_exit_ms=missing; {cleanup}",
                    entry_elapsed_ms
                        .map(|elapsed| elapsed.to_string())
                        .unwrap_or_else(|| "missing".to_owned())
                ));
            }
            thread::sleep(Duration::from_millis(25).min(remaining));
        }
    }

    #[test]
    fn gate_marker_timeout_reports_bounded_failure_diagnostics() {
        let _process_lock = process_lifecycle_test_lock_blocking();
        let fixture = tempfile::tempdir().expect("create marker-timeout fixture");
        let entry_file = fixture.path().join("entry");
        let ready_file = fixture.path().join("ready");
        let mut command = Command::new("/bin/bash");
        command
            .args([
                "-c",
                "printf '%9000s' x >&2; printf '\\nfixture stderr marker\\n' >&2; exec /bin/sleep 60",
            ])
            .env("FIXTURE_SECRET", "fixture-secret-must-not-leak")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut gate =
            GateChild::new(spawn_in_own_session(&mut command).expect("spawn diagnostic fixture"))
                .expect("capture diagnostic fixture identity");

        let diagnostic = wait_for_proxy_ready_or_gate_exit(
            &mut gate,
            &entry_file,
            &ready_file,
            Duration::from_millis(50),
            "diagnostic fixture",
        )
        .expect_err("a live child without markers must report diagnostics");

        assert!(diagnostic.contains("proxy_entry_ms=missing"), "{diagnostic}");
        assert!(diagnostic.contains("child_exit_ms=missing"), "{diagnostic}");
        assert!(diagnostic.contains("fixture stderr marker"), "{diagnostic}");
        assert!(diagnostic.len() < 5 * 1024, "diagnostic was not bounded");
        assert!(!diagnostic.contains("fixture-secret-must-not-leak"));
    }
    #[test]
    fn rest_gate_fuser_diagnostic_cannot_outlive_lock_budget() {
        let _process_lock = process_lifecycle_test_lock_blocking();
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
        )
        .expect("capture lock holder identity");
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
            .env("PATH", &path)
            .env("REST_GATE_DRY_RUN", "1")
            .env("REST_GATE_LOCK_TIMEOUT_SECS", "1")
            .env("REST_GATE_TARGET_DIR", &target)
            .env("REST_TEST_TARGETS_PER_BATCH", "999")
            .env("REST_GATE_FUSER_NEVER_RETURN_READY_FILE", &fuser_ready_file)
            .env("REST_GATE_FUSER_PID_FILE", &fuser_pid_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut gate =
            GateChild::new(spawn_in_own_session(&mut command).expect("spawn isolated REST gate"))
                .expect("capture isolated REST gate identity");
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

        let delayed_fuser_entry_file = fixture.path().join("delayed-fuser-entry");
        let delayed_fuser_ready_file = fixture.path().join("delayed-fuser-ready");
        let delayed_fuser_pid_file = fixture.path().join("delayed-fuser.pid");
        let mut command = Command::new("/bin/bash");
        command
            .arg(repo_root().join("scripts/gates/rest-tests.sh"))
            .current_dir(repo_root())
            .env("PATH", &path)
            .env("REST_GATE_DRY_RUN", "1")
            .env("REST_GATE_LOCK_TIMEOUT_SECS", "1")
            .env("REST_GATE_TARGET_DIR", &target)
            .env("REST_TEST_TARGETS_PER_BATCH", "999")
            .env("REST_GATE_FUSER_ENTRY_FILE", &delayed_fuser_entry_file)
            .env("REST_GATE_FUSER_DELAY_BEFORE_READY_SECS", "2")
            .env(
                "REST_GATE_FUSER_NEVER_RETURN_READY_FILE",
                &delayed_fuser_ready_file,
            )
            .env("REST_GATE_FUSER_PID_FILE", &delayed_fuser_pid_file)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let started = Instant::now();
        let mut gate =
            GateChild::new(spawn_in_own_session(&mut command).expect("spawn isolated REST gate"))
                .expect("capture isolated REST gate identity");
        let milestone = wait_for_proxy_ready_or_gate_exit(
            &mut gate,
            &delayed_fuser_entry_file,
            &delayed_fuser_ready_file,
            Duration::from_secs(2),
            "delayed stalled fuser",
        )
        .unwrap_or_else(|diagnostic| panic!("{diagnostic}"));

        let output = gate
            .wait_with_timeout(Duration::from_secs(3))
            .expect("reap REST gate");

        assert_eq!(milestone, GateMilestone::ChildExited);
        assert_eq!(output.status.code(), Some(75));
        assert!(
            delayed_fuser_entry_file.exists(),
            "delayed fuser proxy did not enter"
        );
        assert!(
            !delayed_fuser_ready_file.exists(),
            "the delayed proxy unexpectedly became ready before the product deadline"
        );
        assert!(
            !delayed_fuser_pid_file.exists(),
            "the delayed proxy unexpectedly published its PID before the product deadline"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the fuser diagnostic exceeded the advertised lock deadline: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
