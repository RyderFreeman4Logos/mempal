use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use mempal::core::db::{CURRENT_SCHEMA_VERSION, Database};
use mempal::core::queue::{PendingMessageStore, QueueFailureDisposition};
use serde_json::Value;
use tempfile::TempDir;

fn mempal_bin() -> String {
    env!("CARGO_BIN_EXE_mempal").to_string()
}

static LOAD_DOTENV: OnceLock<()> = OnceLock::new();
const CLI_TIMEOUT: Duration = Duration::from_secs(10);

/// Inject hermetic embed environment into a child Command.
fn inject_embed_env(cmd: &mut Command) {
    LOAD_DOTENV.get_or_init(|| {
        dotenvy::dotenv().ok();
    });
    for key in [
        "MEMPAL_EMBED_BACKEND",
        "MEMPAL_EMBED_BASE_URL",
        "MEMPAL_EMBED_MODEL",
        "MEMPAL_EMBED_DIM",
    ] {
        if let Ok(val) = std::env::var(key) {
            cmd.env(key, val);
        }
    }
    if std::env::var("MEMPAL_EMBED_BACKEND").is_err() {
        cmd.env("MEMPAL_EMBED_BACKEND", "stub");
    }
}

fn run_mempal(home: &TempDir, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(mempal_bin());
    cmd.args(args).env("HOME", home.path());
    inject_embed_env(&mut cmd);
    command_output_with_timeout(&mut cmd, CLI_TIMEOUT, "mempal")
}

fn run_mempal_with_path(home: &TempDir, args: &[&str], path_value: &str) -> std::process::Output {
    let mut cmd = Command::new(mempal_bin());
    cmd.args(args)
        .env("HOME", home.path())
        .env("PATH", path_value);
    inject_embed_env(&mut cmd);
    command_output_with_timeout(&mut cmd, CLI_TIMEOUT, "mempal")
}

fn command_output_with_timeout(command: &mut Command, timeout: Duration, label: &str) -> Output {
    let child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {label}: {error}"));
    wait_child_output_timeout(child, timeout, label)
}

fn wait_child_output_timeout(mut child: Child, timeout: Duration, label: &str) -> Output {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_status)) => {
                return child
                    .wait_with_output()
                    .unwrap_or_else(|error| panic!("collect {label} output: {error}"));
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                let _ = child.kill();
                let output = child
                    .wait_with_output()
                    .unwrap_or_else(|error| panic!("collect timed-out {label} output: {error}"));
                panic!(
                    "{label} did not exit within {timeout:?}; stdout={}, stderr={}",
                    String::from_utf8_lossy(&output.stdout),
                    String::from_utf8_lossy(&output.stderr)
                );
            }
            Err(error) => panic!("poll {label}: {error}"),
        }
    }
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn assert_success(output: &std::process::Output) {
    assert!(
        output.status.success(),
        "command failed\nstdout:\n{}\nstderr:\n{}",
        stdout(output),
        stderr(output)
    );
}

fn palace_db_path(home: &TempDir) -> PathBuf {
    home.path().join(".mempal/palace.db")
}

fn install_fake_path_mempal(home: &TempDir) -> String {
    let bin_dir = home.path().join("fake-bin");
    fs::create_dir_all(&bin_dir).expect("create fake bin");
    let fake = bin_dir.join("mempal");
    fs::write(&fake, "#!/bin/sh\nprintf 'mempal 0.0.0\\n'\n").expect("write fake mempal");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&fake).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&fake, perms).unwrap();
    }
    let old_path = std::env::var("PATH").unwrap_or_default();
    format!("{}:{old_path}", bin_dir.display())
}

#[test]

