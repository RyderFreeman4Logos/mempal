// Receipt-bound execution and content-free reporting for historical rejudge apply.

fn read_historical_rejudge_apply_receipt(
    receipt_file: &Path,
) -> Result<(HistoricalRejudgeApplyReport, String)> {
    validate_absolute_path(receipt_file, "--receipt-file")?;
    let bytes =
        fs::read(receipt_file).context("failed to read historical rejudge apply receipt")?;
    let receipt = serde_json::from_slice::<HistoricalRejudgeApplyReport>(&bytes)
        .context("failed to parse historical rejudge apply receipt")?;
    if receipt.receipt_schema_version != HISTORICAL_REJUDGE_APPLY_RECEIPT_SCHEMA_VERSION {
        bail!("unsupported historical rejudge apply receipt schema_version");
    }
    if !receipt.dry_run
        || receipt.backup_path.is_some()
        || receipt.backup_format.is_some()
        || receipt.mutated_count != 0
    {
        bail!("historical rejudge apply receipt is not an unmodified dry-run receipt");
    }
    if receipt.generation_id.as_deref().is_none_or(str::is_empty)
        || receipt.config_hash.is_empty()
        || receipt.manifest_file_sha256.is_empty()
        || receipt.artifact_options_hash.is_empty()
        || receipt.proposals_file_sha256.is_empty()
        || receipt.confirmations_file_sha256.is_empty()
        || receipt.cursor_file_sha256.is_empty()
        || receipt.policy_fingerprint.is_empty()
        || receipt.options_hash.is_empty()
        || receipt.proposal_endpoint_model_fingerprint.is_empty()
        || receipt.confirmation_endpoint_model_fingerprint.is_empty()
        || receipt.endpoint_model_fingerprint.is_empty()
    {
        bail!("historical rejudge apply receipt is missing provenance");
    }
    let decision_count = receipt
        .keep_count
        .checked_add(receipt.delete_count)
        .context("historical rejudge apply receipt decision counts overflow")?;
    let occ_skip_count = receipt
        .occ_missing_current_count
        .checked_add(receipt.occ_content_hash_mismatch_count)
        .and_then(|count| count.checked_add(receipt.occ_retention_snapshot_mismatch_count))
        .and_then(|count| count.checked_add(receipt.occ_other_skip_count))
        .context("historical rejudge apply receipt OCC counts overflow")?;
    let delete_reconciliation = receipt
        .matched_count
        .checked_add(receipt.stale_count)
        .context("historical rejudge apply receipt delete counts overflow")?;
    if decision_count != receipt.confirmation_count
        || occ_skip_count != receipt.stale_count
        || receipt.skipped_count != receipt.stale_count
        || delete_reconciliation != receipt.delete_count
    {
        bail!("historical rejudge apply receipt counts do not reconcile");
    }
    Ok((receipt, sha256_hex(&bytes)))
}

