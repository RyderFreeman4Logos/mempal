use super::*;

#[test]
fn cli_hybrid_timeout_leaves_bm25_budget_for_followup() {
    let total_deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let hybrid_deadline = cli_search_hybrid_deadline_with_bm25_reserve(
        std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(2),
        std::time::Duration::from_millis(500),
    );
    assert_eq!(hybrid_deadline, std::time::Duration::from_millis(1500));

    let hybrid = run_cli_search_read_bounded("hybrid-test", hybrid_deadline, || {
        std::thread::sleep(std::time::Duration::from_millis(1600));
    })
    .expect("bounded hybrid stage");
    assert!(hybrid.is_none(), "hybrid stage should time out");

    let bm25 = run_cli_search_read_bounded(
        "bm25-test",
        cli_search_remaining_deadline(total_deadline).expect("BM25 reserve remains"),
        || true,
    )
    .expect("bounded BM25 fallback");
    assert_eq!(
        bm25,
        Some(true),
        "BM25 must be attempted after hybrid timeout"
    );
}

#[test]
fn cli_search_hybrid_deadline_reserves_default_bm25_budget() {
    assert_eq!(
        cli_search_hybrid_deadline(
            std::time::Duration::from_secs(240),
            CLI_SEARCH_TOTAL_DEADLINE,
            true,
        ),
        std::time::Duration::from_secs(90),
    );
    assert_eq!(
        cli_search_hybrid_deadline(
            std::time::Duration::from_secs(240),
            std::time::Duration::from_secs(20),
            true,
        ),
        std::time::Duration::ZERO,
    );
}

#[test]
fn cli_search_hybrid_deadline_uses_full_remaining_budget_when_bm25_fallback_disabled() {
    for (db_deadline, remaining_deadline) in [
        (
            std::time::Duration::from_secs(240),
            std::time::Duration::from_secs(100),
        ),
        (
            std::time::Duration::from_secs(240),
            std::time::Duration::from_secs(20),
        ),
    ] {
        assert_eq!(
            cli_search_hybrid_deadline(db_deadline, remaining_deadline, false),
            db_deadline.min(remaining_deadline),
        );
    }
}
