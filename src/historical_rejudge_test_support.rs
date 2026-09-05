use super::*;
use crate::historical_rejudge_tests::{
    audit_count, backup_dir, full_rejudge_options,
    historical_rejudge_confirm_pending_count_for_rows, insert_low_signal_rejudge_drawers,
    llm_chat_response, two_stage_llm_rejudge_config,
};

// ponytail: one process-local historical-rejudge class lock; split by fixture family if throughput matters.
static HISTORICAL_REJUDGE_CLASS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub(super) async fn historical_rejudge_class_lock() -> tokio::sync::MutexGuard<'static, ()> {
    HISTORICAL_REJUDGE_CLASS_LOCK.lock().await
}

#[tokio::test]
async fn historical_rejudge_paired_spark_exhausted_persists_confirm_backlog_and_continues_qwen() {
    let _class_lock = historical_rejudge_class_lock().await;
    let mut proposal_server = mockito::Server::new_async().await;
    let mut confirm_server = mockito::Server::new_async().await;
    let proposal_mock = proposal_server
        .mock("POST", "/v1/chat/completions")
        .with_status(200)
        .with_body(llm_chat_response(
            "qwen3.6-27b-decensor-by-aeon",
            0.05,
            "proposal_delete_secret_reason",
        ))
        .expect(6)
        .create_async()
        .await;
    let exhausted_confirm_mock = confirm_server
        .mock("POST", "/v1/chat/completions")
        .with_status(429)
        .with_header("Retry-After", "0")
        .with_body("spark quota exhausted raw body")
        .expect(6)
        .create_async()
        .await;
    let tmp = tempfile::TempDir::new().expect("tempdir");
    let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
    let rowids = insert_low_signal_rejudge_drawers(&db, "paired-spark-exhausted", 6);
    let backups = backup_dir(&tmp);
    let progress_file = tmp.path().join("paired-spark-exhausted-progress.jsonl");
    let config = two_stage_llm_rejudge_config(&proposal_server.url(), &confirm_server.url());

    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        maintenance_rejudge_command_with_runtime(
            &db,
            &config,
            HistoricalRejudgeOptions {
                progress_file: Some(&progress_file),
                proposal_llm_endpoint: Some("qwen"),
                confirm_llm_endpoint: Some("spark"),
                ..full_rejudge_options(true, Some(&backups), 6)
            },
            HistoricalRejudgeRuntimeOptions {
                llm_concurrency: 6,
                ..HistoricalRejudgeRuntimeOptions::default()
            },
        ),
    )
    .await
    .expect("Spark quota exhaustion must not keep paired rejudge sleeping forever")
    .expect("paired rejudge should preserve Qwen proposals as confirmation backlog");

    proposal_mock.assert_async().await;
    exhausted_confirm_mock.assert_async().await;
    assert_eq!(db.deleted_drawer_count().expect("deleted count"), 0);
    assert_eq!(
        audit_count(&db),
        0,
        "unconfirmed proposals must not audit a final mutation"
    );
    let checkpoint = load_historical_rejudge_checkpoint(&db)
        .expect("load checkpoint")
        .expect("checkpoint");
    assert_eq!(checkpoint.status, "confirm_pending");
    assert_eq!(checkpoint.mutated_count, 0);
    let backlog =
        historical_rejudge_backlog_counts(&db, &checkpoint.run_id).expect("load backlog counts");
    assert_eq!(backlog.no_stage_pending_count, 0);
    assert_eq!(backlog.confirm_pending_count, 6);
    assert_eq!(
        historical_rejudge_confirm_pending_count_for_rows(&db, &checkpoint.run_id, &rowids),
        6
    );

    let raw_progress = std::fs::read_to_string(&progress_file).expect("read progress");
    assert!(
        !raw_progress.contains("Low-signal transient output"),
        "{raw_progress}"
    );
    assert!(
        !raw_progress.contains("paired-spark-exhausted"),
        "{raw_progress}"
    );
    assert!(
        !raw_progress.contains("proposal_delete_secret_reason"),
        "{raw_progress}"
    );
    assert!(
        !raw_progress.contains("spark quota exhausted raw body"),
        "{raw_progress}"
    );
}
