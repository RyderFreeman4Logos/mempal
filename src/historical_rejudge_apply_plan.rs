// Strict artifact parsing and typed OCC planning for historical rejudge apply.

fn read_historical_rejudge_apply_jsonl<T>(
    path: &Path,
    artifact_name: &'static str,
) -> Result<(Vec<T>, Vec<u8>)>
where
    T: for<'de> Deserialize<'de>,
{
    let bytes = fs::read(path)
        .with_context(|| format!("failed to read historical rejudge {artifact_name}"))?;
    if !bytes.is_empty() && !bytes.ends_with(b"\n") {
        bail!("historical rejudge {artifact_name} JSONL has an incomplete final line");
    }
    let text = std::str::from_utf8(&bytes)
        .with_context(|| format!("historical rejudge {artifact_name} JSONL is not UTF-8"))?;
    let mut records = Vec::new();
    for (index, line) in text.split_terminator('\n').enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            bail!(
                "historical rejudge {artifact_name} JSONL line {} is empty",
                index + 1
            );
        }
        let record = serde_json::from_str::<T>(trimmed).with_context(|| {
            format!(
                "failed to parse historical rejudge {artifact_name} JSONL line {}",
                index + 1
            )
        })?;
        records.push(record);
    }
    Ok((records, bytes))
}