fn validate_historical_rejudge_apply_receipt_binding(
    receipt: &HistoricalRejudgeApplyReport,
    planned: &HistoricalRejudgeApplyReport,
) -> Result<()> {
    fn ensure_matches(matches: bool, field: &'static str) -> Result<()> {
        if !matches {
            bail!("historical rejudge apply receipt binding mismatch: {field}");
        }
        Ok(())
    }

    ensure_matches(
        receipt.receipt_schema_version == planned.receipt_schema_version,
        "receipt_schema_version",
    )?;
    ensure_matches(
        receipt.schema_version == planned.schema_version,
        "artifact_schema_version",
    )?;
    ensure_matches(
        receipt.generation_id == planned.generation_id,
        "artifact_generation_id",
    )?;
    ensure_matches(
        receipt.manifest_file_sha256 == planned.manifest_file_sha256,
        "manifest_file_sha256",
    )?;
    ensure_matches(receipt.config_hash == planned.config_hash, "config_hash")?;
    ensure_matches(
        receipt.artifact_options_hash == planned.artifact_options_hash,
        "artifact_options_hash",
    )?;
    ensure_matches(
        receipt.proposals_file_sha256 == planned.proposals_file_sha256,
        "proposals_file_sha256",
    )?;
    ensure_matches(
        receipt.confirmations_file_sha256 == planned.confirmations_file_sha256,
        "confirmations_file_sha256",
    )?;
    ensure_matches(
        receipt.cursor_file_sha256 == planned.cursor_file_sha256,
        "cursor_file_sha256",
    )?;
    ensure_matches(
        receipt.policy_fingerprint == planned.policy_fingerprint,
        "policy_fingerprint",
    )?;
    ensure_matches(receipt.options_hash == planned.options_hash, "options_hash")?;
    ensure_matches(
        receipt.proposal_endpoint_model_fingerprint == planned.proposal_endpoint_model_fingerprint,
        "proposal_endpoint_model_fingerprint",
    )?;
    ensure_matches(
        receipt.confirmation_endpoint_model_fingerprint
            == planned.confirmation_endpoint_model_fingerprint,
        "confirmation_endpoint_model_fingerprint",
    )?;
    ensure_matches(
        receipt.endpoint_model_fingerprint == planned.endpoint_model_fingerprint,
        "endpoint_model_fingerprint",
    )?;
    ensure_matches(receipt.hard_delete == planned.hard_delete, "hard_delete")?;
    ensure_matches(receipt.db_fence == planned.db_fence, "db_fence")
}

fn validate_historical_rejudge_apply_lease_fence(
    receipt: &HistoricalRejudgeApplyReport,
    db: &Database,
    writer_lease: &MaintenanceWriterLeaseGuard,
) -> Result<()> {
    let lease = writer_lease.lease();
    let mut expected = receipt.db_fence.clone();
    let generation = match lease.name.as_str() {
        SQLITE_WRITER_LEASE_NAME => &mut expected.sqlite_writer_generation,
        HISTORICAL_REJUDGE_WRITER_LEASE_NAME => &mut expected.historical_rejudge_writer_generation,
        _ => bail!("historical rejudge apply acquired an unexpected writer lease"),
    };
    *generation = generation
        .checked_add(1)
        .context("historical rejudge apply receipt DB generation overflow")?;
    if lease.generation != *generation || historical_rejudge_db_fence(db)? != expected {
        bail!("historical rejudge apply receipt binding mismatch: db_fence");
    }
    Ok(())
}

fn reconcile_historical_rejudge_apply_execution(
    report: &mut HistoricalRejudgeApplyReport,
    execute: bool,
    mutated_count: usize,
) -> Result<()> {
    if !execute && mutated_count != 0 {
        bail!("historical rejudge dry run cannot report mutations");
    }
    let late_occ_skip_count = if execute {
        report
            .matched_count
            .checked_sub(mutated_count)
            .context("historical rejudge mutated count exceeds the matched cohort")?
    } else {
        0
    };
    report.matched_count -= late_occ_skip_count;
    report.occ_other_skip_count += late_occ_skip_count;
    report.stale_count += late_occ_skip_count;
    report.skipped_count += late_occ_skip_count;
    report.mutated_count = mutated_count;
    report.dry_run = !execute;
    Ok(())
}

#[derive(Clone, Copy, Debug)]
struct HistoricalRejudgeApplyMode {
    execute: bool,
    hard_delete: bool,
}

