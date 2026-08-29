use std::{fs, path::Path};

use anyhow::{Context, Result};

use crate::core::{AsyncDb, types::RuntimeWriterLease};

pub(super) async fn discard_model_rejected_admission(
    db: &AsyncDb,
    runtime_writer_lease: Option<&RuntimeWriterLease>,
    drawer_id: &str,
    source_file: &str,
    admission_owner: &str,
) -> Result<()> {
    let drawer_id = drawer_id.to_string();
    let source_file = source_file.to_string();
    let source_file_for_db = source_file.clone();
    let admission_owner = admission_owner.to_string();
    let runtime_writer_lease = runtime_writer_lease.cloned();
    let payload_unreferenced = db
        .run_write_anyhow(move |db| {
            super::with_daemon_runtime_writer_lease_write(
                db,
                runtime_writer_lease.as_ref(),
                "discard model-rejected hook drawer",
                || {
                    let deleted = db
                        .conn()
                        .execute(
                            "DELETE FROM drawers WHERE id = ?1 AND admission_owner = ?2",
                            [&drawer_id, &admission_owner],
                        )
                        .with_context(|| format!("failed to discard hook drawer {drawer_id}"))?;
                    if deleted == 0 {
                        return Ok(false);
                    }
                    let referenced = db.conn().query_row(
                        "SELECT EXISTS(SELECT 1 FROM drawers WHERE source_file = ?1 AND deleted_at IS NULL)",
                        [source_file_for_db.as_str()],
                        |row| row.get::<_, i64>(0),
                    )?;
                    Ok(referenced == 0)
                },
            )
        })
        .await?;
    if payload_unreferenced {
        match fs::remove_file(&source_file) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to discard hook payload {source_file}"));
            }
        }
        if let Some(parent) = Path::new(&source_file).parent() {
            match fs::remove_dir(parent) {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "failed to discard hook payload directory {}",
                            parent.display()
                        )
                    });
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::core::{
        AsyncDb,
        config::{Config, LlmJudgeConfig},
        db::Database,
        queue::{AsyncPendingMessageStore, PendingMessageStore},
    };
    use crate::embed::Embedder;
    use crate::hook::{CapturedHookEnvelope, HookEvent};

    use super::super::{
        DaemonIngestContext, HookLlmGateRuntime, process_claimed_message_with_embedder,
        raw_payload_storage_path,
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
}
