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
        let llm_gate = HookLlmGateRuntime::new(&config.llm);

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
        let llm_gate = HookLlmGateRuntime::new(&unavailable_config.llm);
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
}