fn maintenance_rejudge_apply_command(
    db: &Database,
    config: &Config,
    confirmations_file: &Path,
    receipt_file: Option<&Path>,
    backup_dir: &Path,
    mode: HistoricalRejudgeApplyMode,
    format: &str,
) -> Result<()> {
    let HistoricalRejudgeApplyMode {
        execute,
        hard_delete,
    } = mode;
    validate_absolute_path(backup_dir, "--backup-dir")?;
    if execute && receipt_file.is_none() {
        bail!("--receipt-file is required with --execute");
    }
    if !execute && receipt_file.is_some() {
        bail!("--receipt-file requires --execute");
    }
    let accepted_receipt = receipt_file
        .map(read_historical_rejudge_apply_receipt)
        .transpose()?;
    let mut plan = plan_historical_rejudge_apply(db, config, confirmations_file, hard_delete)?;
    if let Some((receipt, _)) = &accepted_receipt {
        validate_historical_rejudge_apply_receipt_binding(receipt, &plan.report)?;
    }
    let writer_lease = if execute {
        Some(acquire_historical_rejudge_writer_lease(db)?)
    } else {
        None
    };
    let writer_lease = writer_lease.as_ref();
    if let (Some((receipt, _)), Some(writer_lease)) = (&accepted_receipt, writer_lease) {
        validate_historical_rejudge_apply_lease_fence(receipt, db, writer_lease)?;
    }

    let backup_path = if execute && !plan.backup_items.is_empty() {
        let options = HistoricalRejudgeOptions {
            execute,
            hard_delete,
            backup_dir: Some(backup_dir),
            unsafe_no_backup: false,
            limit: plan.report.delete_count,
            all: true,
            resume: false,
            unsafe_allow_config_version_drift: false,
            page_size: plan.report.delete_count.max(1),
            progress_file: None,
            candidates_file: None,
            stage_mode: HistoricalRejudgeStageMode::Paired,
            proposal_llm_endpoint: None,
            confirm_llm_endpoint: None,
            wing: None,
            room: None,
            project: None,
            format,
        };
        let path = create_historical_rejudge_sqlite_backup(
            db,
            backup_dir,
            options,
            Some("artifact-confirmations".to_string()),
            "artifact".to_string(),
        )
        .context("failed to create historical rejudge apply backup")?;
        append_historical_rejudge_backup_items_durable(&path, &plan.backup_items)?;
        Some(path)
    } else {
        None
    };

    let mutated_count = if execute {
        with_historical_rejudge_transaction_with_writer_lease(
            db,
            writer_lease,
            "apply historical rejudge artifact confirmations",
            |db| delete_rejudge_backup_items_by_version_inner(db, &plan.backup_items, hard_delete),
        )?
    } else {
        0
    };
    reconcile_historical_rejudge_apply_execution(&mut plan.report, execute, mutated_count)?;
    plan.report.backup_path = backup_path;
    plan.report.backup_format = plan
        .report
        .backup_path
        .as_ref()
        .map(|_| "sqlite".to_string());

    if execute {
        append_audit_entry_with_writer_lease(
            db,
            writer_lease,
            "append historical rejudge artifact apply audit",
            "maintenance-rejudge-apply",
            &serde_json::json!({
                "schema_version": plan.report.schema_version,
                "generation_id": plan.report.generation_id,
                "manifest_file_sha256": plan.report.manifest_file_sha256,
                "receipt_file_sha256": accepted_receipt.as_ref().map(|(_, hash)| hash),
                "config_hash": plan.report.config_hash,
                "artifact_options_hash": plan.report.artifact_options_hash,
                "proposals_file_sha256": plan.report.proposals_file_sha256,
                "confirmations_file_sha256": plan.report.confirmations_file_sha256,
                "cursor_file_sha256": plan.report.cursor_file_sha256,
                "policy_fingerprint": plan.report.policy_fingerprint,
                "options_hash": plan.report.options_hash,
                "proposal_endpoint_model_fingerprint": plan.report.proposal_endpoint_model_fingerprint,
                "confirmation_endpoint_model_fingerprint": plan.report.confirmation_endpoint_model_fingerprint,
                "endpoint_model_fingerprint": plan.report.endpoint_model_fingerprint,
                "db_fence": plan.report.db_fence,
                "hard_delete": hard_delete,
                "confirmation_count": plan.report.confirmation_count,
                "keep_count": plan.report.keep_count,
                "delete_count": plan.report.delete_count,
                "matched_count": plan.report.matched_count,
                "stale_count": plan.report.stale_count,
                "occ_missing_current_count": plan.report.occ_missing_current_count,
                "occ_content_hash_mismatch_count": plan.report.occ_content_hash_mismatch_count,
                "occ_retention_snapshot_mismatch_count": plan.report.occ_retention_snapshot_mismatch_count,
                "occ_other_skip_count": plan.report.occ_other_skip_count,
                "skipped_count": plan.report.skipped_count,
                "mutated_count": mutated_count,
            }),
        )
        .context("failed to append historical rejudge artifact apply audit log")?;
    }

    print_historical_rejudge_apply_report(&plan.report, format)
}

