// Historical rejudge apply planning and execution regression tests.

    #[derive(Deserialize)]
    struct HistoricalRejudgeApplyCase {
        drawer_id: String,
        decision: String,
        drift: String,
    }

    fn write_test_historical_rejudge_manifest(
        config: &Config,
        artifact_dir: &Path,
        source_snapshot_count: usize,
        source_snapshot_max_rowid: i64,
    ) -> HistoricalRejudgeArtifactManifest {
        let proposals_path = artifact_dir.join("proposals.jsonl");
        let confirmations_path = artifact_dir.join("confirmations.jsonl");
        let cursor_path = artifact_dir.join("cursor.json");
        let manifest = HistoricalRejudgeArtifactManifest {
            schema_version: HISTORICAL_REJUDGE_ARTIFACT_SCHEMA_VERSION.to_string(),
            generation_id: "historical-rejudge-fixture-generation".to_string(),
            source_snapshot_count,
            source_snapshot_max_rowid,
            proposal_stage_status: HistoricalRejudgeArtifactStageStatus::Completed,
            confirmation_stage_status: HistoricalRejudgeArtifactStageStatus::Completed,
            config_hash: historical_rejudge_artifact_config_hash(config)
                .expect("artifact config hash"),
            options_hash: sha256_hex(b"fixture-artifact-options"),
            proposal_endpoint_model_fingerprint: sha256_hex(b"fixture-proposal-endpoint"),
            confirmation_endpoint_model_fingerprint: sha256_hex(
                b"fixture-confirmation-endpoint",
            ),
            policy_fingerprint: historical_rejudge_judge_policy_fingerprint(config)
                .expect("policy fingerprint"),
            proposals_file_sha256: historical_rejudge_artifact_file_sha256(&proposals_path)
                .expect("proposal hash"),
            confirmations_file_sha256: historical_rejudge_artifact_file_sha256(
                &confirmations_path,
            )
            .expect("confirmation hash"),
            cursor_file_sha256: historical_rejudge_artifact_file_sha256(&cursor_path)
                .expect("cursor hash"),
            proposal_count: source_snapshot_count,
            confirmation_count: source_snapshot_count,
            updated_at: "2026-07-21T00:00:00Z".to_string(),
        };
        write_historical_rejudge_artifact_manifest(
            &artifact_dir.join(HISTORICAL_REJUDGE_ARTIFACT_MANIFEST_FILE),
            &manifest,
        )
        .expect("write fixture manifest");
        manifest
    }

    fn write_test_historical_rejudge_apply_receipt(
        db: &Database,
        config: &Config,
        confirmations_path: &Path,
        receipt_path: &Path,
        hard_delete: bool,
    ) -> HistoricalRejudgeApplyReport {
        let report = plan_historical_rejudge_apply(
            db,
            config,
            confirmations_path,
            hard_delete,
        )
        .expect("plan fixture apply receipt")
        .report;
        fs::write(
            receipt_path,
            serde_json::to_vec_pretty(&report).expect("serialize fixture apply receipt"),
        )
        .expect("write fixture apply receipt");
        report
    }

    #[test]
    fn historical_rejudge_apply_fixture_reconciles_typed_occ_counts() {
        let cases: Vec<HistoricalRejudgeApplyCase> = serde_json::from_str(include_str!(
            "../tests/fixtures/historical_rejudge_apply_cases.json"
        ))
        .expect("parse apply cases fixture");
        let temp = tempfile::tempdir().expect("tempdir");
        let artifact_dir = temp.path().join("artifacts");
        fs::create_dir(&artifact_dir).expect("artifact dir");
        let proposals_path = artifact_dir.join("proposals.jsonl");
        let confirmations_path = artifact_dir.join("confirmations.jsonl");
        let cursor_path = artifact_dir.join("cursor.json");
        let db = Database::open(&temp.path().join("palace.db")).expect("open db");
        let config = Config::default();
        let mut max_rowid = 0;

        for case in &cases {
            let content = format!("private fixture content for {}", case.drawer_id);
            let drawer = Drawer {
                id: case.drawer_id.clone(),
                content,
                wing: "fixture-wing".to_string(),
                room: Some("Read".to_string()),
                source_file: Some(format!("{}.json", case.drawer_id)),
                source_type: SourceType::AgentInference,
                added_at: "2026-01-01T00:00:00Z".to_string(),
                ..Drawer::default()
            };
            db.insert_drawer(&drawer).expect("insert fixture drawer");
            let rowid = drawer_rowid(&db, &case.drawer_id);
            max_rowid = max_rowid.max(rowid);
            let row = db
                .historical_rejudge_candidate_by_rowid(rowid, &case.drawer_id)
                .expect("load candidate")
                .expect("candidate exists");
            let should_forget = case.decision == "delete";
            let proposal = historical_rejudge_proposal_artifact_line(
                &row,
                &HistoricalRejudgeArtifactProposalDecision {
                    score: if should_forget { 0.2 } else { 0.9 },
                    reason: "fixture decision".to_string(),
                    should_forget,
                },
            );
            append_jsonl_record(&proposals_path, &proposal).expect("append proposal");
            let decision = HistoricalRejudgeDecision {
                delete_candidate: should_forget,
                protected: false,
                reason: "fixture confirmation".to_string(),
                label: Some("fixture".to_string()),
                score: Some(if should_forget { 0.1 } else { 0.9 }),
                tier: 3,
                judge: "fixture".to_string(),
                requires_confirmation: false,
            };
            let mut confirmation =
                historical_rejudge_confirmation_artifact_line(&row, &proposal, &decision);
            if case.drift == "incomplete_snapshot" {
                confirmation.snapshot_status = None;
            }
            append_jsonl_record(&confirmations_path, &confirmation)
                .expect("append confirmation");

            match case.drift.as_str() {
                "none" | "incomplete_snapshot" => {}
                "missing_current" => {
                    db.conn()
                        .execute("DELETE FROM drawers WHERE id = ?1", [&case.drawer_id])
                        .expect("delete current row");
                }
                "content_hash" => {
                    let changed = "changed private fixture content";
                    db.conn()
                        .execute(
                            "UPDATE drawers SET content = ?1, content_hash = ?2 WHERE id = ?3",
                            rusqlite::params![
                                changed,
                                historical_rejudge_content_hash(changed),
                                case.drawer_id.as_str(),
                            ],
                        )
                        .expect("change content");
                }
                "retention" => {
                    db.conn()
                        .execute(
                            "UPDATE drawers SET importance = 5, is_pinned = 1 WHERE id = ?1",
                            [&case.drawer_id],
                        )
                        .expect("change retention");
                }
                other => panic!("unexpected drift fixture: {other}"),
            }
        }
        write_historical_rejudge_artifact_cursor(
            &cursor_path,
            &HistoricalRejudgeArtifactCursor {
                last_processed_rowid: max_rowid,
                options_hash: sha256_hex(b"fixture-artifact-options"),
                judge_model: "fixture".to_string(),
                started_at: "2026-07-21T00:00:00Z".to_string(),
                updated_at: "2026-07-21T00:00:01Z".to_string(),
            },
        )
        .expect("write fixture cursor");
        let manifest = write_test_historical_rejudge_manifest(
            &config,
            &artifact_dir,
            cases.len(),
            max_rowid,
        );

        let plan = plan_historical_rejudge_apply(
            &db,
            &config,
            &confirmations_path,
            false,
        )
        .expect("plan fixture apply");
        let report = plan.report;

        assert_eq!(report.confirmation_count, 6);
        assert_eq!(report.keep_count, 1);
        assert_eq!(report.delete_count, 5);
        assert_eq!(report.matched_count, 1);
        assert_eq!(report.stale_count, 4);
        assert_eq!(report.occ_missing_current_count, 1);
        assert_eq!(report.occ_content_hash_mismatch_count, 1);
        assert_eq!(report.occ_retention_snapshot_mismatch_count, 1);
        assert_eq!(report.occ_other_skip_count, 1);
        assert_eq!(report.skipped_count, 4);
        assert_eq!(report.mutated_count, 0);
        assert_eq!(report.generation_id.as_deref(), Some(manifest.generation_id.as_str()));
        assert_eq!(
            report.confirmations_file_sha256,
            manifest.confirmations_file_sha256
        );
        assert!(!report.hard_delete, "soft delete must remain the default");
        let encoded = serde_json::to_string(&report).expect("serialize receipt");
        assert!(!encoded.contains("private fixture content"));
        assert!(!encoded.contains("fixture-valid"));
        assert!(!encoded.contains(confirmations_path.to_string_lossy().as_ref()));
    }

    #[test]
    fn historical_rejudge_apply_execute_requires_receipt_before_side_effects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&temp.path().join("palace.db")).expect("open db");
        let drawer = Drawer {
            id: "receipt-required".to_string(),
            content: "private receipt fixture".to_string(),
            wing: "fixture-wing".to_string(),
            source_type: SourceType::AgentInference,
            added_at: "2026-01-01T00:00:00Z".to_string(),
            ..Drawer::default()
        };
        db.insert_drawer(&drawer).expect("insert fixture drawer");
        let confirmations_path = temp.path().join("confirmations.jsonl");
        append_test_delete_confirmation(
            &db,
            &confirmations_path,
            drawer_rowid(&db, &drawer.id),
            &drawer.id,
        );
        let backup_dir = temp.path().join("backups");

        let error = maintenance_rejudge_apply_command(
            &db,
            &Config::default(),
            &confirmations_path,
            None,
            &backup_dir,
            HistoricalRejudgeApplyMode {
                execute: true,
                hard_delete: false,
            },
            "json",
        )
        .expect_err("execute without a receipt must fail closed");

        assert!(error.to_string().contains("--receipt-file"));
        assert!(
            db.get_drawer(&drawer.id)
                .expect("load drawer")
                .is_some(),
            "missing receipt must not mutate the drawer"
        );
        assert!(
            fs::read_dir(&backup_dir).is_err(),
            "missing receipt must fail before backup creation"
        );
    }

    #[test]
    fn historical_rejudge_apply_rejects_malformed_receipt_before_side_effects() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&temp.path().join("palace.db")).expect("open db");
        let drawer = Drawer {
            id: "malformed-receipt".to_string(),
            content: "private malformed receipt fixture".to_string(),
            wing: "fixture-wing".to_string(),
            source_type: SourceType::AgentInference,
            added_at: "2026-01-01T00:00:00Z".to_string(),
            ..Drawer::default()
        };
        db.insert_drawer(&drawer).expect("insert fixture drawer");
        let confirmations_path = temp.path().join("confirmations.jsonl");
        append_test_delete_confirmation(
            &db,
            &confirmations_path,
            drawer_rowid(&db, &drawer.id),
            &drawer.id,
        );
        let receipt_path = temp.path().join("receipt.json");
        fs::write(&receipt_path, b"{\"dry_run\":").expect("write malformed receipt");
        let backup_dir = temp.path().join("backups");

        let error = maintenance_rejudge_apply_command(
            &db,
            &Config::default(),
            &confirmations_path,
            Some(&receipt_path),
            &backup_dir,
            HistoricalRejudgeApplyMode {
                execute: true,
                hard_delete: false,
            },
            "json",
        )
        .expect_err("malformed receipt must fail closed");

        assert!(error.to_string().contains("receipt"));
        assert!(
            db.get_drawer(&drawer.id)
                .expect("load drawer")
                .is_some(),
            "malformed receipt must not mutate the drawer"
        );
        assert!(
            fs::read_dir(&backup_dir).is_err(),
            "malformed receipt must fail before backup creation"
        );
    }

    #[test]
    fn historical_rejudge_apply_execute_accepts_unchanged_receipt_and_backs_up() {
        let temp = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&temp.path().join("palace.db")).expect("open db");
        let config = Config::default();
        let drawer = Drawer {
            id: "unchanged-receipt".to_string(),
            content: "private unchanged receipt fixture".to_string(),
            wing: "fixture-wing".to_string(),
            source_type: SourceType::AgentInference,
            added_at: "2026-01-01T00:00:00Z".to_string(),
            ..Drawer::default()
        };
        db.insert_drawer(&drawer).expect("insert fixture drawer");
        let confirmations_path = temp.path().join("confirmations.jsonl");
        append_test_delete_confirmation(
            &db,
            &confirmations_path,
            drawer_rowid(&db, &drawer.id),
            &drawer.id,
        );
        let receipt_path = temp.path().join("receipt.json");
        let dry_run = write_test_historical_rejudge_apply_receipt(
            &db,
            &config,
            &confirmations_path,
            &receipt_path,
            false,
        );
        assert_eq!(dry_run.matched_count, 1);
        let backup_dir = temp.path().join("backups");

        maintenance_rejudge_apply_command(
            &db,
            &config,
            &confirmations_path,
            Some(&receipt_path),
            &backup_dir,
            HistoricalRejudgeApplyMode {
                execute: true,
                hard_delete: false,
            },
            "json",
        )
        .expect("execute unchanged receipt");

        assert!(
            db.get_drawer(&drawer.id)
                .expect("load drawer")
                .is_none(),
            "unchanged matched drawer must be soft deleted"
        );
        assert_eq!(db.deleted_drawer_count().expect("deleted count"), 1);
        assert_eq!(
            fs::read_dir(&backup_dir)
                .expect("backup directory")
                .count(),
            1,
            "execute must write exactly one backup"
        );
        let audit_log =
            fs::read_to_string(temp.path().join("audit.jsonl")).expect("read audit log");
        assert!(audit_log.contains("\"command\":\"maintenance-rejudge-apply\""));
        assert!(!audit_log.contains(&drawer.content));
        assert!(!audit_log.contains(&drawer.id));
        assert!(!audit_log.contains(confirmations_path.to_string_lossy().as_ref()));
    }
