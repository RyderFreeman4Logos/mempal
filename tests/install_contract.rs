use std::{
    env, fs, io,
    os::unix::{fs::PermissionsExt, process::CommandExt},
    path::Path,
    process::{Command, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

const FIXTURE_SUBPROCESS_DEADLINE: Duration = Duration::from_secs(30);

fn repo_file(path: &str) -> String {
    fs::read_to_string(format!("{}/{}", env!("CARGO_MANIFEST_DIR"), path))
        .unwrap_or_else(|error| panic!("read {path}: {error}"))
}

fn write_executable(path: &Path, contents: &str) {
    fs::write(path, contents).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    let mut permissions = fs::metadata(path)
        .unwrap_or_else(|error| panic!("stat {}: {error}", path.display()))
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions)
        .unwrap_or_else(|error| panic!("chmod {}: {error}", path.display()));
}

fn run_bounded_output(mut command: Command, timeout: Duration) -> Output {
    // SAFETY: the child immediately becomes its own process-group leader before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().expect("spawn bounded fixture subprocess");
    let process_group = -(child.id() as libc::pid_t);
    let deadline = Instant::now() + timeout;
    loop {
        if child
            .try_wait()
            .expect("poll bounded fixture subprocess")
            .is_some()
        {
            // SAFETY: pre_exec assigned this child a private process group.
            unsafe { libc::kill(process_group, libc::SIGKILL) };
            return child
                .wait_with_output()
                .expect("collect bounded fixture subprocess");
        }
        if Instant::now() >= deadline {
            // SAFETY: pre_exec assigned this child a private process group.
            unsafe { libc::kill(process_group, libc::SIGKILL) };
            let _ = child.wait();
            panic!("bounded fixture subprocess exceeded {timeout:?}");
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn run_git(repo: &Path, args: &[&str]) -> String {
    let mut command = Command::new("git");
    command
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_bounded_output(command, FIXTURE_SUBPROCESS_DEADLINE);
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn run_post_merge_fixture(systemd_state: &str, expected_events: &[&str]) -> Vec<String> {
    let temp = tempfile::tempdir().expect("post-merge fixture tempdir");
    let repo = temp.path().join("repo");
    let fake_bin = temp.path().join("fake-bin");
    let install_root = temp.path().join("install-root");
    let trace = temp.path().join("events");
    fs::create_dir_all(repo.join("src")).expect("create fixture source");
    fs::create_dir_all(&fake_bin).expect("create fake bin");
    fs::create_dir_all(install_root.join("bin")).expect("create fixture install bin");

    run_git(&repo, &["init", "--quiet"]);
    run_git(&repo, &["config", "user.email", "test@example.invalid"]);
    run_git(&repo, &["config", "user.name", "install-contract-test"]);
    fs::write(repo.join("src/initial.rs"), "fn initial() {}\n").expect("write initial source");
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "--quiet", "-m", "initial"]);
    let old_head = run_git(&repo, &["rev-parse", "HEAD"]);
    fs::write(repo.join("src/changed.rs"), "fn changed() {}\n").expect("write changed source");
    run_git(&repo, &["add", "."]);
    run_git(&repo, &["commit", "--quiet", "-m", "changed"]);
    run_git(&repo, &["update-ref", "ORIG_HEAD", &old_head]);

    write_executable(
        &fake_bin.join("just"),
        "#!/usr/bin/env bash\nprintf 'just %s\\n' \"$*\" >> \"$TRACE_FILE\"\n",
    );
    write_executable(
        &fake_bin.join("systemctl"),
        "#!/usr/bin/env bash\nprintf 'systemctl %s\\n' \"$*\" >> \"$TRACE_FILE\"\nif [ \"$*\" = \"--user show -p ActiveState --value mempal-daemon.service\" ]; then\n  case \"${SYSTEMD_STATE:-error}\" in\n    active|inactive) printf '%s\\n' \"$SYSTEMD_STATE\" ;;\n    *) exit 42 ;;\n  esac\nfi\n",
    );
    write_executable(
        &install_root.join("bin/mempal"),
        "#!/usr/bin/env bash\nprintf 'cli %s\\n' \"$*\" >> \"$TRACE_FILE\"\n",
    );

    let original_path = env::var("PATH").expect("test PATH");
    let path = format!("{}:{original_path}", fake_bin.display());
    let hook = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hooks/post-merge.sh");
    let waiter = r#"
set -euo pipefail
bash "$1"
deadline=$((SECONDS + 10))
while (( SECONDS < deadline )); do
  if [ -f "$TRACE_FILE" ] && [ "$(wc -l < "$TRACE_FILE")" -eq "$EXPECTED_EVENTS" ]; then
    exit 0
  fi
  sleep 0.01
done
printf 'post-merge fixture did not reach the expected event count\n' >&2
exit 1
"#;
    let mut command = Command::new("bash");
    command
        .args(["-c", waiter, "post-merge-fixture"])
        .arg(&hook)
        .current_dir(&repo)
        .env("CARGO_INSTALL_ROOT", &install_root)
        .env("PATH", path)
        .env("SYSTEMD_STATE", systemd_state)
        .env("TRACE_FILE", &trace)
        .env("EXPECTED_EVENTS", expected_events.len().to_string())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let output = run_bounded_output(command, FIXTURE_SUBPROCESS_DEADLINE);
    assert!(
        output.status.success(),
        "post-merge hook failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let contents = fs::read_to_string(&trace).expect("post-merge fixture trace");
    let events: Vec<_> = contents.lines().map(str::to_owned).collect();
    assert_eq!(
        events,
        expected_events
            .iter()
            .map(|event| (*event).to_owned())
            .collect::<Vec<_>>()
    );
    events
}

#[test]
fn live_install_contract_keeps_rest_and_recycles_daemon() {
    let justfile = repo_file("justfile");
    let source_installer = repo_file("scripts/install-from-source.sh");

    for docs_path in ["README.md", "docs/usage.md"] {
        let docs = repo_file(docs_path);
        assert!(
            docs.contains("cargo install --path . --locked --features rest"),
            "{docs_path} must document a REST-enabled source install"
        );
        assert!(
            !docs.contains("cargo install --path . --locked\n"),
            "{docs_path} must not document a default-feature source install"
        );
    }

    assert!(
        justfile.contains("install --path . --locked --features rest --force --root"),
        "the live install recipe must build REST support"
    );
    assert!(
        source_installer.contains("install --path . --force --locked --features rest --root"),
        "the source installer must build REST support"
    );
}

#[test]
fn active_systemd_install_contract_recycles_without_cli_restart() {
    let events = run_post_merge_fixture(
        "active",
        &[
            "just install",
            "systemctl --user show -p ActiveState --value mempal-daemon.service",
            "systemctl --user try-restart mempal-daemon.service",
        ],
    );
    assert!(
        !events.iter().any(|event| event.contains("mempal.service")),
        "mempal.service must never be invoked: {events:?}"
    );
}

#[test]
fn inactive_systemd_install_contract_uses_unmanaged_cli_restart() {
    let events = run_post_merge_fixture(
        "inactive",
        &[
            "just install",
            "systemctl --user show -p ActiveState --value mempal-daemon.service",
            "cli daemon restart",
        ],
    );
    assert!(
        !events.iter().any(|event| event.contains("mempal.service")),
        "mempal.service must never be invoked: {events:?}"
    );
}

#[test]
fn probe_error_install_contract_aborts_recycle_without_cli_or_systemd_restart() {
    let events = run_post_merge_fixture(
        "error",
        &[
            "just install",
            "systemctl --user show -p ActiveState --value mempal-daemon.service",
        ],
    );
    assert!(!events.iter().any(|event| event.contains("try-restart")));
    assert!(
        !events
            .iter()
            .any(|event| event.contains("cli daemon restart"))
    );
    assert!(
        !events.iter().any(|event| event.contains("mempal.service")),
        "mempal.service must never be invoked: {events:?}"
    );
}