fn plan_historical_rejudge_apply(
    db: &Database,
    config: &Config,
    confirmations_file: &Path,
    hard_delete: bool,
) -> Result<HistoricalRejudgeApplyPlan> {
    validate_absolute_path(confirmations_file, "--confirmations-file")?;
    let (confirmations, confirmations_bytes) = read_historical_rejudge_apply_jsonl::<
        HistoricalRejudgeConfirmationArtifactLine,
    >(confirmations_file, "confirmations")?;
    let confirmations_file_sha256 = sha256_hex(&confirmations_bytes);
    let artifact_dir = confirmations_file
        .parent()
        .context("historical rejudge confirmations file has no parent directory")?;
    let proposals_path = artifact_dir.join("proposals.jsonl");
    let cursor_path = artifact_dir.join("cursor.json");
    let (proposals, _) = read_historical_rejudge_apply_jsonl::<
        HistoricalRejudgeProposalArtifactLine,
    >(&proposals_path, "proposals")?;
    let (manifest, manifest_file_sha256) = read_historical_rejudge_artifact_manifest_with_sha256(
        &artifact_dir.join(HISTORICAL_REJUDGE_ARTIFACT_MANIFEST_FILE),
    )?;
    if manifest.proposal_stage_status == HistoricalRejudgeArtifactStageStatus::InProgress {
        bail!("historical rejudge artifact proposal stage is still in progress");
    }
    if manifest.confirmation_stage_status == HistoricalRejudgeArtifactStageStatus::InProgress {
        bail!("historical rejudge artifact confirmation stage is still in progress");
    }
    validate_historical_rejudge_artifact_manifest_hashes(
        &manifest,
        &proposals_path,
        confirmations_file,
        &cursor_path,
    )?;
    let artifact_cursor_rowid = if manifest.source_snapshot_count > 0 {
        let cursor = read_historical_rejudge_artifact_cursor(&cursor_path)?;
        if cursor.options_hash != manifest.options_hash {
            bail!("historical rejudge artifact cursor options do not match generation");
        }
        if cursor.last_processed_rowid > manifest.source_snapshot_max_rowid
            || (manifest.proposal_stage_status == HistoricalRejudgeArtifactStageStatus::Completed
                && cursor.last_processed_rowid < manifest.source_snapshot_max_rowid)
        {
            bail!("historical rejudge artifact cursor does not match generation boundary");
        }
        Some(cursor.last_processed_rowid)
    } else {
        None
    };
    if manifest.confirmations_file_sha256 != confirmations_file_sha256 {
        bail!("historical rejudge confirmation bytes do not match artifact manifest");
    }
    if manifest.confirmation_count != confirmations.len() {
        bail!("historical rejudge confirmation count does not match artifact manifest");
    }
    if manifest.proposal_count != proposals.len() {
        bail!("historical rejudge proposal count does not match artifact manifest");
    }
    let current_config_hash = historical_rejudge_artifact_config_hash(config)?;
    if manifest.config_hash != current_config_hash {
        bail!("historical rejudge artifact config_hash does not match current config");
    }
    let current_policy_fingerprint = historical_rejudge_judge_policy_fingerprint(config)?;
    if manifest.policy_fingerprint != current_policy_fingerprint {
        bail!("historical rejudge artifact policy_fingerprint does not match current policy");
    }
    let mut proposal_keys = BTreeSet::new();
    let mut proposal_drawer_ids = BTreeSet::new();
    let mut proposal_rowids = BTreeSet::new();
    for proposal in &proposals {
        if proposal.drawer_rowid > manifest.source_snapshot_max_rowid
            || artifact_cursor_rowid
                .is_some_and(|cursor_rowid| proposal.drawer_rowid > cursor_rowid)
        {
            bail!("historical rejudge proposal exceeds the generation cursor boundary");
        }
        if !proposal_keys.insert(historical_rejudge_artifact_key(
            proposal.drawer_rowid,
            &proposal.drawer_id,
        )) || !proposal_drawer_ids.insert(proposal.drawer_id.as_str())
            || !proposal_rowids.insert(proposal.drawer_rowid)
        {
            bail!("historical rejudge proposals contain conflicting duplicate keys");
        }
    }
    let mut artifact_keys = BTreeSet::new();
    let mut drawer_ids = BTreeSet::new();
    let mut drawer_rowids = BTreeSet::new();
    let mut keep_count = 0usize;
    let mut delete_count = 0usize;
    for confirmation in &confirmations {
        let artifact_key =
            historical_rejudge_artifact_key(confirmation.drawer_rowid, &confirmation.drawer_id);
        if !artifact_keys.insert(artifact_key.clone())
            || !drawer_ids.insert(confirmation.drawer_id.as_str())
            || !drawer_rowids.insert(confirmation.drawer_rowid)
        {
            bail!("historical rejudge confirmations contain conflicting duplicate keys");
        }
        if !proposal_keys.contains(&artifact_key) {
            bail!("historical rejudge confirmation has no canonical proposal");
        }
        match confirmation.final_decision.as_str() {
            "keep" => keep_count += 1,
            "delete" => delete_count += 1,
            other => bail!("unsupported historical rejudge final_decision: {other}"),
        }
    }
    let mutation = if hard_delete {
        "hard_delete"
    } else {
        "soft_delete"
    };
    let mut occ_missing_current_count = 0usize;
    let mut occ_content_hash_mismatch_count = 0usize;
    let mut occ_retention_snapshot_mismatch_count = 0usize;
    let mut occ_other_skip_count = 0usize;
    let mut backup_items = Vec::new();
    for confirmation in confirmations
        .iter()
        .filter(|line| line.final_decision == "delete")
    {
        let current_content_hash = current_historical_rejudge_drawer_content_hash(
            db,
            confirmation.drawer_rowid,
            &confirmation.drawer_id,
        )
        .with_context(|| {
            format!(
                "failed to load confirmed drawer {} content hash at rowid {}",
                confirmation.drawer_id, confirmation.drawer_rowid
            )
        })?;
        let Some(current_content_hash) = current_content_hash else {
            occ_missing_current_count += 1;
            continue;
        };
        if confirmation.snapshot_content_hash.is_empty()
            || current_content_hash != confirmation.snapshot_content_hash
        {
            occ_content_hash_mismatch_count += 1;
            continue;
        }
        let row = db
            .historical_rejudge_candidate_by_rowid(
                confirmation.drawer_rowid,
                &confirmation.drawer_id,
            )
            .with_context(|| {
                format!(
                    "failed to load confirmed drawer {} at rowid {}",
                    confirmation.drawer_id, confirmation.drawer_rowid
                )
            })?;
        let Some(row) = row else {
            occ_missing_current_count += 1;
            continue;
        };
        let Some(snapshot) = historical_rejudge_snapshot_from_confirmation(confirmation) else {
            occ_other_skip_count += 1;
            continue;
        };
        let current_snapshot = historical_rejudge_artifact_snapshot_from_row(&row);
        if current_snapshot != snapshot {
            occ_retention_snapshot_mismatch_count += 1;
            continue;
        }
        let decision = HistoricalRejudgeDecision {
            delete_candidate: true,
            protected: false,
            reason: confirmation.confirm_reason.clone(),
            label: Some("llm_judge".to_string()),
            score: Some(confirmation.confirm_score),
            tier: 3,
            judge: "llm".to_string(),
            requires_confirmation: false,
        };
        match build_historical_rejudge_backup_item(db, &row, &decision, mutation)? {
            Some(item) => backup_items.push(item),
            None => occ_other_skip_count += 1,
        }
    }

    let stale_count = occ_missing_current_count
        + occ_content_hash_mismatch_count
        + occ_retention_snapshot_mismatch_count
        + occ_other_skip_count;
    let options_hash = sha256_hex(&serde_json::to_vec(&serde_json::json!({
        "artifact_options_hash": &manifest.options_hash,
        "hard_delete": hard_delete,
    }))?);
    let endpoint_model_fingerprint = sha256_hex(&serde_json::to_vec(&serde_json::json!({
        "proposal": &manifest.proposal_endpoint_model_fingerprint,
        "confirmation": &manifest.confirmation_endpoint_model_fingerprint,
    }))?);
    let report = HistoricalRejudgeApplyReport {
        receipt_schema_version: HISTORICAL_REJUDGE_APPLY_RECEIPT_SCHEMA_VERSION.to_string(),
        dry_run: true,
        backup_path: None,
        backup_format: None,
        hard_delete,
        schema_version: manifest.schema_version.clone(),
        generation_id: Some(manifest.generation_id.clone()),
        manifest_file_sha256,
        config_hash: manifest.config_hash.clone(),
        artifact_options_hash: manifest.options_hash.clone(),
        proposals_file_sha256: manifest.proposals_file_sha256.clone(),
        confirmations_file_sha256,
        cursor_file_sha256: manifest.cursor_file_sha256.clone(),
        policy_fingerprint: manifest.policy_fingerprint.clone(),
        options_hash,
        proposal_endpoint_model_fingerprint: manifest.proposal_endpoint_model_fingerprint.clone(),
        confirmation_endpoint_model_fingerprint: manifest
            .confirmation_endpoint_model_fingerprint
            .clone(),
        endpoint_model_fingerprint,
        db_fence: historical_rejudge_db_fence(db)?,
        confirmation_count: confirmations.len(),
        keep_count,
        delete_count,
        matched_count: backup_items.len(),
        stale_count,
        occ_missing_current_count,
        occ_content_hash_mismatch_count,
        occ_retention_snapshot_mismatch_count,
        occ_other_skip_count,
        skipped_count: stale_count,
        mutated_count: 0,
    };
    Ok(HistoricalRejudgeApplyPlan {
        report,
        backup_items,
    })
}
