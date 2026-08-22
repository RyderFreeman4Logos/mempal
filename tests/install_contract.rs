use std::{
    env, fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::Command,
    thread,
    time::{Duration, Instant},
};

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

fn run_git(repo: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap_or_else(|error| panic!("run git {args:?}: {error}"));
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_owned()
}

fn run_post_merge_fixture(systemd_active: bool) -> Vec<String> {
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
        "#!/usr/bin/env bash\nprintf 'systemctl %s\\n' \"$*\" >> \"$TRACE_FILE\"\nif [ \"$*\" = \"--user is-active --quiet mempal-daemon.service\" ]; then\n  [ \"${SYSTEMD_ACTIVE:-0}\" = 1 ]\nfi\n",
    );
    write_executable(
        &install_root.join("bin/mempal"),
        "#!/usr/bin/env bash\nprintf 'cli %s\\n' \"$*\" >> \"$TRACE_FILE\"\n",
    );

    let original_path = env::var("PATH").expect("test PATH");
    let path = format!("{}:{original_path}", fake_bin.display());
    let hook = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/hooks/post-merge.sh");
    let output = Command::new("bash")
        .arg(hook)
        .current_dir(&repo)
        .env("CARGO_INSTALL_ROOT", &install_root)
        .env("PATH", path)
        .env("SYSTEMD_ACTIVE", if systemd_active { "1" } else { "0" })
        .env("TRACE_FILE", &trace)
        .output()
        .expect("run post-merge hook");
    assert!(
        output.status.success(),
        "post-merge hook failed: stdout={}, stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = fs::read_to_string(&trace) {
            let events: Vec<_> = contents.lines().map(str::to_owned).collect();
            if events.len() >= 2 {
                return events;
            }
        }
        assert!(
            Instant::now() < deadline,
            "post-merge fixture did not finish"
        );
        thread::sleep(Duration::from_millis(10));
    }
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
    let events = run_post_merge_fixture(true);
    assert_eq!(
        events,
        [
            "just install",
            "systemctl --user is-active --quiet mempal-daemon.service",
            "systemctl --user try-restart mempal-daemon.service",
        ]
    );
    assert!(
        !events.iter().any(|event| event.contains("mempal.service")),
        "mempal.service must never be invoked: {events:?}"
    );
}

#[test]
fn inactive_systemd_install_contract_uses_unmanaged_cli_restart() {
    let events = run_post_merge_fixture(false);
    assert_eq!(
        events,
        [
            "just install",
            "systemctl --user is-active --quiet mempal-daemon.service",
            "cli daemon restart",
        ]
    );
    assert!(
        !events.iter().any(|event| event.contains("mempal.service")),
        "mempal.service must never be invoked: {events:?}"
    );
}