fn historical_rejudge_db_fence(db: &Database) -> Result<HistoricalRejudgeDbFence> {
    fn writer_generation(db: &Database, lease_name: &str) -> Result<u64> {
        use rusqlite::OptionalExtension;

        let generation = db
            .conn()
            .query_row(
                "SELECT last_generation FROM runtime_writer_lease_generations WHERE name = ?1",
                [lease_name],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .with_context(|| format!("failed to read writer generation for {lease_name}"))?
            .unwrap_or_default();
        u64::try_from(generation)
            .with_context(|| format!("invalid writer generation for {lease_name}: {generation}"))
    }

    Ok(HistoricalRejudgeDbFence {
        schema_version: db
            .schema_version()
            .context("failed to read database schema fence")?,
        sqlite_writer_generation: writer_generation(db, SQLITE_WRITER_LEASE_NAME)?,
        historical_rejudge_writer_generation: writer_generation(
            db,
            HISTORICAL_REJUDGE_WRITER_LEASE_NAME,
        )?,
    })
}

fn current_historical_rejudge_drawer_content_hash(
    db: &Database,
    drawer_rowid: i64,
    drawer_id: &str,
) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;

    db.conn()
        .query_row(
            r#"
            SELECT COALESCE(content_hash, '')
            FROM drawers
            WHERE deleted_at IS NULL
              AND rowid = ?1
              AND id = ?2
            "#,
            rusqlite::params![drawer_rowid, drawer_id],
            |row| row.get(0),
        )
        .optional()
        .context("failed to query current drawer content hash")
}

fn print_historical_rejudge_apply_report(
    report: &HistoricalRejudgeApplyReport,
    format: &str,
) -> Result<()> {
    match format {
        "json" => {
            println!("{}", serde_json::to_string_pretty(report)?);
            Ok(())
        }
        "plain" => {
            println!("Historical Memory Rejudge Apply");
            println!("dry_run={}", report.dry_run);
            println!(
                "confirmations_file_sha256={}",
                report.confirmations_file_sha256
            );
            println!("schema_version={}", report.schema_version);
            if let Some(generation_id) = &report.generation_id {
                println!("generation_id={generation_id}");
            }
            println!("manifest_file_sha256={}", report.manifest_file_sha256);
            println!("hard_delete={}", report.hard_delete);
            println!("confirmations={}", report.confirmation_count);
            println!("keep={}", report.keep_count);
            println!("delete_candidates={}", report.delete_count);
            println!("matched={}", report.matched_count);
            println!("stale={}", report.stale_count);
            println!("occ_missing_current={}", report.occ_missing_current_count);
            println!(
                "occ_content_hash_mismatch={}",
                report.occ_content_hash_mismatch_count
            );
            println!(
                "occ_retention_snapshot_mismatch={}",
                report.occ_retention_snapshot_mismatch_count
            );
            println!("occ_other_skip={}", report.occ_other_skip_count);
            println!("skipped={}", report.skipped_count);
            println!("mutated={}", report.mutated_count);
            if let Some(path) = &report.backup_path {
                println!("backup={}", path.display());
            }
            if report.dry_run && report.delete_count > 0 {
                println!(
                    "save a --format json dry-run report, then rerun with --execute --receipt-file <absolute-path>"
                );
            }
            Ok(())
        }
        other => bail!("unsupported maintenance rejudge apply format: {other}"),
    }
}
