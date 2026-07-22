// Historical rejudge artifact-confirmation regression tests.

    fn append_test_confirmation_proposals(db: &Database, proposals_path: &Path, rowids: &[i64]) {
        for rowid in rowids {
            let drawer_id = format!("artifact-circuit-{rowid}");
            let row = db
                .historical_rejudge_candidate_by_rowid(*rowid, &drawer_id)
                .expect("load proposal candidate")
                .expect("proposal candidate exists");
            append_jsonl_record(
                proposals_path,
                &historical_rejudge_proposal_artifact_line(
                    &row,
                    &HistoricalRejudgeArtifactProposalDecision {
                        score: 0.2,
                        reason: "low value".to_string(),
                        should_forget: true,
                    },
                ),
            )
            .expect("write proposal");
        }
    }

    #[test]
    fn historical_rejudge_max_confirmations_cli_argument_is_preserved() {
        let command = top_level_rejudge_command(|args| args.max_confirmations = Some(25));
        let Commands::Maintenance {
            command: MaintenanceCommands::Rejudge(args),
        } = command
        else {
            panic!("expected maintenance rejudge command");
        };
        assert_eq!(args.max_confirmations, Some(25));
    }

    #[test]
    fn historical_rejudge_confirmation_circuit_breaker_diagnostic_is_sanitized_and_aggregate() {
        let error = anyhow::Error::new(LlmError::ClientError {
            status: reqwest::StatusCode::TOO_MANY_REQUESTS,
            body: "response body containing https://secret.example and prompt-secret".to_string(),
            retry_after: Some(std::time::Duration::from_secs(60)),
        });
        let diagnostic = historical_rejudge_confirmation_circuit_breaker_diagnostic(
            "spark",
            &error,
            HistoricalRejudgeArtifactConfirmationStats {
                pending_count: 100,
                processed_count: 6,
                confirmed_count: 2,
                ..Default::default()
            },
        );

        assert_eq!(diagnostic.lines().count(), 1, "{diagnostic}");
        assert!(diagnostic.contains("endpoint_id=spark"), "{diagnostic}");
        assert!(
            diagnostic.contains("failure_class=client_error"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("http_status=429"), "{diagnostic}");
        assert!(diagnostic.contains("retry_after_secs=60"), "{diagnostic}");
        assert!(diagnostic.contains("processed=6"), "{diagnostic}");
        assert!(diagnostic.contains("confirmed=2"), "{diagnostic}");
        assert!(diagnostic.contains("pending=98"), "{diagnostic}");
        assert!(!diagnostic.contains("response body"), "{diagnostic}");
        assert!(!diagnostic.contains("secret.example"), "{diagnostic}");
        assert!(!diagnostic.contains("prompt-secret"), "{diagnostic}");
    }

    #[test]
    fn historical_rejudge_confirmation_circuit_breaker_diagnostic_preserves_cooldown_status() {
        let error = anyhow::Error::new(LlmError::TemporarilyUnavailable {
            retry_after: std::time::Duration::from_secs(60),
            reason: "cooldown after https://secret.example and prompt-secret".to_string(),
            http_status: Some(reqwest::StatusCode::TOO_MANY_REQUESTS),
        });
        let diagnostic = historical_rejudge_confirmation_circuit_breaker_diagnostic(
            "spark",
            &error,
            HistoricalRejudgeArtifactConfirmationStats {
                pending_count: 3,
                processed_count: 1,
                ..Default::default()
            },
        );

        assert_eq!(diagnostic.lines().count(), 1, "{diagnostic}");
        assert!(
            diagnostic.contains("failure_class=temporarily_unavailable"),
            "{diagnostic}"
        );
        assert!(diagnostic.contains("http_status=429"), "{diagnostic}");
        assert!(diagnostic.contains("retry_after_secs=60"), "{diagnostic}");
        assert!(!diagnostic.contains("secret.example"), "{diagnostic}");
        assert!(!diagnostic.contains("prompt-secret"), "{diagnostic}");
    }

    #[tokio::test]
    async fn historical_rejudge_artifact_confirmation_circuit_breaker_stops_admission_and_keeps_backlog()
     {
        let mut confirm_server = mockito::Server::new_async().await;
        let successful_confirmations = confirm_server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(llm_chat_response("spark", 0.02, "confirm_delete"))
            .expect(2)
            .create_async()
            .await;
        let exhausted_confirmations = confirm_server
            .mock("POST", "/v1/chat/completions")
            .with_status(429)
            .with_header("Retry-After", "60")
            .with_body("response body containing https://secret.example and prompt-secret")
            .expect_at_least(1)
            .expect_at_most(4)
            .create_async()
            .await;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        let rowids = (0..100)
            .map(|index| {
                let drawer_id = format!("artifact-circuit-{}", index + 1);
                insert_drawer(
                    &db,
                    &drawer_id,
                    "Low-signal transient output requiring confirmation.",
                    "notes",
                    None,
                );
                drawer_rowid(&db, &drawer_id)
            })
            .collect::<Vec<_>>();
        let proposals_path = tmp.path().join("proposals.jsonl");
        let confirmations_path = tmp.path().join("confirmations.jsonl");
        append_test_confirmation_proposals(&db, &proposals_path, &rowids);
        let config = two_stage_llm_rejudge_config("http://proposal.invalid", &confirm_server.url());
        let options = HistoricalRejudgeOptions {
            proposal_llm_endpoint: Some("qwen"),
            confirm_llm_endpoint: Some("spark"),
            ..full_rejudge_options(false, None, 100)
        };
        let llm_context = historical_rejudge_llm_context(&config, options)
            .expect("build confirmation LLM context")
            .expect("two-stage context");
        let mut confirmation_keys = BTreeSet::new();

        let stats = confirm_historical_rejudge_artifact_backlog(
            &db,
            &config,
            Some(&llm_context),
            &proposals_path,
            &confirmations_path,
            &mut confirmation_keys,
            HistoricalRejudgeArtifactConfirmationDrainOptions {
                llm_concurrency: 4,
                max_confirmations: None,
            },
        )
        .await
        .expect("circuit-broken confirmation drain must return waiting stats");

        successful_confirmations.assert_async().await;
        exhausted_confirmations.assert_async().await;
        assert!(stats.circuit_broken, "{stats:?}");
        assert!(stats.processed_count <= 6, "{stats:?}");
        assert_eq!(stats.confirmed_count, 2, "{stats:?}");
        assert_eq!(confirmation_keys.len(), 2, "{stats:?}");
        assert!(stats.pending_count > stats.confirmed_count, "{stats:?}");
        assert_eq!(db.deleted_drawer_count().expect("deleted count"), 0);
        assert_eq!(active_drawer_count(&db), 100);
        assert_eq!(audit_count(&db), 0);
        assert_eq!(
            read_jsonl_records::<HistoricalRejudgeConfirmationArtifactLine>(&confirmations_path)
                .expect("read confirmations")
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn historical_rejudge_artifact_confirmation_respects_max_confirmations_and_deduplicates()
    {
        let mut confirm_server = mockito::Server::new_async().await;
        let confirmations = confirm_server
            .mock("POST", "/v1/chat/completions")
            .with_status(200)
            .with_body(llm_chat_response("spark", 0.02, "confirm_delete"))
            .expect(50)
            .create_async()
            .await;
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        let rowids = (0..100)
            .map(|index| {
                let drawer_id = format!("artifact-circuit-{}", index + 1);
                insert_drawer(
                    &db,
                    &drawer_id,
                    "Low-signal transient output requiring confirmation.",
                    "notes",
                    None,
                );
                drawer_rowid(&db, &drawer_id)
            })
            .collect::<Vec<_>>();
        let proposals_path = tmp.path().join("proposals.jsonl");
        let confirmations_path = tmp.path().join("confirmations.jsonl");
        append_test_confirmation_proposals(&db, &proposals_path, &rowids);
        let config = two_stage_llm_rejudge_config("http://proposal.invalid", &confirm_server.url());
        let options = HistoricalRejudgeOptions {
            proposal_llm_endpoint: Some("qwen"),
            confirm_llm_endpoint: Some("spark"),
            ..full_rejudge_options(false, None, 100)
        };
        let llm_context = historical_rejudge_llm_context(&config, options)
            .expect("build confirmation LLM context")
            .expect("two-stage context");
        let mut confirmation_keys = BTreeSet::new();

        for _ in 0..2 {
            let stats = confirm_historical_rejudge_artifact_backlog(
                &db,
                &config,
                Some(&llm_context),
                &proposals_path,
                &confirmations_path,
                &mut confirmation_keys,
                HistoricalRejudgeArtifactConfirmationDrainOptions {
                    llm_concurrency: 4,
                    max_confirmations: Some(25),
                },
            )
            .await
            .expect("bounded confirmation drain");
            assert!(stats.processed_count <= 25, "{stats:?}");
            assert_eq!(stats.confirmed_count, 25, "{stats:?}");
            assert!(!stats.circuit_broken, "{stats:?}");
        }

        confirmations.assert_async().await;
        let confirmation_lines =
            read_jsonl_records::<HistoricalRejudgeConfirmationArtifactLine>(&confirmations_path)
                .expect("read confirmations");
        let confirmation_keys_from_file =
            read_historical_rejudge_artifact_confirmation_keys(&confirmations_path)
                .expect("read confirmation keys");
        assert_eq!(confirmation_lines.len(), 50);
        assert_eq!(confirmation_keys_from_file.len(), 50);
        assert_eq!(confirmation_keys.len(), 50);
        assert_eq!(db.deleted_drawer_count().expect("deleted count"), 0);
        assert_eq!(active_drawer_count(&db), 100);
        assert_eq!(audit_count(&db), 0);
    }

    #[test]
    fn historical_rejudge_artifact_jsonl_and_cursor_round_trip() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let proposals_path = tmp.path().join("proposals.jsonl");
        let confirmations_path = tmp.path().join("confirmations.jsonl");
        let cursor_path = tmp.path().join("cursor.json");
        let proposal = HistoricalRejudgeProposalArtifactLine {
            drawer_id: "drawer-a".to_string(),
            drawer_rowid: 42,
            snapshot_content_hash: "snapshot-hash-a".to_string(),
            snapshot_importance: Some(1),
            snapshot_is_pinned: Some(false),
            snapshot_effective_importance: Some(1.0),
            snapshot_status: Some(String::new()),
            snapshot_memory_kind: Some("evidence".to_string()),
            snapshot_room: Some("Read".to_string()),
            snapshot_source_file: Some("artifact-delete.json".to_string()),
            snapshot_source_type: "agent_inference".to_string(),
            snapshot_chunk_index: Some(0),
            snapshot_normalize_version: Some(2),
            snapshot_project_id: Some("default".to_string()),
            proposal_decision: "forget".to_string(),
            proposal_score: 0.2,
            proposal_reason: "low value".to_string(),
            snapshot_added_at: "2026-01-01T00:00:00Z".to_string(),
            snapshot_wing: "hooks-raw".to_string(),
            timestamp: "2026-01-01T00:00:01Z".to_string(),
        };
        let confirmation = HistoricalRejudgeConfirmationArtifactLine {
            drawer_id: "drawer-a".to_string(),
            drawer_rowid: 42,
            snapshot_content_hash: proposal.snapshot_content_hash.clone(),
            snapshot_importance: proposal.snapshot_importance,
            snapshot_is_pinned: proposal.snapshot_is_pinned,
            snapshot_effective_importance: proposal.snapshot_effective_importance,
            snapshot_status: proposal.snapshot_status.clone(),
            snapshot_memory_kind: proposal.snapshot_memory_kind.clone(),
            snapshot_wing: Some(proposal.snapshot_wing.clone()),
            snapshot_room: proposal.snapshot_room.clone(),
            snapshot_source_file: proposal.snapshot_source_file.clone(),
            snapshot_source_type: proposal.snapshot_source_type.clone(),
            snapshot_chunk_index: proposal.snapshot_chunk_index,
            snapshot_normalize_version: proposal.snapshot_normalize_version,
            snapshot_project_id: proposal.snapshot_project_id.clone(),
            final_decision: "delete".to_string(),
            confirm_score: 0.1,
            confirm_reason: "confirmed low value".to_string(),
            proposal_score: proposal.proposal_score,
            snapshot_added_at: proposal.snapshot_added_at.clone(),
            timestamp: "2026-01-01T00:00:02Z".to_string(),
        };
        append_jsonl_record(&proposals_path, &proposal).expect("write proposal");
        append_jsonl_record(&confirmations_path, &confirmation).expect("write confirmation");
        write_historical_rejudge_artifact_cursor(
            &cursor_path,
            &HistoricalRejudgeArtifactCursor {
                last_processed_rowid: 42,
                options_hash: "options".to_string(),
                judge_model: "proposal:qwen, confirm:spark".to_string(),
                started_at: "2026-01-01T00:00:00Z".to_string(),
                updated_at: "2026-01-01T00:00:03Z".to_string(),
            },
        )
        .expect("write cursor");

        let proposals =
            read_jsonl_records::<HistoricalRejudgeProposalArtifactLine>(&proposals_path)
                .expect("read proposals");
        let confirmation_keys =
            read_historical_rejudge_artifact_confirmation_keys(&confirmations_path)
                .expect("read confirmation keys");
        let confirmations =
            read_jsonl_records::<HistoricalRejudgeConfirmationArtifactLine>(&confirmations_path)
                .expect("read confirmations");
        let cursor = read_historical_rejudge_artifact_cursor(&cursor_path).expect("read cursor");

        assert_eq!(proposals.len(), 1);
        assert_eq!(proposals[0].proposal_decision, "forget");
        assert_eq!(proposals[0].snapshot_content_hash, "snapshot-hash-a");
        assert_eq!(
            proposals[0].snapshot_source_file.as_deref(),
            Some("artifact-delete.json")
        );
        assert_eq!(proposals[0].snapshot_source_type, "agent_inference");
        assert_eq!(proposals[0].snapshot_chunk_index, Some(0));
        assert_eq!(proposals[0].snapshot_normalize_version, Some(2));
        assert_eq!(confirmations.len(), 1);
        assert_eq!(
            confirmations[0].snapshot_source_file.as_deref(),
            Some("artifact-delete.json")
        );
        assert_eq!(confirmations[0].snapshot_source_type, "agent_inference");
        assert_eq!(confirmations[0].snapshot_chunk_index, Some(0));
        assert_eq!(confirmations[0].snapshot_normalize_version, Some(2));
        assert!(confirmation_keys.contains("42:drawer-a"));
        assert_eq!(cursor.last_processed_rowid, 42);
        assert_eq!(cursor.options_hash, "options");
    }

    #[tokio::test]
    async fn historical_rejudge_artifact_confirmation_skips_provenance_drift() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        let drawer = Drawer {
            id: "artifact-proposal-provenance".to_string(),
            content: "low value hook transcript".to_string(),
            wing: "hooks-raw".to_string(),
            room: Some("Read".to_string()),
            source_file: Some("artifact-proposal-original.json".to_string()),
            source_type: SourceType::AgentInference,
            added_at: "2026-01-01T00:00:00Z".to_string(),
            chunk_index: Some(0),
            normalize_version: 2,
            ..Drawer::default()
        };
        db.insert_drawer(&drawer).expect("insert drawer");
        let rowid = drawer_rowid(&db, "artifact-proposal-provenance");
        let row = db
            .historical_rejudge_candidate_by_rowid(rowid, "artifact-proposal-provenance")
            .expect("load proposal candidate")
            .expect("proposal candidate exists");
        let proposal = historical_rejudge_proposal_artifact_line(
            &row,
            &HistoricalRejudgeArtifactProposalDecision {
                score: 0.2,
                reason: "low value".to_string(),
                should_forget: true,
            },
        );
        let proposals_path = tmp.path().join("proposals.jsonl");
        let confirmations_path = tmp.path().join("confirmations.jsonl");
        append_jsonl_record(&proposals_path, &proposal).expect("write proposal");
        db.conn()
            .execute(
                r#"
                UPDATE drawers
                SET source_file = 'artifact-proposal-moved.json',
                    source_type = 'user_explicit',
                    chunk_index = 7,
                    normalize_version = 99,
                    updated_at = ?1
                WHERE id = 'artifact-proposal-provenance'
                "#,
                [current_timestamp()],
            )
            .expect("concurrent provenance update");
        let mut confirmation_keys = BTreeSet::new();

        confirm_historical_rejudge_artifact_backlog(
            &db,
            &Config::default(),
            None,
            &proposals_path,
            &confirmations_path,
            &mut confirmation_keys,
            HistoricalRejudgeArtifactConfirmationDrainOptions {
                llm_concurrency: 1,
                max_confirmations: None,
            },
        )
        .await
        .expect("provenance drift should skip before requiring LLM confirmation");

        let confirmations =
            read_jsonl_records::<HistoricalRejudgeConfirmationArtifactLine>(&confirmations_path)
                .expect("read confirmations");
        assert!(confirmations.is_empty());
        assert!(confirmation_keys.is_empty());
    }

    #[test]
    fn historical_rejudge_apply_dry_run_reads_confirmations_without_deleting() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        let drawer = Drawer {
            id: "artifact-delete".to_string(),
            content: "low value hook transcript".to_string(),
            wing: "hooks-raw".to_string(),
            room: Some("Read".to_string()),
            source_file: Some("artifact-delete.json".to_string()),
            source_type: SourceType::AgentInference,
            added_at: "2026-01-01T00:00:00Z".to_string(),
            ..Drawer::default()
        };
        db.insert_drawer(&drawer).expect("insert drawer");
        let rowid = db
            .conn()
            .query_row(
                "SELECT rowid FROM drawers WHERE id = ?1",
                ["artifact-delete"],
                |row| row.get::<_, i64>(0),
            )
            .expect("load rowid");
        let confirmations_path = tmp.path().join("confirmations.jsonl");
        append_test_delete_confirmation(&db, &confirmations_path, rowid, "artifact-delete");
        let backup_dir = tmp.path().join("backups");

        maintenance_rejudge_apply_command(
            &db,
            &confirmations_path,
            &backup_dir,
            false,
            false,
            "json",
        )
        .expect("dry-run apply");

        assert_eq!(
            drawer_deleted_at(&db, "artifact-delete").expect("load deleted_at"),
            Some(None)
        );
        assert!(
            fs::read_dir(&backup_dir).is_err(),
            "dry-run apply must not create backup artifacts"
        );
    }
