use std::fs;
use std::path::Path;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use mempal::core::db::Database;
use mempal::core::queue::PendingMessageStore;
use serde_json::{Value, json};

const TEST_KIND: &str = "spool_followability_test";

fn write_spool_receipt(home: &Path, idempotency_key: &str) -> String {
    let spool_dir = home.join(".mempal/ingress-spool");
    fs::create_dir_all(&spool_dir).expect("create ingress spool");
    let file = fs::File::create(spool_dir.join("cli-followability.json"))
        .expect("create ingress spool receipt");
    serde_json::to_writer(
        &file,
        &json!({
            "kind": TEST_KIND,
            "payload": "{}",
            "idempotency_key": idempotency_key,
        }),
    )
    .expect("write ingress spool receipt");
    file.sync_all().expect("fsync ingress spool receipt");
    fs::File::open(&spool_dir)
        .expect("open ingress spool directory")
        .sync_all()
        .expect("fsync ingress spool directory");
    PendingMessageStore::idempotent_message_id(TEST_KIND, idempotency_key)
}

fn wait_output(mut child: Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(4);
    loop {
        if child.try_wait().expect("poll CLI").is_some() {
            return child.wait_with_output().expect("collect CLI output");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("operation wait CLI did not exit within four seconds");
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn operation_wait_follows_spool_receipt_until_queue_replay() {
    let home = tempfile::TempDir::new_in("/tmp").expect("short tempdir");
    let mempal_home = home.path().join(".mempal");
    fs::create_dir_all(&mempal_home).expect("create mempal home");
    let db_path = mempal_home.join("palace.db");
    Database::open(&db_path).expect("open database");
    let idempotency_key = "cli-spool-followability";
    let operation_id = write_spool_receipt(home.path(), idempotency_key);

    let child = Command::new(env!("CARGO_BIN_EXE_mempal"))
        .args([
            "operation",
            "wait",
            &operation_id,
            "--timeout-secs",
            "1",
            "--json",
        ])
        .env("HOME", home.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn operation wait CLI");
    std::thread::sleep(Duration::from_millis(250));
    let replayed_id = PendingMessageStore::new_without_reclaim(&db_path)
        .enqueue_idempotent_with_key(TEST_KIND, "{}", idempotency_key)
        .expect("replay spool receipt into queue");
    assert_eq!(replayed_id, operation_id);

    let output = wait_output(child);
    assert!(!output.status.success(), "one-second wait must time out");
    let stdout: Value = serde_json::from_slice(&output.stdout).expect("parse timeout receipt");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(stdout["operation_id"], operation_id);
    assert_eq!(stdout["state"], "queued");
    assert_eq!(stdout["timed_out"], true);
    assert!(
        !stderr.contains("operation not found"),
        "CLI wait must follow the fsynced receipt through replay: {stderr}"
    );
}