fn test_cli_doctor_json_reports_schema_and_path() {
    let home = TempDir::new().expect("home");
    fs::create_dir_all(home.path().join(".mempal")).expect("create mempal home");
    let db = Database::open(&palace_db_path(&home)).expect("open db");
    assert_eq!(db.schema_version().expect("schema"), CURRENT_SCHEMA_VERSION);
    drop(db);
    let path_value = install_fake_path_mempal(&home);

    let output = run_mempal_with_path(&home, &["doctor", "--format", "json"], &path_value);
    assert_success(&output);
    let value: Value = serde_json::from_str(&stdout(&output)).expect("doctor json");
    assert_eq!(
        value["current_version"].as_str(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    assert_eq!(value["supported_schema_version"], CURRENT_SCHEMA_VERSION);
    assert_eq!(value["db"]["exists"], true);
    assert_eq!(value["db"]["schema_version"], CURRENT_SCHEMA_VERSION);
    let expected_db_path = palace_db_path(&home).display().to_string();
    assert_eq!(
        value["db_holders"]["db_path"].as_str(),
        Some(expected_db_path.as_str())
    );
    assert!(value["db_holders"]["holder_count"].as_u64().is_some());
    assert_eq!(value["install"]["path_matches_current_exe"], false);
    assert!(
        value["warnings"]
            .as_array()
            .expect("warnings")
            .iter()
            .any(|warning| warning.as_str().unwrap_or_default().contains("PATH"))
    );
}

#[test]
fn test_cli_doctor_and_status_report_daemon_outage_queue_high_severity() {
    let home = TempDir::new().expect("home");
    fs::create_dir_all(home.path().join(".mempal")).expect("create mempal home");
    let db_path = palace_db_path(&home);
    Database::open(&db_path).expect("open db");
    let store = PendingMessageStore::new_without_reclaim(&db_path);
    for n in 0..100 {
        store
            .enqueue("hook_event", &format!(r#"{{"n":{n}}}"#))
            .expect("enqueue pending row");
    }

    let doctor = run_mempal(&home, &["doctor", "--format", "json"]);
    assert_success(&doctor);
    let report: Value = serde_json::from_str(&stdout(&doctor)).expect("doctor json");
    assert_eq!(report["availability"]["severity"], "high");
    assert_eq!(
        report["availability"]["signal"],
        "daemon_down_large_pending_queue"
    );
    assert_eq!(report["availability"]["pending_queue_threshold"], 100);
    assert!(
        report["recommendations"]
            .as_array()
            .expect("recommendations")
            .iter()
            .any(|recommendation| recommendation
                .as_str()
                .unwrap_or_default()
                .contains("start the daemon")),
        "{report}"
    );

    let status = run_mempal(&home, &["status"]);
    assert_success(&status);
    let output = stdout(&status);
    assert!(output.contains("[ERROR]"), "{output}");
    assert!(output.contains("daemon_outage_queue"), "{output}");
    assert!(output.contains("start the daemon"), "{output}");
}

#[test]
fn test_cli_doctor_reports_unavailable_when_config_is_invalid() {
    let home = TempDir::new().expect("home");
    let mempal_home = home.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&palace_db_path(&home)).expect("open db");
    fs::write(mempal_home.join("config.toml"), "not = [valid toml").expect("write invalid config");

    let doctor = run_mempal(&home, &["doctor", "--format", "json"]);
    assert_success(&doctor);
    let report: Value = serde_json::from_str(&stdout(&doctor)).expect("doctor json");

    assert_eq!(report["availability"]["severity"], "unavailable");
    assert_eq!(
        report["availability"]["unavailable_reasons"],
        serde_json::json!(["config"])
    );
}

#[test]
fn test_cli_doctor_reports_unavailable_when_queue_stats_fail() {
    let home = TempDir::new().expect("home");
    fs::create_dir_all(home.path().join(".mempal")).expect("create mempal home");
    fs::write(palace_db_path(&home), "not a sqlite database").expect("write invalid db");

    let doctor = run_mempal(&home, &["doctor", "--format", "json"]);
    assert_success(&doctor);
    let report: Value = serde_json::from_str(&stdout(&doctor)).expect("doctor json");

    assert_eq!(report["availability"]["severity"], "unavailable");
    assert_eq!(
        report["availability"]["unavailable_reasons"],
        serde_json::json!(["queue_stats"])
    );
}

#[cfg(target_os = "linux")]
#[test]
fn test_cli_doctor_rejects_unrelated_live_pid_as_daemon_identity() {
    let home = TempDir::new().expect("home");
    fs::create_dir_all(home.path().join(".mempal")).expect("create mempal home");
    Database::open(&palace_db_path(&home)).expect("open db");
    let mut unrelated = Command::new("sleep")
        .arg("120")
        .spawn()
        .expect("spawn unrelated process");
    fs::write(
        home.path().join(".mempal/daemon.pid"),
        unrelated.id().to_string(),
    )
    .expect("write stale pidfile");

    let doctor = run_mempal(&home, &["doctor", "--format", "json"]);
    let status = run_mempal(&home, &["status"]);
    unrelated.kill().expect("stop unrelated process");
    unrelated.wait().expect("reap unrelated process");
    assert_success(&doctor);
    assert_success(&status);
    let report: Value = serde_json::from_str(&stdout(&doctor)).expect("doctor json");
    let status = stdout(&status);

    assert_eq!(report["daemon"]["running"], false);
    assert_eq!(report["availability"]["severity"], "unavailable");
    assert_eq!(
        report["availability"]["unavailable_reasons"],
        serde_json::json!(["daemon_identity"])
    );
    assert!(status.contains("running: false"), "{status}");
    assert!(status.contains("availability is unavailable"), "{status}");
}

#[test]

fn test_cli_doctor_reports_queue_failure_classes() {
    let home = TempDir::new().expect("home");
    fs::create_dir_all(home.path().join(".mempal")).expect("create mempal home");
    fs::write(
        home.path().join(".mempal/config.toml"),
        "[api]\nenabled = false\n",
    )
    .expect("write isolated doctor config");
    let db_path = palace_db_path(&home);
    Database::open(&db_path).expect("open db");
    let store = PendingMessageStore::new_without_reclaim(&db_path);

    let retryable = store
        .enqueue("hook_event", r#"{"n":1}"#)
        .expect("enqueue retryable row");
    let terminal = store
        .enqueue("hook_event", r#"{"n":2}"#)
        .expect("enqueue terminal row");
    store
        .mark_model_task_failed_retryable(&retryable, "429 Too Many Requests")
        .expect("mark retryable failed");
    let terminal_claim = store
        .claim_next("terminal-worker", 60)
        .expect("claim terminal row")
        .expect("terminal row claimed");
    assert_eq!(terminal_claim.id, terminal);
    store
        .mark_failed_with_disposition(
            &terminal_claim,
            "invalid payload",
            QueueFailureDisposition::Terminal,
        )
        .expect("mark terminal failed");

    let json = run_mempal(&home, &["doctor", "--format", "json"]);
    assert_success(&json);
    let value: Value = serde_json::from_str(&stdout(&json)).expect("doctor json");
    assert_eq!(value["embedding"]["queue"]["failed"], 2);
    assert_eq!(value["embedding"]["queue"]["failed_retryable"], 1);
    assert_eq!(value["embedding"]["queue"]["failed_terminal"], 1);
    assert_eq!(value["embedding"]["queue"]["failed_retryable_embed"], 1);
    assert_eq!(value["embedding"]["queue"]["failed_retryable_llm"], 0);
    assert_eq!(
        value["embedding"]["runtime_status_source"].as_str(),
        Some("unavailable")
    );
    assert_eq!(
        value["embedding"]["runtime_status_available"].as_bool(),
        Some(false)
    );
    assert_eq!(value["embedding"]["degraded"], false);
    assert_eq!(value["embedding"]["block_writes_when_degraded"], true);
    assert_eq!(value["embedding"]["write_refused"], false);
    assert_eq!(value["embedding"]["fail_count"], 2);
    assert_eq!(
        value["embedding"]["queue"]["last_auto_requeue_at_unix_ms"],
        Value::Null
    );

    let plain = run_mempal(&home, &["doctor", "--format", "plain"]);
    assert_success(&plain);
    let out = stdout(&plain);
    assert!(
        out.contains("embedding_queue=pending:0 claimed:0 failed:2 retryable_model:1 terminal:1 retryable_embedding:1 retryable_llm:0 last_auto_requeue_at_unix_ms:none"),
        "{out}"
    );
    assert!(
        out.contains("embedding_runtime_status_source=unavailable"),
        "{out}"
    );
    assert!(
        out.contains("embedding_runtime_status_available=false"),
        "{out}"
    );
    assert!(out.contains("embedding_degraded=false"), "{out}");
    assert!(
        out.contains("embedding_block_writes_when_degraded=true"),
        "{out}"
    );
    assert!(out.contains("embedding_write_refused=false"), "{out}");
    assert!(out.contains("embedding_fail_count=2"), "{out}");
}

#[test]
fn test_cli_daemon_status_reports_queue_failure_classes() {
    let home = TempDir::new().expect("home");
    fs::create_dir_all(home.path().join(".mempal")).expect("create mempal home");
    let db_path = palace_db_path(&home);
    Database::open(&db_path).expect("open db");
    let store = PendingMessageStore::new_without_reclaim(&db_path);

    let retryable = store
        .enqueue("hook_event", r#"{"n":1}"#)
        .expect("enqueue retryable row");
    let terminal = store
        .enqueue("hook_event", r#"{"n":2}"#)
        .expect("enqueue terminal row");
    store
        .mark_model_task_failed_retryable(&retryable, "429 Too Many Requests")
        .expect("mark retryable failed");
    let terminal_claim = store
        .claim_next("terminal-worker", 60)
        .expect("claim terminal row")
        .expect("terminal row claimed");
    assert_eq!(terminal_claim.id, terminal);
    store
        .mark_failed_with_disposition(
            &terminal_claim,
            "invalid payload",
            QueueFailureDisposition::Terminal,
        )
        .expect("mark terminal failed");
    fs::write(
        home.path().join(".mempal/daemon.pid"),
        std::process::id().to_string(),
    )
    .expect("write daemon pid");

    let output = run_mempal(&home, &["daemon", "status"]);
    assert_success(&output);
    let out = stdout(&output);
    assert!(out.contains("queue.failed: 2"), "{out}");
    assert!(out.contains("queue.failed_retryable: 1"), "{out}");
    assert!(out.contains("queue.failed_terminal: 1"), "{out}");
    assert!(
        out.contains("queue.failed_retryable_model: embedding=1 llm=0"),
        "{out}"
    );
}

#[test]

fn test_cli_doctor_plain_no_db_is_read_only() {
    let home = TempDir::new().expect("home");
    let output = run_mempal(&home, &["doctor", "--format", "plain"]);
    assert_success(&output);
    let out = stdout(&output);
    assert!(out.contains("db_exists=false"), "{out}");
    assert!(!palace_db_path(&home).exists());
}

#[test]

fn test_cli_doctor_rejects_invalid_format() {
    let home = TempDir::new().expect("home");
    let output = run_mempal(&home, &["doctor", "--format", "yaml"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unsupported doctor format"));
    assert!(!palace_db_path(&home).exists());
}

#[test]

fn test_cli_maintenance_guided_run_json() {
    let home = TempDir::new().expect("home");
    fs::create_dir_all(home.path().join(".mempal")).expect("create mempal home");
    Database::open(&palace_db_path(&home)).expect("open db");

    let output = run_mempal(&home, &["maintenance", "guided-run", "--format", "json"]);
    assert_success(&output);
    let value: Value = serde_json::from_str(&stdout(&output)).expect("guided run json");
    assert_eq!(value["writes"], false);
    let commands = value["steps"]
        .as_array()
        .expect("steps")
        .iter()
        .filter_map(|step| step["command"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(commands.contains("research-validate-plan"), "{commands}");
    assert!(commands.contains("adoption review"), "{commands}");
    assert!(commands.contains("cowork-doctor"), "{commands}");
}

#[test]

fn test_cli_maintenance_guided_run_plain() {
    let home = TempDir::new().expect("home");
    fs::create_dir_all(home.path().join(".mempal")).expect("create mempal home");
    Database::open(&palace_db_path(&home)).expect("open db");

    let output = run_mempal(&home, &["maintenance", "guided-run", "--format", "plain"]);
    assert_success(&output);
    let out = stdout(&output);
    assert!(out.contains("Guided Maintenance Run"), "{out}");
    assert!(out.contains("mempal phase3 adoption review"), "{out}");
    assert!(out.contains("mempal cowork-capture"), "{out}");
}

#[test]

fn test_cli_maintenance_guided_run_rejects_invalid_format() {
    let home = TempDir::new().expect("home");
    fs::create_dir_all(home.path().join(".mempal")).expect("create mempal home");
    Database::open(&palace_db_path(&home)).expect("open db");

    let output = run_mempal(&home, &["maintenance", "guided-run", "--format", "yaml"]);
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unsupported maintenance guided-run format"));
}

#[test]

fn test_cli_release_readiness_json() {
    let home = TempDir::new().expect("home");
    let mut cmd = Command::new(mempal_bin());
    cmd.args(["release-readiness", "--format", "json"])
        .env("HOME", home.path())
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")));
    inject_embed_env(&mut cmd);
    let output = command_output_with_timeout(&mut cmd, CLI_TIMEOUT, "mempal release-readiness");
    assert_success(&output);
    let value: Value = serde_json::from_str(&stdout(&output)).expect("release readiness json");
    assert_eq!(value["writes"], false);
    let check_names = value["checks"]
        .as_array()
        .expect("checks")
        .iter()
        .filter_map(|check| check["name"].as_str())
        .collect::<Vec<_>>();
    assert!(check_names.contains(&"cargo-metadata"), "{check_names:?}");
    assert!(
        check_names.contains(&"spec-plan-inventory"),
        "{check_names:?}"
    );
    assert!(
        value["recommended_commands"]
            .as_array()
            .expect("commands")
            .iter()
            .any(|command| command
                .as_str()
                .unwrap_or_default()
                .contains("cargo package"))
    );
}

#[test]

fn test_cli_release_readiness_plain() {
    let home = TempDir::new().expect("home");
    let mut cmd = Command::new(mempal_bin());
    cmd.args(["release-readiness", "--format", "plain"])
        .env("HOME", home.path())
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")));
    inject_embed_env(&mut cmd);
    let output = command_output_with_timeout(&mut cmd, CLI_TIMEOUT, "mempal release-readiness");
    assert_success(&output);
    let out = stdout(&output);
    assert!(out.contains("Release Readiness"), "{out}");
    assert!(out.contains("cargo package"), "{out}");
    assert!(out.contains("mempal doctor"), "{out}");
}

#[test]

fn test_cli_release_readiness_rejects_invalid_format() {
    let home = TempDir::new().expect("home");
    let mut cmd = Command::new(mempal_bin());
    cmd.args(["release-readiness", "--format", "yaml"])
        .env("HOME", home.path())
        .current_dir(Path::new(env!("CARGO_MANIFEST_DIR")));
    inject_embed_env(&mut cmd);
    let output = command_output_with_timeout(&mut cmd, CLI_TIMEOUT, "mempal release-readiness");
    assert!(!output.status.success());
    assert!(stderr(&output).contains("unsupported release-readiness format"));
}
