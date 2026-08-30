#[test]
fn test_large_capture_establishes_missing_home_before_retention_lock() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let mempal_home = tmp.path().join("missing/.mempal");
    let bytes = vec![b'x'; MAX_ENVELOPE_INLINE_PAYLOAD_BYTES + 1];

    let captured = capture_stdin_payload(bytes, &mempal_home)
        .expect("large capture must establish a missing mempal home");
    let payload_path = captured.payload_path.expect("large capture path");

    assert!(mempal_home.is_dir());
    assert_eq!(
        fs::read_to_string(payload_path).expect("read spooled payload"),
        "x".repeat(MAX_ENVELOPE_INLINE_PAYLOAD_BYTES + 1)
    );
}

#[test]
fn test_large_capture_syncs_payload_before_publishing_handle() {
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let raw_payload = "x".repeat(MAX_ENVELOPE_INLINE_PAYLOAD_BYTES + 1);
    crate::hook_payload::HOOK_SPOOL_SYNC_EVENTS.with(|events| events.borrow_mut().clear());

    let payload_path = spool_hook_payload(&raw_payload, tmp.path()).expect("spool payload");

    assert_eq!(
        crate::hook_payload::HOOK_SPOOL_SYNC_EVENTS.with(|events| events.borrow().clone()),
        vec!["payload", "directory"]
    );
    assert_eq!(
        fs::read_to_string(payload_path).expect("read published payload"),
        raw_payload
    );
}
