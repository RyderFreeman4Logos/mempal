use anyhow::{Context, Result};

use crate::core::{AsyncDb, db::Database, types::RuntimeWriterLease};

pub(super) fn finalize_admission_owner_after_completion(
    db: &Database,
    drawer_id: &str,
) -> Result<()> {
    db.conn()
        .execute(
            "UPDATE drawers SET admission_owner = NULL WHERE id = ?1",
            [drawer_id],
        )
        .with_context(|| format!("failed to finalize hook drawer admission {drawer_id}"))?;
    Ok(())
}

pub(super) async fn soft_delete_model_rejected_admission(
    db: &AsyncDb,
    runtime_writer_lease: Option<&RuntimeWriterLease>,
    drawer_id: &str,
    admission_owner: &str,
) -> Result<()> {
    let drawer_id = drawer_id.to_string();
    let admission_owner = admission_owner.to_string();
    let runtime_writer_lease = runtime_writer_lease.cloned();
    db.run_write_anyhow(move |db| {
        super::with_daemon_runtime_writer_lease_write(
            db,
            runtime_writer_lease.as_ref(),
            "soft-delete model-rejected hook drawer",
            || {
                let deleted_at = super::current_timestamp();
                db.conn()
                    .execute(
                        "UPDATE drawers SET deleted_at = ?1 \
                         WHERE id = ?2 AND admission_owner = ?3 AND deleted_at IS NULL",
                        [&deleted_at, &drawer_id, &admission_owner],
                    )
                    .with_context(|| format!("failed to soft-delete hook drawer {drawer_id}"))?;
                Ok(())
            },
        )
    })
    .await?;
    // Payload retention owns filesystem deletion: `source_file` is citation data,
    // and unlinking here would race concurrent admissions that share a payload.
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::core::{
        AsyncDb,
        config::{Config, HooksSessionEndConfig, LlmJudgeConfig},
        db::Database,
        queue::{AsyncPendingMessageStore, PendingMessageStore},
    };
    use crate::embed::Embedder;
    use crate::hook::{CapturedHookEnvelope, HookEvent};
    use crate::session_review::{SessionReviewOutcome, extract_session_review};

    use super::super::{
        DaemonIngestContext, DrawerRecord, HookLlmGateRuntime, insert_drawer_with_admission_owner,
        process_claimed_message_with_embedder, raw_payload_storage_path,
    };

    struct StaticEmbedder;

    #[async_trait::async_trait]
    impl Embedder for StaticEmbedder {
        async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
            Ok(texts.iter().map(|_| vec![0.1, 0.2, 0.3]).collect())
        }

        fn dimensions(&self) -> usize {
            3
        }

        fn name(&self) -> &str {
            "static-test"
        }
    }

    #[tokio::test]
    async fn model_rejection_soft_deletes_admitted_drawer_without_unlinking_payload() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let victim = tmp.path().join("session-id-victim.txt");
        std::fs::write(&victim, "must survive model rejection").expect("write victim");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let payload = serde_json::json!({
            "session_id": victim,
            "messages": [{"role": "assistant", "content": "retain this review"}],
            "tool_calls": []
        })
        .to_string();
        let review = match extract_session_review(
            Some(&payload),
            "codex",
            &HooksSessionEndConfig {
                extract_self_review: true,
                min_length: 1,
                ..HooksSessionEndConfig::default()
            },
        )
        .expect("extract session review")
        {
            SessionReviewOutcome::Review(review) => review,
            outcome => panic!("expected session review, got {outcome:?}"),
        };
        let record = DrawerRecord {
            wing: review.wing,
            room: review.room,
            source_file: review.source_file,
            content: review.content,
            added_at: "2026-05-01T12:34:56Z".to_string(),
            importance: review.importance,
            bypass_novelty: true,
            project_id: None,
            deferred_raw_payload: None,
            deferred_raw_payload_path: None,
        };
        insert_drawer_with_admission_owner(&db, "rejected-review", &record, Some("owner"))
            .expect("insert admission");

        super::soft_delete_model_rejected_admission(&async_db, None, "rejected-review", "owner")
            .await
            .expect("discard admission");

        assert!(
            db.drawer_is_soft_deleted("rejected-review")
                .expect("rejected admission remains as a durable soft-deleted row")
        );
        assert_eq!(
            std::fs::read_to_string(&victim).expect("victim must remain"),
            "must survive model rejection"
        );
    }

    #[tokio::test]
    async fn retry_rejection_does_not_discard_another_messages_completed_drawer() {
        let worker_test_lock = crate::llm::acquire_llm_worker_test_lock();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let hook_payload = serde_json::json!({
            "tool_name": "DesignCapture",
            "input": "record completed capture ownership",
            "output": "A completed capture must retain its drawer when a later retried duplicate is rejected.",
            "exit_code": 0
        })
        .to_string();
        let envelope = CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "codex".to_string(),
            captured_at: "2026-05-01T12:34:56Z".to_string(),
            claude_cwd: tmp.path().to_string_lossy().to_string(),
            payload: Some(hook_payload.clone()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: hook_payload.len(),
            truncated: false,
        };
        let payload = serde_json::to_string(&envelope).expect("serialize envelope");
        store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue first capture");
        let first = store
            .claim_next("completed-capture-worker", 60)
            .expect("claim first capture")
            .expect("first capture");
        let first_drawer_id = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "completed-capture-worker",
            &first,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                llm_gate: None,
                config: &Config::default(),
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
                heartbeat_trigger: None,
            },
        )
        .await
        .expect("complete first capture");
        store.confirm(&first).expect("confirm first capture");

        store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue later capture");
        let mut retry = store
            .claim_next("retried-capture-worker", 60)
            .expect("claim later capture")
            .expect("later capture");
        assert_ne!(
            retry.id, first.id,
            "captures must have distinct queue owners"
        );
        retry.retry_count = 1;

        let mut llm_server = mockito::Server::new_async().await;
        let llm_mock = llm_server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(r#"{"model":"test-llm","choices":[{"message":{"role":"assistant","content":"{\"verdict\":\"reject\",\"score\":0.05}"}}]}"#)
            .create_async()
            .await;
        let mut config = Config::default();
        config.ingest_gating.enabled = true;
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        config.llm.enabled = true;
        config.llm.base_url = Some(format!("{}/v1", llm_server.url()));
        config.llm.model = Some("test-llm".to_string());
        config.llm.enabled_for = vec!["gating".to_string()];
        let llm_gate = HookLlmGateRuntime::new_with_worker_test_lock(&config.llm, worker_test_lock);

        let rejected_drawer_id = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "retried-capture-worker",
            &retry,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                llm_gate: Some(&llm_gate),
                config: &config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
                heartbeat_trigger: None,
            },
        )
        .await
        .expect("reject later retried capture");
        llm_mock.assert_async().await;

        assert_eq!(rejected_drawer_id, first_drawer_id);
        assert!(
            db.drawer_exists(&first_drawer_id)
                .expect("first drawer exists after later rejection"),
            "later retry rejection must not delete another message's completed drawer"
        );
        let vector_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM drawer_vectors WHERE id = ?1",
                [first_drawer_id.as_str()],
                |row| row.get(0),
            )
            .expect("first drawer vector count");
        assert_eq!(vector_count, 1, "completed drawer vector must remain");
        assert!(
            raw_payload_storage_path(&hook_payload, tmp.path()).exists(),
            "completed drawer raw payload must remain"
        );
    }

    #[tokio::test]
    async fn retry_rejection_does_not_discard_completed_drawer_owned_by_stale_admission() {
        let worker_test_lock = crate::llm::acquire_llm_worker_test_lock();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let hook_payload = serde_json::json!({
            "tool_name": "DesignCapture",
            "input": "record stale admission ownership",
            "output": "A completed duplicate must survive its earlier failed admission retry.",
            "exit_code": 0
        })
        .to_string();
        let envelope = CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "codex".to_string(),
            captured_at: "2026-05-01T12:34:56Z".to_string(),
            claude_cwd: tmp.path().to_string_lossy().to_string(),
            payload: Some(hook_payload.clone()),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: hook_payload.len(),
            truncated: false,
        };
        let payload = serde_json::to_string(&envelope).expect("serialize envelope");
        store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue failed admission");
        let failed_admission = store
            .claim_next("failed-admission-worker", 60)
            .expect("claim failed admission")
            .expect("failed admission");
        let mut unavailable_config = Config::default();
        unavailable_config.ingest_gating.enabled = true;
        unavailable_config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "failed-admission-worker",
            &failed_admission,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                llm_gate: None,
                config: &unavailable_config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
                heartbeat_trigger: None,
            },
        )
        .await
        .expect_err("first admission must fail after persisting its drawer");

        store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue completed duplicate");
        let completed_duplicate = store
            .claim_next("completed-duplicate-worker", 60)
            .expect("claim completed duplicate")
            .expect("completed duplicate");
        let drawer_id = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "completed-duplicate-worker",
            &completed_duplicate,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                llm_gate: None,
                config: &Config::default(),
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
                heartbeat_trigger: None,
            },
        )
        .await
        .expect("complete duplicate");

        let mut llm_server = mockito::Server::new_async().await;
        let llm_mock = llm_server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(r#"{"model":"test-llm","choices":[{"message":{"role":"assistant","content":"{\"verdict\":\"reject\",\"score\":0.05}"}}]}"#)
            .create_async()
            .await;
        unavailable_config.llm.enabled = true;
        unavailable_config.llm.base_url = Some(format!("{}/v1", llm_server.url()));
        unavailable_config.llm.model = Some("test-llm".to_string());
        unavailable_config.llm.enabled_for = vec!["gating".to_string()];
        let llm_gate = HookLlmGateRuntime::new_with_worker_test_lock(
            &unavailable_config.llm,
            worker_test_lock,
        );
        let mut retry = failed_admission.clone();
        retry.retry_count = 1;
        process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "failed-admission-worker",
            &retry,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                llm_gate: Some(&llm_gate),
                config: &unavailable_config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
                heartbeat_trigger: None,
            },
        )
        .await
        .expect("reject retry without stealing completed duplicate");
        llm_mock.assert_async().await;

        assert!(
            db.drawer_exists(&drawer_id)
                .expect("completed duplicate drawer exists after stale retry rejection"),
            "stale retry rejection must not delete another message's completed drawer"
        );
        let vector_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM drawer_vectors WHERE id = ?1",
                [drawer_id.as_str()],
                |row| row.get(0),
            )
            .expect("completed duplicate vector count");
        assert_eq!(vector_count, 1, "completed duplicate vector must remain");
    }

    #[tokio::test]
    async fn terminal_model_misses_reconcile_only_the_exact_admission_owner() {
        let worker_test_lock = crate::llm::acquire_llm_worker_test_lock();
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db_path = tmp.path().join("palace.db");
        let db = Database::open(&db_path).expect("open db");
        let async_db = AsyncDb::open(&db_path, 4).expect("open async db");
        let store = PendingMessageStore::new(db.path()).expect("open queue");
        let async_store = AsyncPendingMessageStore::from_store(store.clone());
        let foreign_record = DrawerRecord {
            wing: "hooks-raw".to_string(),
            room: "foreign".to_string(),
            source_file: "synthetic://foreign".to_string(),
            content: "foreign admission must survive".to_string(),
            added_at: "2026-09-08T12:00:00Z".to_string(),
            importance: 0,
            bypass_novelty: false,
            project_id: None,
            deferred_raw_payload: None,
            deferred_raw_payload_path: None,
        };
        insert_drawer_with_admission_owner(
            &db,
            "foreign-drawer",
            &foreign_record,
            Some("foreign-operation"),
        )
        .expect("insert foreign admission");

        let hook_payload = serde_json::json!({
            "tool_name": "DesignCapture",
            "input": "exercise terminal endpoint misses",
            "output": "The exact failed admission must be reconciled without touching foreign work.",
            "exit_code": 0
        })
        .to_string();
        let envelope = CapturedHookEnvelope {
            event: HookEvent::PostToolUse.display_name().to_string(),
            kind: HookEvent::PostToolUse.queue_kind().to_string(),
            agent: "codex".to_string(),
            captured_at: "2026-09-08T12:00:00Z".to_string(),
            claude_cwd: tmp.path().to_string_lossy().to_string(),
            payload: Some(hook_payload),
            payload_path: None,
            payload_preview: None,
            original_size_bytes: 128,
            truncated: false,
        };
        let payload = serde_json::to_string(&envelope).expect("serialize envelope");
        let operation_id = store
            .enqueue(HookEvent::PostToolUse.queue_kind(), &payload)
            .expect("enqueue hook envelope");
        let message = store
            .claim_next("terminal-model-worker", 60)
            .expect("claim hook envelope")
            .expect("claimed hook envelope");
        assert_eq!(message.id, operation_id);

        let mut primary = mockito::Server::new_async().await;
        let mut secondary = mockito::Server::new_async().await;
        let primary_mock = primary
            .mock("POST", "/v1/chat/completions")
            .with_status(404)
            .with_body("primary model missing")
            .expect(1)
            .create_async()
            .await;
        let secondary_mock = secondary
            .mock("POST", "/v1/chat/completions")
            .with_status(404)
            .with_body("secondary model missing")
            .expect(1)
            .create_async()
            .await;
        let mut config = Config::parse(&format!(
            r#"
[llm]
enabled = true
enabled_for = ["gating"]

[[llm.endpoints]]
id = "primary"
base_url = "{}/v1"
model = "missing-primary"

[[llm.endpoints]]
id = "secondary"
base_url = "{}/v1"
model = "missing-secondary"
"#,
            primary.url(),
            secondary.url()
        ))
        .expect("parse endpoint pool config");
        config.ingest_gating.enabled = true;
        config.ingest_gating.llm_judge = Some(LlmJudgeConfig {
            enabled: true,
            ..LlmJudgeConfig::default()
        });
        let llm_gate = HookLlmGateRuntime::new_with_worker_test_lock(&config.llm, worker_test_lock);

        let error = process_claimed_message_with_embedder(
            &async_db,
            &async_store,
            "terminal-model-worker",
            &message,
            &StaticEmbedder,
            DaemonIngestContext {
                prototype_classifier: None,
                llm_gate: Some(&llm_gate),
                config: &config,
                mempal_home: tmp.path(),
                runtime_writer_lease: None,
                heartbeat_trigger: None,
            },
        )
        .await
        .expect_err("all endpoint model misses must fail terminally");
        primary_mock.assert_async().await;
        secondary_mock.assert_async().await;
        let disposition = super::super::queue_failure_disposition(&error);
        assert_eq!(
            disposition,
            crate::core::queue::QueueFailureDisposition::Terminal
        );
        store
            .mark_failed_with_disposition(&message, &format!("{error:#}"), disposition)
            .expect("record terminal operation receipt");

        let (candidate_id, deleted_at, creation_operation_id): (
            String,
            Option<String>,
            Option<String>,
        ) = db
            .conn()
            .query_row(
                "SELECT id, deleted_at, creation_operation_id FROM drawers WHERE admission_owner = ?1",
                [operation_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("load exact failed admission");
        assert!(deleted_at.is_some(), "terminal admission must be inactive");
        assert!(creation_operation_id.is_none());
        let active_residue: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM drawers WHERE id = ?1 AND deleted_at IS NULL",
                [candidate_id.as_str()],
                |row| row.get(0),
            )
            .expect("count active candidate residue");
        assert_eq!(active_residue, 0);
        let foreign_owner: String = db
            .conn()
            .query_row(
                "SELECT admission_owner FROM drawers WHERE id = 'foreign-drawer' AND deleted_at IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("foreign admission remains active");
        assert_eq!(foreign_owner, "foreign-operation");

        let (status, op_state, failure_class, retry_count, last_error): (
            String,
            String,
            Option<String>,
            i64,
            Option<String>,
        ) = db
            .conn()
            .query_row(
                "SELECT status, op_state, failure_class, retry_count, last_error FROM pending_messages WHERE id = ?1",
                [operation_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            )
            .expect("load terminal operation receipt");
        assert_eq!((status.as_str(), op_state.as_str()), ("failed", "failed"));
        assert_eq!(failure_class.as_deref(), Some("terminal"));
        assert_eq!(retry_count, 1);
        assert!(
            last_error.as_deref().is_some_and(
                |error| error.contains("LLM gating request failed") && error.contains("404")
            ),
            "last_error={last_error:?}"
        );
        let verdict_count: i64 = db
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM gating_audit WHERE candidate_hash = ?1 AND llm_verdict IS NOT NULL",
                [candidate_id.as_str()],
                |row| row.get(0),
            )
            .expect("count completed verdicts");
        assert_eq!(verdict_count, 0, "requests are not completed verdicts");
    }
}
