// Historical rejudge artifact-manifest regression tests.

    #[test]
    fn historical_rejudge_artifact_manifest_is_machine_readable_and_generation_bound() {
        let temp = tempfile::tempdir().expect("tempdir");
        let manifest_path = temp.path().join(HISTORICAL_REJUDGE_ARTIFACT_MANIFEST_FILE);
        let manifest = HistoricalRejudgeArtifactManifest {
            schema_version: HISTORICAL_REJUDGE_ARTIFACT_SCHEMA_VERSION.to_string(),
            generation_id: "historical-rejudge-generation-fixture".to_string(),
            source_snapshot_count: 7,
            source_snapshot_max_rowid: 41,
            proposal_stage_status: HistoricalRejudgeArtifactStageStatus::Completed,
            confirmation_stage_status: HistoricalRejudgeArtifactStageStatus::Partial,
            config_hash:
                "1111111111111111111111111111111111111111111111111111111111111111"
                    .to_string(),
            options_hash:
                "2222222222222222222222222222222222222222222222222222222222222222"
                    .to_string(),
            proposal_endpoint_model_fingerprint:
                "3333333333333333333333333333333333333333333333333333333333333333"
                    .to_string(),
            confirmation_endpoint_model_fingerprint:
                "4444444444444444444444444444444444444444444444444444444444444444"
                    .to_string(),
            policy_fingerprint: "policy-fixture".to_string(),
            proposals_file_sha256:
                "5555555555555555555555555555555555555555555555555555555555555555"
                    .to_string(),
            confirmations_file_sha256:
                "6666666666666666666666666666666666666666666666666666666666666666"
                    .to_string(),
            cursor_file_sha256:
                "7777777777777777777777777777777777777777777777777777777777777777"
                    .to_string(),
            proposal_count: 7,
            confirmation_count: 3,
            updated_at: "2026-07-21T00:00:00Z".to_string(),
        };

        write_historical_rejudge_artifact_manifest(&manifest_path, &manifest)
            .expect("write manifest");
        let loaded = read_historical_rejudge_artifact_manifest(&manifest_path)
            .expect("read manifest");

        assert_eq!(loaded, manifest);
        let encoded = fs::read_to_string(&manifest_path).expect("manifest text");
        assert!(encoded.contains("\"proposal_stage_status\": \"completed\""));
        assert!(encoded.contains("\"confirmation_stage_status\": \"partial\""));
        assert!(!encoded.contains("drawer text"));
        assert!(!encoded.contains("https://"));
    }

    #[test]
    fn historical_rejudge_partial_confirmation_is_usable_but_incomplete_proposal_cannot_complete()
    {
        let temp = tempfile::tempdir().expect("tempdir");
        let db = Database::open(&temp.path().join("palace.db")).expect("open db");
        let config = Config::default();
        let drawer = Drawer {
            id: "partial-artifact".to_string(),
            content: "private partial artifact fixture".to_string(),
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
        let manifest_path = temp.path().join(HISTORICAL_REJUDGE_ARTIFACT_MANIFEST_FILE);
        let mut manifest = read_historical_rejudge_artifact_manifest(&manifest_path)
            .expect("read fixture manifest");
        manifest.source_snapshot_count = 2;
        manifest.proposal_stage_status = HistoricalRejudgeArtifactStageStatus::Partial;
        manifest.confirmation_stage_status = HistoricalRejudgeArtifactStageStatus::Partial;
        write_historical_rejudge_artifact_manifest(&manifest_path, &manifest)
            .expect("write partial manifest");

        let plan = plan_historical_rejudge_apply(&db, &config, &confirmations_path, false)
            .expect("complete rows from a partial split-stage generation remain usable");
        assert_eq!(plan.report.matched_count, 1);

        let error = refresh_historical_rejudge_artifact_manifest(
            &manifest_path,
            &mut manifest,
            HistoricalRejudgeArtifactManifestRefresh {
                proposals_path: &temp.path().join("proposals.jsonl"),
                confirmations_path: &confirmations_path,
                cursor_path: &temp.path().join("cursor.json"),
                proposal_stage_status: HistoricalRejudgeArtifactStageStatus::Partial,
                confirmation_stage_status: HistoricalRejudgeArtifactStageStatus::Completed,
                proposal_count: 0,
                confirmation_count: 1,
            },
        )
        .expect_err("incomplete proposal generation must not be marked completed");
        assert!(error.to_string().contains("complete"));
    }

    #[test]
    fn historical_rejudge_incomplete_generation_stage_status_stays_partial() {
        assert_eq!(
            historical_rejudge_proposal_stage_status(41, 41, 6, 7),
            HistoricalRejudgeArtifactStageStatus::Partial
        );
        assert_eq!(
            historical_rejudge_confirmation_stage_status(
                HistoricalRejudgeArtifactStageStatus::Partial,
                0,
                0,
                false,
            ),
            HistoricalRejudgeArtifactStageStatus::Partial
        );
        assert_eq!(
            historical_rejudge_confirmation_stage_status(
                HistoricalRejudgeArtifactStageStatus::Completed,
                3,
                3,
                false,
            ),
            HistoricalRejudgeArtifactStageStatus::Completed
        );
    }
