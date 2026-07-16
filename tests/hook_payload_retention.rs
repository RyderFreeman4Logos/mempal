use std::path::Path;

use filetime::{FileTime, set_file_mtime};
use mempal::core::{config::Config, db::Database, queue::PendingMessageStore};
use mempal::hook::HOOK_SPOOL_DIR;
use mempal::hook_payload::prune_hook_payloads;
use serde_json::json;

fn write_old_payload(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("write spool payload");
    set_file_mtime(path, FileTime::from_unix_time(0, 0)).expect("age spool payload");
}

fn enqueue_payload_handle(store: &PendingMessageStore, path: &Path) {
    let envelope = json!({
        "event": "PostToolUse",
        "kind": "hook_post_tool",
        "agent": "claude",
        "captured_at": "2026-07-15T00:00:00Z",
        "claude_cwd": "/tmp/project",
        "payload": null,
        "payload_path": path.display().to_string(),
        "payload_preview": null,
        "original_size_bytes": 70_000,
        "truncated": false
    });
    store
        .enqueue("hook_post_tool", &envelope.to_string())
        .expect("enqueue payload handle");
}

#[test]
fn hook_payload_retention_prunes_only_old_unreferenced_files() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join(".mempal");
    let db_path = mempal_home.join("palace.db");
    std::fs::create_dir_all(&mempal_home).expect("create mempal home");
    Database::open(&db_path).expect("initialize database");
    let store = PendingMessageStore::new(&db_path).expect("open queue");
    let spool = mempal_home.join(HOOK_SPOOL_DIR);
    std::fs::create_dir_all(&spool).expect("create spool");

    let claimed = spool.join("claimed.json");
    let pending = spool.join("pending.json");
    let orphan = spool.join("orphan.json");
    let young = spool.join("young.json");
    for path in [&claimed, &pending, &orphan] {
        write_old_payload(path, "old raw payload");
    }
    std::fs::write(&young, "young raw payload").expect("write young payload");

    enqueue_payload_handle(&store, &claimed);
    enqueue_payload_handle(&store, &pending);
    store
        .claim_next("retention-test", 120)
        .expect("claim queue row")
        .expect("claimed queue row");

    let outcome = prune_hook_payloads(&mempal_home, &db_path, 7).expect("prune hook payloads");

    assert_eq!(outcome.scanned_files, 4);
    assert_eq!(outcome.deleted_files, 1);
    assert_eq!(outcome.referenced_files, 2);
    assert_eq!(outcome.young_files, 1);
    assert!(claimed.exists(), "claimed queue payload must be retained");
    assert!(pending.exists(), "pending queue payload must be retained");
    assert!(!orphan.exists(), "old orphan payload should be pruned");
    assert!(young.exists(), "young orphan payload must be retained");
}

#[test]
fn hooks_payload_retention_days_defaults_to_seven_and_is_configurable() {
    let defaults = Config::parse("[hooks]\nenabled = true\n").expect("parse default retention");
    assert_eq!(defaults.hooks.payload_retention_days, 7);

    let configured = Config::parse("[hooks]\npayload_retention_days = 30\n")
        .expect("parse configured retention");
    assert_eq!(configured.hooks.payload_retention_days, 30);

    let error = Config::parse("[hooks]\npayload_retention_days = 0\n")
        .expect_err("zero-day retention must be rejected");
    assert!(error.to_string().contains("payload_retention_days"));
}
