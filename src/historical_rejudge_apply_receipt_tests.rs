// Historical rejudge apply receipt and OCC regression tests.

    fn sample_historical_rejudge_apply_report() -> HistoricalRejudgeApplyReport {
        HistoricalRejudgeApplyReport {
            receipt_schema_version: HISTORICAL_REJUDGE_APPLY_RECEIPT_SCHEMA_VERSION.to_string(),
            dry_run: true,
            backup_path: None,
            backup_format: None,
            hard_delete: false,
            schema_version: HISTORICAL_REJUDGE_ARTIFACT_SCHEMA_VERSION.to_string(),
            generation_id: Some("generation-fixture".to_string()),
            manifest_file_sha256:
                "9999999999999999999999999999999999999999999999999999999999999999"
                    .to_string(),
            config_hash:
                "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd"
                    .to_string(),
            artifact_options_hash:
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
                    .to_string(),
            proposals_file_sha256:
                "ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
                    .to_string(),
            confirmations_file_sha256:
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
            cursor_file_sha256:
                "1111111111111111111111111111111111111111111111111111111111111111"
                    .to_string(),
            policy_fingerprint: "policy-fixture".to_string(),
            options_hash:
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
                    .to_string(),
            proposal_endpoint_model_fingerprint:
                "2222222222222222222222222222222222222222222222222222222222222222"
                    .to_string(),
            confirmation_endpoint_model_fingerprint:
                "3333333333333333333333333333333333333333333333333333333333333333"
                    .to_string(),
            endpoint_model_fingerprint:
                "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                    .to_string(),
            db_fence: HistoricalRejudgeDbFence {
                schema_version: 1,
                sqlite_writer_generation: 7,
                historical_rejudge_writer_generation: 3,
            },
            confirmation_count: 6,
            keep_count: 1,
            delete_count: 5,
            matched_count: 1,
            stale_count: 4,
            occ_missing_current_count: 1,
            occ_content_hash_mismatch_count: 1,
            occ_retention_snapshot_mismatch_count: 1,
            occ_other_skip_count: 1,
            skipped_count: 4,
            mutated_count: 0,
        }
    }

    #[test]
    fn historical_rejudge_apply_receipt_is_content_free_and_reconcilable() {
        let expected: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/historical_rejudge_apply_receipt.json"
        ))
        .expect("parse receipt fixture");
        let actual = serde_json::to_value(sample_historical_rejudge_apply_report())
            .expect("serialize apply receipt");

        for (key, expected_value) in expected.as_object().expect("fixture object") {
            assert_eq!(
                actual.get(key),
                Some(expected_value),
                "receipt field {key} must be mechanically reconcilable"
            );
        }
        assert!(
            actual.get("confirmations_file").is_none(),
            "receipt must not expose artifact paths"
        );
        assert_eq!(
            actual["confirmation_count"],
            actual["keep_count"].as_u64().expect("keep_count")
                + actual["delete_count"].as_u64().expect("delete_count")
        );
        assert_eq!(
            actual["stale_count"],
            actual["occ_missing_current_count"]
                .as_u64()
                .expect("missing count")
                + actual["occ_content_hash_mismatch_count"]
                    .as_u64()
                    .expect("content mismatch count")
                + actual["occ_retention_snapshot_mismatch_count"]
                    .as_u64()
                    .expect("retention mismatch count")
                + actual["occ_other_skip_count"]
                    .as_u64()
                    .expect("other skip count")
        );
        assert_eq!(actual["skipped_count"], actual["stale_count"]);
    }
