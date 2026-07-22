// Artifact-generation and receipt contracts for historical rejudge.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HistoricalRejudgeArtifactStageStatus {
    InProgress,
    Partial,
    Completed,
}

fn historical_rejudge_proposal_stage_status(
    cursor_rowid: i64,
    snapshot_max_rowid: i64,
    proposal_count: usize,
    snapshot_count: usize,
) -> HistoricalRejudgeArtifactStageStatus {
    if cursor_rowid >= snapshot_max_rowid && proposal_count == snapshot_count {
        HistoricalRejudgeArtifactStageStatus::Completed
    } else {
        HistoricalRejudgeArtifactStageStatus::Partial
    }
}

fn historical_rejudge_confirmation_stage_status(
    proposal_stage_status: HistoricalRejudgeArtifactStageStatus,
    pending_count: usize,
    confirmed_count: usize,
    circuit_broken: bool,
) -> HistoricalRejudgeArtifactStageStatus {
    if proposal_stage_status == HistoricalRejudgeArtifactStageStatus::Completed
        && pending_count.saturating_sub(confirmed_count) == 0
        && !circuit_broken
    {
        HistoricalRejudgeArtifactStageStatus::Completed
    } else {
        HistoricalRejudgeArtifactStageStatus::Partial
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalRejudgeArtifactManifest {
    schema_version: String,
    generation_id: String,
    source_snapshot_count: usize,
    source_snapshot_max_rowid: i64,
    proposal_stage_status: HistoricalRejudgeArtifactStageStatus,
    confirmation_stage_status: HistoricalRejudgeArtifactStageStatus,
    config_hash: String,
    options_hash: String,
    proposal_endpoint_model_fingerprint: String,
    confirmation_endpoint_model_fingerprint: String,
    policy_fingerprint: String,
    proposals_file_sha256: String,
    confirmations_file_sha256: String,
    cursor_file_sha256: String,
    proposal_count: usize,
    confirmation_count: usize,
    updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalRejudgeApplyReport {
    receipt_schema_version: String,
    dry_run: bool,
    backup_path: Option<PathBuf>,
    backup_format: Option<String>,
    hard_delete: bool,
    schema_version: String,
    generation_id: Option<String>,
    manifest_file_sha256: String,
    config_hash: String,
    artifact_options_hash: String,
    proposals_file_sha256: String,
    confirmations_file_sha256: String,
    cursor_file_sha256: String,
    policy_fingerprint: String,
    options_hash: String,
    proposal_endpoint_model_fingerprint: String,
    confirmation_endpoint_model_fingerprint: String,
    endpoint_model_fingerprint: String,
    db_fence: HistoricalRejudgeDbFence,
    confirmation_count: usize,
    keep_count: usize,
    delete_count: usize,
    matched_count: usize,
    stale_count: usize,
    occ_missing_current_count: usize,
    occ_content_hash_mismatch_count: usize,
    occ_retention_snapshot_mismatch_count: usize,
    occ_other_skip_count: usize,
    skipped_count: usize,
    mutated_count: usize,
}

struct HistoricalRejudgeApplyPlan {
    report: HistoricalRejudgeApplyReport,
    backup_items: Vec<HistoricalRejudgeBackupItem>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoricalRejudgeDbFence {
    schema_version: u32,
    sqlite_writer_generation: u64,
    historical_rejudge_writer_generation: u64,
}

fn historical_rejudge_artifact_key(rowid: i64, drawer_id: &str) -> String {
    format!("{rowid}:{drawer_id}")
}

fn historical_rejudge_artifact_snapshot_from_row(
    row: &mempal::core::db::HistoricalRejudgeCandidate,
) -> HistoricalRejudgeArtifactSnapshot {
    HistoricalRejudgeArtifactSnapshot {
        content_hash: historical_rejudge_content_hash(&row.drawer.content),
        added_at: row.drawer.added_at.clone(),
        importance: row.drawer.importance,
        is_pinned: row.drawer.is_pinned,
        effective_importance: row.drawer.effective_importance,
        status: row
            .drawer
            .status
            .as_ref()
            .map(knowledge_status_slug)
            .unwrap_or_default()
            .to_string(),
        memory_kind: memory_kind_slug(&row.drawer.memory_kind).to_string(),
        wing: row.drawer.wing.clone(),
        room: row.drawer.room.clone().unwrap_or_default(),
        source_file: row.drawer.source_file.clone(),
        source_type: row.drawer.source_type.as_str().to_string(),
        project_id: row.project_id.clone().unwrap_or_default(),
        chunk_index: row.drawer.chunk_index,
        normalize_version: i64::from(row.drawer.normalize_version),
    }
}

fn historical_rejudge_snapshot_from_proposal(
    proposal: &HistoricalRejudgeProposalArtifactLine,
) -> Option<HistoricalRejudgeArtifactSnapshot> {
    Some(HistoricalRejudgeArtifactSnapshot {
        content_hash: non_empty_snapshot_string(&proposal.snapshot_content_hash)?,
        added_at: non_empty_snapshot_string(&proposal.snapshot_added_at)?,
        importance: proposal.snapshot_importance?,
        is_pinned: proposal.snapshot_is_pinned?,
        effective_importance: proposal.snapshot_effective_importance?,
        status: proposal.snapshot_status.clone()?,
        memory_kind: proposal.snapshot_memory_kind.clone()?,
        wing: non_empty_snapshot_string(&proposal.snapshot_wing)?,
        room: proposal.snapshot_room.clone()?,
        source_file: proposal.snapshot_source_file.clone(),
        source_type: non_empty_snapshot_string(&proposal.snapshot_source_type)?,
        project_id: proposal.snapshot_project_id.clone()?,
        chunk_index: proposal.snapshot_chunk_index,
        normalize_version: proposal.snapshot_normalize_version?,
    })
}

fn historical_rejudge_snapshot_from_confirmation(
    confirmation: &HistoricalRejudgeConfirmationArtifactLine,
) -> Option<HistoricalRejudgeArtifactSnapshot> {
    Some(HistoricalRejudgeArtifactSnapshot {
        content_hash: non_empty_snapshot_string(&confirmation.snapshot_content_hash)?,
        added_at: non_empty_snapshot_string(&confirmation.snapshot_added_at)?,
        importance: confirmation.snapshot_importance?,
        is_pinned: confirmation.snapshot_is_pinned?,
        effective_importance: confirmation.snapshot_effective_importance?,
        status: confirmation.snapshot_status.clone()?,
        memory_kind: confirmation.snapshot_memory_kind.clone()?,
        wing: confirmation.snapshot_wing.clone()?,
        room: confirmation.snapshot_room.clone()?,
        source_file: confirmation.snapshot_source_file.clone(),
        source_type: non_empty_snapshot_string(&confirmation.snapshot_source_type)?,
        project_id: confirmation.snapshot_project_id.clone()?,
        chunk_index: confirmation.snapshot_chunk_index,
        normalize_version: confirmation.snapshot_normalize_version?,
    })
}

fn non_empty_snapshot_string(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn historical_rejudge_proposal_artifact_line(
    row: &mempal::core::db::HistoricalRejudgeCandidate,
    proposal: &HistoricalRejudgeArtifactProposalDecision,
) -> HistoricalRejudgeProposalArtifactLine {
    let snapshot = historical_rejudge_artifact_snapshot_from_row(row);
    HistoricalRejudgeProposalArtifactLine {
        drawer_id: row.drawer.id.clone(),
        drawer_rowid: row.rowid,
        snapshot_content_hash: snapshot.content_hash,
        snapshot_importance: Some(snapshot.importance),
        snapshot_is_pinned: Some(snapshot.is_pinned),
        snapshot_effective_importance: Some(snapshot.effective_importance),
        snapshot_status: Some(snapshot.status),
        snapshot_memory_kind: Some(snapshot.memory_kind),
        snapshot_room: Some(snapshot.room),
        snapshot_source_file: snapshot.source_file,
        snapshot_source_type: snapshot.source_type,
        snapshot_chunk_index: snapshot.chunk_index,
        snapshot_normalize_version: Some(snapshot.normalize_version),
        snapshot_project_id: Some(snapshot.project_id),
        proposal_decision: if proposal.should_forget {
            "forget"
        } else {
            "keep"
        }
        .to_string(),
        proposal_score: proposal.score,
        proposal_reason: proposal.reason.clone(),
        snapshot_added_at: snapshot.added_at,
        snapshot_wing: snapshot.wing,
        timestamp: iso_timestamp(),
    }
}

fn historical_rejudge_confirmation_artifact_line(
    row: &mempal::core::db::HistoricalRejudgeCandidate,
    proposal: &HistoricalRejudgeProposalArtifactLine,
    decision: &HistoricalRejudgeDecision,
) -> HistoricalRejudgeConfirmationArtifactLine {
    let snapshot = historical_rejudge_artifact_snapshot_from_row(row);
    HistoricalRejudgeConfirmationArtifactLine {
        drawer_id: row.drawer.id.clone(),
        drawer_rowid: row.rowid,
        snapshot_content_hash: snapshot.content_hash,
        snapshot_importance: Some(snapshot.importance),
        snapshot_is_pinned: Some(snapshot.is_pinned),
        snapshot_effective_importance: Some(snapshot.effective_importance),
        snapshot_status: Some(snapshot.status),
        snapshot_memory_kind: Some(snapshot.memory_kind),
        snapshot_wing: Some(snapshot.wing),
        snapshot_room: Some(snapshot.room),
        snapshot_source_file: snapshot.source_file,
        snapshot_source_type: snapshot.source_type,
        snapshot_chunk_index: snapshot.chunk_index,
        snapshot_normalize_version: Some(snapshot.normalize_version),
        snapshot_project_id: Some(snapshot.project_id),
        final_decision: if decision.delete_candidate {
            "delete"
        } else {
            "keep"
        }
        .to_string(),
        confirm_score: decision.score.unwrap_or_default(),
        confirm_reason: decision.reason.clone(),
        proposal_score: proposal.proposal_score,
        snapshot_added_at: snapshot.added_at,
        timestamp: iso_timestamp(),
    }
}

fn historical_rejudge_options_hash(
    options: HistoricalRejudgeOptions<'_>,
    project_id: Option<&str>,
    config_version: &str,
) -> Result<String> {
    let value = serde_json::json!({
        "hard_delete": options.hard_delete,
        "wing": options.wing,
        "room": options.room,
        "project_id": project_id,
        "proposal_llm_endpoint": options.proposal_llm_endpoint,
        "confirm_llm_endpoint": options.confirm_llm_endpoint,
        "config_version": config_version,
    });
    Ok(sha256_hex(&serde_json::to_vec(&value)?))
}

fn historical_rejudge_artifact_endpoint_model_fingerprints(
    llm_context: Option<&HistoricalRejudgeLlmContext>,
    options: HistoricalRejudgeOptions<'_>,
) -> (String, String) {
    let fingerprint = |role: &str, identity: &str| {
        sha256_hex(
            &serde_json::to_vec(&serde_json::json!({
                "role": role,
                "identity": identity,
            }))
            .unwrap_or_default(),
        )
    };
    match llm_context {
        Some(HistoricalRejudgeLlmContext::Single { model_summary, .. }) => (
            fingerprint("proposal", model_summary),
            fingerprint("confirmation", model_summary),
        ),
        Some(HistoricalRejudgeLlmContext::TwoStage {
            proposal_summary,
            confirm_summary,
            ..
        }) => (
            fingerprint("proposal", proposal_summary),
            fingerprint("confirmation", confirm_summary),
        ),
        None => (
            fingerprint(
                "proposal",
                options
                    .proposal_llm_endpoint
                    .unwrap_or("deterministic-unconfigured"),
            ),
            fingerprint(
                "confirmation",
                options
                    .confirm_llm_endpoint
                    .unwrap_or("deterministic-unconfigured"),
            ),
        ),
    }
}

fn historical_rejudge_config_identity(config: &Config) -> serde_json::Value {
    serde_json::json!({
        "gating": config.ingest_gating,
        "llm": {
            "enabled": config.llm.enabled,
            "backend": config.llm.backend.clone(),
            "endpoints": config.llm.effective_endpoint_fingerprints(),
            "enabled_for": config.llm.enabled_for.clone(),
            "max_concurrent": config.llm.max_concurrent,
            "request_timeout_secs": config.llm.request_timeout_secs,
            "retry_interval_secs": config.llm.retry_interval_secs,
        }
    })
}

fn historical_rejudge_artifact_config_hash(config: &Config) -> Result<String> {
    Ok(sha256_hex(&serde_json::to_vec(
        &historical_rejudge_config_identity(config),
    )?))
}

fn read_historical_rejudge_artifact_manifest(
    path: &Path,
) -> Result<HistoricalRejudgeArtifactManifest> {
    Ok(read_historical_rejudge_artifact_manifest_with_sha256(path)?.0)
}

fn read_historical_rejudge_artifact_manifest_with_sha256(
    path: &Path,
) -> Result<(HistoricalRejudgeArtifactManifest, String)> {
    let raw = fs::read(path)
        .with_context(|| format!("failed to read artifact manifest {}", path.display()))?;
    let manifest: HistoricalRejudgeArtifactManifest = serde_json::from_slice(&raw)
        .with_context(|| format!("failed to parse artifact manifest {}", path.display()))?;
    if manifest.schema_version != HISTORICAL_REJUDGE_ARTIFACT_SCHEMA_VERSION {
        bail!(
            "unsupported historical rejudge artifact schema version: {}",
            manifest.schema_version
        );
    }
    for (field, value) in [
        ("generation_id", manifest.generation_id.as_str()),
        ("config_hash", manifest.config_hash.as_str()),
        ("options_hash", manifest.options_hash.as_str()),
        (
            "proposal_endpoint_model_fingerprint",
            manifest.proposal_endpoint_model_fingerprint.as_str(),
        ),
        (
            "confirmation_endpoint_model_fingerprint",
            manifest.confirmation_endpoint_model_fingerprint.as_str(),
        ),
        ("policy_fingerprint", manifest.policy_fingerprint.as_str()),
        (
            "proposals_file_sha256",
            manifest.proposals_file_sha256.as_str(),
        ),
        (
            "confirmations_file_sha256",
            manifest.confirmations_file_sha256.as_str(),
        ),
        ("cursor_file_sha256", manifest.cursor_file_sha256.as_str()),
    ] {
        if value.trim().is_empty() {
            bail!("historical rejudge artifact manifest is missing {field}");
        }
    }
    validate_historical_rejudge_artifact_manifest_state(&manifest)?;
    Ok((manifest, sha256_hex(&raw)))
}

fn validate_historical_rejudge_artifact_manifest_state(
    manifest: &HistoricalRejudgeArtifactManifest,
) -> Result<()> {
    if manifest.confirmation_stage_status == HistoricalRejudgeArtifactStageStatus::Completed
        && manifest.proposal_stage_status != HistoricalRejudgeArtifactStageStatus::Completed
    {
        bail!(
            "historical rejudge artifact confirmation stage cannot complete before proposal stage"
        );
    }
    if manifest.proposal_count > manifest.source_snapshot_count {
        bail!("historical rejudge artifact proposal_count exceeds source snapshot count");
    }
    if manifest.confirmation_count > manifest.proposal_count {
        bail!("historical rejudge artifact confirmation_count exceeds proposal_count");
    }
    if manifest.proposal_stage_status == HistoricalRejudgeArtifactStageStatus::Completed
        && manifest.proposal_count != manifest.source_snapshot_count
    {
        bail!("historical rejudge artifact proposal stage is incomplete");
    }
    Ok(())
}

fn historical_rejudge_artifact_file_sha256(path: &Path) -> Result<String> {
    match fs::read(path) {
        Ok(bytes) => Ok(sha256_hex(&bytes)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(sha256_hex(&[])),
        Err(error) => {
            Err(error).with_context(|| format!("failed to hash artifact file {}", path.display()))
        }
    }
}

fn validate_historical_rejudge_artifact_manifest_identity(
    manifest: &HistoricalRejudgeArtifactManifest,
    options_hash: &str,
    config_hash: &str,
    proposal_endpoint_model_fingerprint: &str,
    confirmation_endpoint_model_fingerprint: &str,
    policy_fingerprint: &str,
) -> Result<()> {
    for (field, actual, expected) in [
        ("options_hash", manifest.options_hash.as_str(), options_hash),
        ("config_hash", manifest.config_hash.as_str(), config_hash),
        (
            "proposal_endpoint_model_fingerprint",
            manifest.proposal_endpoint_model_fingerprint.as_str(),
            proposal_endpoint_model_fingerprint,
        ),
        (
            "confirmation_endpoint_model_fingerprint",
            manifest.confirmation_endpoint_model_fingerprint.as_str(),
            confirmation_endpoint_model_fingerprint,
        ),
        (
            "policy_fingerprint",
            manifest.policy_fingerprint.as_str(),
            policy_fingerprint,
        ),
    ] {
        if actual != expected {
            bail!("historical rejudge artifact manifest {field} does not match current options");
        }
    }
    Ok(())
}

fn validate_historical_rejudge_artifact_manifest_hashes(
    manifest: &HistoricalRejudgeArtifactManifest,
    proposals_path: &Path,
    confirmations_path: &Path,
    cursor_path: &Path,
) -> Result<()> {
    for (field, actual, expected) in [
        (
            "proposals_file_sha256",
            historical_rejudge_artifact_file_sha256(proposals_path)?,
            manifest.proposals_file_sha256.as_str(),
        ),
        (
            "confirmations_file_sha256",
            historical_rejudge_artifact_file_sha256(confirmations_path)?,
            manifest.confirmations_file_sha256.as_str(),
        ),
        (
            "cursor_file_sha256",
            historical_rejudge_artifact_file_sha256(cursor_path)?,
            manifest.cursor_file_sha256.as_str(),
        ),
    ] {
        if actual != expected {
            bail!("historical rejudge artifact manifest {field} does not match canonical bytes");
        }
    }
    Ok(())
}

struct HistoricalRejudgeArtifactManifestRefresh<'a> {
    proposals_path: &'a Path,
    confirmations_path: &'a Path,
    cursor_path: &'a Path,
    proposal_stage_status: HistoricalRejudgeArtifactStageStatus,
    confirmation_stage_status: HistoricalRejudgeArtifactStageStatus,
    proposal_count: usize,
    confirmation_count: usize,
}

fn refresh_historical_rejudge_artifact_manifest(
    manifest_path: &Path,
    manifest: &mut HistoricalRejudgeArtifactManifest,
    refresh: HistoricalRejudgeArtifactManifestRefresh<'_>,
) -> Result<()> {
    manifest.proposal_stage_status = refresh.proposal_stage_status;
    manifest.confirmation_stage_status = refresh.confirmation_stage_status;
    manifest.proposal_count = refresh.proposal_count;
    manifest.confirmation_count = refresh.confirmation_count;
    manifest.proposals_file_sha256 =
        historical_rejudge_artifact_file_sha256(refresh.proposals_path)?;
    manifest.confirmations_file_sha256 =
        historical_rejudge_artifact_file_sha256(refresh.confirmations_path)?;
    manifest.cursor_file_sha256 = historical_rejudge_artifact_file_sha256(refresh.cursor_path)?;
    manifest.updated_at = iso_timestamp();
    write_historical_rejudge_artifact_manifest(manifest_path, manifest)
}

fn write_historical_rejudge_artifact_manifest(
    path: &Path,
    manifest: &HistoricalRejudgeArtifactManifest,
) -> Result<()> {
    validate_historical_rejudge_artifact_manifest_state(manifest)?;
    let tmp_path = path.with_extension("json.tmp");
    let payload =
        serde_json::to_vec_pretty(manifest).context("failed to encode artifact manifest")?;
    fs::write(&tmp_path, payload)
        .with_context(|| format!("failed to write artifact manifest {}", tmp_path.display()))?;
    fs::rename(&tmp_path, path).with_context(|| {
        format!(
            "failed to replace artifact manifest {} from {}",
            path.display(),
            tmp_path.display()
        )
    })?;
    Ok(())
}
