// Historical rejudge apply global fail-closed regression tests.

    struct HistoricalRejudgeFailClosedFixture {
        db: Database,
        temp: tempfile::TempDir,
        config: Config,
        drawer_id: String,
        confirmations_path: PathBuf,
        receipt_path: PathBuf,
        backup_dir: PathBuf,
    }

    impl HistoricalRejudgeFailClosedFixture {
        fn new(suffix: &str) -> Self {
            let temp = tempfile::tempdir().expect("tempdir");
            let db = Database::open(&temp.path().join("palace.db")).expect("open db");
            let config = Config::default();
            let drawer_id = format!("fail-closed-{suffix}");
            let drawer = Drawer {
                id: drawer_id.clone(),
                content: format!("private fail-closed fixture {suffix}"),
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
                drawer_rowid(&db, &drawer_id),
                &drawer_id,
            );
            let receipt_path = temp.path().join("receipt.json");
            let receipt = write_test_historical_rejudge_apply_receipt(
                &db,
                &config,
                &confirmations_path,
                &receipt_path,
                false,
            );
            assert_eq!(
                receipt.matched_count, 1,
                "fail-closed fixture must begin with an actionable delete"
            );
            let backup_dir = temp.path().join("backups");
            Self {
                db,
                temp,
                config,
                drawer_id,
                confirmations_path,
                receipt_path,
                backup_dir,
            }
        }

        fn manifest_path(&self) -> PathBuf {
            self.temp
                .path()
                .join(HISTORICAL_REJUDGE_ARTIFACT_MANIFEST_FILE)
        }

        fn execute_error(&self, hard_delete: bool) -> anyhow::Error {
            maintenance_rejudge_apply_command(
                &self.db,
                &self.config,
                &self.confirmations_path,
                Some(&self.receipt_path),
                &self.backup_dir,
                HistoricalRejudgeApplyMode {
                    execute: true,
                    hard_delete,
                },
                "json",
            )
            .expect_err("drifted apply must fail closed")
        }

        fn assert_no_side_effects(&self) {
            assert!(
                self.db
                    .get_drawer(&self.drawer_id)
                    .expect("load drawer")
                    .is_some(),
                "fail-closed apply must not mutate the drawer"
            );
            assert!(
                fs::read_dir(&self.backup_dir).is_err(),
                "fail-closed apply must not create a backup"
            );
        }
    }

    fn mutate_test_json(path: &Path, mutation: impl FnOnce(&mut Value)) {
        let mut value: Value =
            serde_json::from_slice(&fs::read(path).expect("read JSON fixture"))
                .expect("parse JSON fixture");
        mutation(&mut value);
        fs::write(
            path,
            serde_json::to_vec_pretty(&value).expect("serialize JSON fixture"),
        )
        .expect("write JSON fixture");
    }

    #[test]
    fn historical_rejudge_apply_fails_closed_when_confirmation_bytes_change() {
        let fixture = HistoricalRejudgeFailClosedFixture::new("confirmation-bytes");
        let mut confirmation: Value = serde_json::from_slice(
            &fs::read(&fixture.confirmations_path).expect("read confirmation"),
        )
        .expect("parse confirmation");
        confirmation["timestamp"] = Value::String("changed-after-dry-run".to_string());
        let mut bytes = serde_json::to_vec(&confirmation).expect("serialize confirmation");
        bytes.push(b'\n');
        fs::write(&fixture.confirmations_path, bytes).expect("change confirmation bytes");

        let error = fixture.execute_error(false);

        assert!(error.to_string().contains("canonical bytes"));
        fixture.assert_no_side_effects();
    }

    #[test]
    fn historical_rejudge_apply_fails_closed_on_artifact_identity_drift() {
        for (suffix, field, replacement, expected) in [
            (
                "generation",
                "generation_id",
                "different-generation",
                "artifact_generation_id",
            ),
            (
                "schema",
                "schema_version",
                "mempal.historical_rejudge_artifact.v999",
                "unsupported historical rejudge artifact schema",
            ),
            (
                "policy",
                "policy_fingerprint",
                "different-policy",
                "policy_fingerprint",
            ),
            (
                "manifest-bytes",
                "updated_at",
                "2026-07-21T23:59:59Z",
                "manifest_file_sha256",
            ),
        ] {
            let fixture = HistoricalRejudgeFailClosedFixture::new(suffix);
            mutate_test_json(&fixture.manifest_path(), |manifest| {
                manifest[field] = Value::String(replacement.to_string());
            });

            let error = fixture.execute_error(false);

            assert!(
                error.to_string().contains(expected),
                "unexpected {suffix} error: {error:#}"
            );
            fixture.assert_no_side_effects();
        }
    }

    #[test]
    fn historical_rejudge_apply_fails_closed_on_db_generation_drift() {
        let fixture = HistoricalRejudgeFailClosedFixture::new("db-generation");
        let intervening_lease = acquire_historical_rejudge_writer_lease(&fixture.db)
            .expect("acquire intervening writer lease");
        drop(intervening_lease);

        let error = fixture.execute_error(false);

        assert!(error.to_string().contains("db_fence"));
        fixture.assert_no_side_effects();
    }

    #[test]
    fn historical_rejudge_apply_strictly_rejects_invalid_confirmation_jsonl() {
        for suffix in ["malformed", "truncated", "duplicate-json-field"] {
            let fixture = HistoricalRejudgeFailClosedFixture::new(suffix);
            let original = fs::read(&fixture.confirmations_path).expect("read confirmation");
            let invalid = match suffix {
                "malformed" => b"{not-json}\n".to_vec(),
                "truncated" => original[..original.len() - 1].to_vec(),
                "duplicate-json-field" => {
                    let line = std::str::from_utf8(&original).expect("UTF-8 confirmation");
                    format!("{{\"drawer_id\":\"duplicate\",{}", &line[1..])
                        .into_bytes()
                }
                _ => unreachable!(),
            };
            fs::write(&fixture.confirmations_path, invalid).expect("write invalid confirmation");

            let error = fixture.execute_error(false);

            assert!(
                error.to_string().contains("confirmations JSONL"),
                "unexpected {suffix} error: {error:#}"
            );
            fixture.assert_no_side_effects();
        }
    }

    #[test]
    fn historical_rejudge_apply_rejects_duplicate_logical_keys() {
        let fixture = HistoricalRejudgeFailClosedFixture::new("duplicate-logical-key");
        let proposals_path = fixture.temp.path().join("proposals.jsonl");
        let proposal = fs::read(&proposals_path).expect("read proposal");
        let confirmation = fs::read(&fixture.confirmations_path).expect("read confirmation");
        fs::write(&proposals_path, [proposal.as_slice(), proposal.as_slice()].concat())
            .expect("duplicate proposal");
        fs::write(
            &fixture.confirmations_path,
            [confirmation.as_slice(), confirmation.as_slice()].concat(),
        )
        .expect("duplicate confirmation");
        let mut manifest = read_historical_rejudge_artifact_manifest(&fixture.manifest_path())
            .expect("read manifest");
        manifest.source_snapshot_count = 2;
        manifest.proposal_count = 2;
        manifest.confirmation_count = 2;
        manifest.proposals_file_sha256 =
            historical_rejudge_artifact_file_sha256(&proposals_path).expect("proposal hash");
        manifest.confirmations_file_sha256 =
            historical_rejudge_artifact_file_sha256(&fixture.confirmations_path)
                .expect("confirmation hash");
        write_historical_rejudge_artifact_manifest(&fixture.manifest_path(), &manifest)
            .expect("write duplicate-key manifest");

        let error = fixture.execute_error(false);

        assert!(error.to_string().contains("duplicate keys"));
        fixture.assert_no_side_effects();
    }

    #[test]
    fn historical_rejudge_apply_rejects_missing_policy_and_unknown_receipt_schema() {
        let missing_policy = HistoricalRejudgeFailClosedFixture::new("missing-policy");
        mutate_test_json(&missing_policy.manifest_path(), |manifest| {
            manifest
                .as_object_mut()
                .expect("manifest object")
                .remove("policy_fingerprint");
        });
        let policy_error = missing_policy.execute_error(false);
        assert!(policy_error.to_string().contains("artifact manifest"));
        missing_policy.assert_no_side_effects();

        let unknown_receipt = HistoricalRejudgeFailClosedFixture::new("receipt-schema");
        mutate_test_json(&unknown_receipt.receipt_path, |receipt| {
            receipt["receipt_schema_version"] = Value::String(
                "mempal.historical_rejudge_apply_receipt.v999".to_string(),
            );
        });
        let schema_error = unknown_receipt.execute_error(false);
        assert!(schema_error.to_string().contains("receipt schema_version"));
        unknown_receipt.assert_no_side_effects();
    }

    #[test]
    fn historical_rejudge_apply_hard_delete_requires_matching_opt_in_receipt() {
        let fixture = HistoricalRejudgeFailClosedFixture::new("hard-delete-opt-in");

        let error = fixture.execute_error(true);

        assert!(error.to_string().contains("binding mismatch"));
        fixture.assert_no_side_effects();
    }

    #[test]
    fn historical_rejudge_apply_hard_delete_can_be_receipted_by_dry_run() {
        let fixture = HistoricalRejudgeFailClosedFixture::new("hard-delete-dry-run");

        maintenance_rejudge_apply_command(
            &fixture.db,
            &fixture.config,
            &fixture.confirmations_path,
            None,
            &fixture.backup_dir,
            HistoricalRejudgeApplyMode {
                execute: false,
                hard_delete: true,
            },
            "json",
        )
        .expect("hard-delete mode must be previewable before execute");

        let mut report = plan_historical_rejudge_apply(
            &fixture.db,
            &fixture.config,
            &fixture.confirmations_path,
            true,
        )
        .expect("plan hard-delete receipt")
        .report;
        reconcile_historical_rejudge_apply_execution(&mut report, false, 0)
            .expect("reconcile dry-run counts");
        assert!(report.dry_run);
        assert!(report.hard_delete);
        assert_eq!(report.matched_count, 1);
        assert_eq!(report.stale_count, 0);
        assert_eq!(report.mutated_count, 0);
        fixture.assert_no_side_effects();
    }
