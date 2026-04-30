use mempal::llm::status::LlmStatus;

#[test]
fn test_llm_status_degrades_after_threshold() {
    let status = LlmStatus::new(10);
    for i in 0..9 {
        status.record_failure(&format!("error {i}"));
        assert!(!status.is_degraded(), "should not degrade at {i} failures");
    }
    status.record_failure(&"error 9");
    assert!(status.is_degraded());
}

#[test]
fn test_llm_status_recovers_on_success() {
    let status = LlmStatus::new(3);
    for _ in 0..5 {
        status.record_failure(&"error");
    }
    assert!(status.is_degraded());
    status.record_success();
    assert!(!status.is_degraded());
    assert_eq!(status.snapshot().fail_count, 0);
    assert!(status.snapshot().last_error.is_none());
}

#[test]
fn test_llm_status_collect_warnings_degraded() {
    let status = LlmStatus::new(2);
    status.record_failure(&"boom");
    status.record_failure(&"boom2");
    let warnings = status.collect_warnings();
    assert!(!warnings.is_empty());
    assert_eq!(warnings[0].level, "error");
    assert_eq!(warnings[0].source, "llm");
    assert!(warnings[0].message.contains("degraded"));
}

#[test]
fn test_llm_status_collect_warnings_healthy() {
    let status = LlmStatus::new(10);
    let warnings = status.collect_warnings();
    assert!(warnings.is_empty());
}

#[test]
fn test_llm_status_should_block_writes_when_degraded() {
    let status = LlmStatus::new(1);
    assert!(!status.should_block_writes());
    status.record_failure(&"error");
    assert!(status.should_block_writes());
    status.record_success();
    assert!(!status.should_block_writes());
}

#[test]
fn test_llm_status_snapshot() {
    let status = LlmStatus::new(10);
    status.record_failure(&"connection refused");
    let snapshot = status.snapshot();
    assert_eq!(snapshot.fail_count, 1);
    assert!(!snapshot.degraded);
    assert_eq!(snapshot.last_error.as_deref(), Some("connection refused"));
}
