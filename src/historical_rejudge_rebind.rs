use super::*;

#[derive(serde::Serialize)]
struct HistoricalRejudgeRebindReport {
    dry_run: bool,
    checkpoint_rebound: bool,
    mutation: &'static str,
    checkpoint_status: String,
    checkpoint_page_size: usize,
}

pub(super) fn maintenance_rejudge_rebind_command(
    db: &Database,
    config: &Config,
    options: HistoricalRejudgeOptions<'_>,
    execute: bool,
) -> Result<()> {
    validate_historical_rejudge_rebind_options(options)?;

    let current_dir = env::current_dir().ok();
    let project_id = if options.project.is_some() {
        resolve_project_id(options.project, config, current_dir.as_deref())
            .context("failed to resolve historical rejudge project id")?
    } else {
        None
    };
    let config_version = historical_rejudge_config_version(config);
    let llm_context = historical_rejudge_llm_context(config, options)?;
    let judge_model = llm_context
        .as_ref()
        .map(|context| Ok(context.model_summary()))
        .or_else(|| {
            llm_context
                .is_none()
                .then(|| historical_rejudge_deterministic_model_summary(config))
        })
        .transpose()?;

    let writer_lease = if execute {
        Some(acquire_historical_rejudge_writer_lease(db)?)
    } else {
        None
    };
    let mut checkpoint = prepare_historical_rejudge_rebind(
        db,
        options,
        project_id.as_deref(),
        &config_version,
        judge_model.as_ref(),
    )?;

    if let Some(writer_lease) = writer_lease.as_ref() {
        persist_historical_rejudge_rebind(
            db,
            &mut checkpoint,
            options,
            project_id.as_deref(),
            config_version,
            judge_model,
            writer_lease,
        )?;
    }

    print_historical_rejudge_rebind_report(
        &HistoricalRejudgeRebindReport {
            dry_run: !execute,
            checkpoint_rebound: execute,
            mutation: "checkpoint_rebind",
            checkpoint_status: checkpoint.status,
            checkpoint_page_size: checkpoint.page_size,
        },
        options.format,
    )
}

fn validate_historical_rejudge_rebind_options(options: HistoricalRejudgeOptions<'_>) -> Result<()> {
    if options.format != "json" && options.format != "plain" {
        bail!(
            "unsupported maintenance rejudge rebind format: {}",
            options.format
        );
    }
    if !options.all || !options.resume {
        bail!("checkpoint rebind requires --all --resume to bind the existing full sweep");
    }
    if options.execute
        || options.hard_delete
        || options.backup_dir.is_some()
        || options.unsafe_no_backup
        || options.unsafe_allow_config_version_drift
        || options.progress_file.is_some()
        || options.candidates_file.is_some()
        || options.stage_mode != HistoricalRejudgeStageMode::Paired
    {
        bail!(
            "checkpoint rebind accepts only equivalent full-sweep binding flags; use `rebind --execute` for the sole mutation"
        );
    }
    Ok(())
}

fn prepare_historical_rejudge_rebind(
    db: &Database,
    options: HistoricalRejudgeOptions<'_>,
    project_id: Option<&str>,
    config_version: &str,
    judge_model: Option<&String>,
) -> Result<HistoricalRejudgeCheckpoint> {
    let checkpoint = load_historical_rejudge_checkpoint(db)?
        .context("no historical rejudge checkpoint exists to rebind")?;
    if checkpoint.status == "done" {
        bail!("historical rejudge checkpoint is already done; omit --resume to start a new sweep");
    }
    let options_hash = historical_rejudge_options_hash(options, project_id, config_version)?;
    if !historical_rejudge_checkpoint_matches_resume(
        &checkpoint,
        options,
        project_id,
        &options_hash,
        judge_model,
    )? {
        bail!("historical rejudge checkpoint does not match current filters/config");
    }
    Ok(checkpoint)
}

fn persist_historical_rejudge_rebind(
    db: &Database,
    checkpoint: &mut HistoricalRejudgeCheckpoint,
    options: HistoricalRejudgeOptions<'_>,
    project_id: Option<&str>,
    config_version: String,
    judge_model: Option<String>,
    writer_lease: &MaintenanceWriterLeaseGuard,
) -> Result<()> {
    checkpoint.options_hash =
        historical_rejudge_options_hash(options, project_id, &config_version)?;
    checkpoint.config_version = config_version;
    checkpoint.judge_model = judge_model;
    checkpoint.updated_at = iso_timestamp();
    save_historical_rejudge_checkpoint_with_writer_lease(
        db,
        checkpoint,
        Some(writer_lease),
        "persist historical rejudge checkpoint rebind",
    )
}

fn print_historical_rejudge_rebind_report(
    report: &HistoricalRejudgeRebindReport,
    format: &str,
) -> Result<()> {
    match format {
        "json" => println!("{}", serde_json::to_string(report)?),
        "plain" => println!(
            "historical rejudge checkpoint rebind: dry_run={} checkpoint_rebound={} status={} page_size={}",
            report.dry_run,
            report.checkpoint_rebound,
            report.checkpoint_status,
            report.checkpoint_page_size
        ),
        other => bail!("unsupported maintenance rejudge rebind format: {other}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rebind_options<'a>() -> HistoricalRejudgeOptions<'a> {
        HistoricalRejudgeOptions {
            execute: false,
            hard_delete: false,
            backup_dir: None,
            unsafe_no_backup: false,
            limit: 1000,
            all: true,
            resume: true,
            unsafe_allow_config_version_drift: false,
            page_size: DEFAULT_HISTORICAL_REJUDGE_PAGE_SIZE,
            progress_file: None,
            candidates_file: None,
            stage_mode: HistoricalRejudgeStageMode::Paired,
            proposal_llm_endpoint: Some("qwen"),
            confirm_llm_endpoint: Some("spark"),
            wing: None,
            room: None,
            project: None,
            format: "json",
        }
    }

    fn legacy_checkpoint(options: HistoricalRejudgeOptions<'_>) -> HistoricalRejudgeCheckpoint {
        HistoricalRejudgeCheckpoint {
            run_id: "legacy-rebind-run".to_string(),
            status: "running".to_string(),
            options_hash: historical_rejudge_options_hash(options, None, "old-config-version")
                .expect("legacy options hash"),
            started_at: "2026-07-23T00:00:00Z".to_string(),
            updated_at: "2026-07-23T00:00:00Z".to_string(),
            snapshot_max_rowid: 522601,
            snapshot_count: 518200,
            last_processed_rowid: Some(222299),
            scanned_count: 189902,
            candidate_count: 16406,
            kept_count: 153431,
            protected_count: 20062,
            mutated_count: 16406,
            estimated_bytes_reclaimed: 8152777,
            mutation: "soft_delete".to_string(),
            page_size: DEFAULT_HISTORICAL_REJUDGE_PAGE_SIZE,
            judge_model: Some("proposal:qwen=qwen-model, confirm:spark=spark-model".to_string()),
            config_version: "old-config-version".to_string(),
            backup_path: Some(PathBuf::from("/backup/rejudge.sqlite")),
        }
    }

    fn current_judge_model() -> String {
        "proposal:qwen=qwen-model@endpoint:proposal-v1, confirm:spark=spark-model@endpoint:confirm-v1@policy:policy-v1".to_string()
    }

    #[test]
    fn rebind_preflight_uses_core_matcher_and_dry_run_does_not_mutate_checkpoint() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        ensure_historical_rejudge_checkpoint_storage(&db).expect("ensure checkpoint storage");
        let options = rebind_options();
        let checkpoint = legacy_checkpoint(options);
        let before = serde_json::to_value(&checkpoint).expect("serialize checkpoint");
        save_historical_rejudge_checkpoint(&db, &checkpoint).expect("save checkpoint");

        let judge_model = current_judge_model();
        let resumed = prepare_historical_rejudge_rebind(
            &db,
            options,
            None,
            "current-config-version",
            Some(&judge_model),
        )
        .expect("equivalent legacy two-stage checkpoint must pass the core matcher");

        assert_eq!(resumed.run_id, checkpoint.run_id);
        let incompatible_judge_model =
            "proposal:qwen=other-model@endpoint:proposal-v2, confirm:spark=spark-model@endpoint:confirm-v1@policy:policy-v1"
                .to_string();
        let error = prepare_historical_rejudge_rebind(
            &db,
            options,
            None,
            "current-config-version",
            Some(&incompatible_judge_model),
        )
        .expect_err("the rebind preflight must reject a changed judge binding");
        assert!(error.to_string().contains("does not match"));
        assert_eq!(
            serde_json::to_value(
                load_historical_rejudge_checkpoint(&db)
                    .expect("load")
                    .expect("checkpoint")
            )
            .expect("serialize persisted checkpoint"),
            before,
            "read-only preflight must not change the residual checkpoint"
        );
    }

    #[test]
    fn rebind_execute_changes_only_binding_under_writer_lease() {
        let tmp = tempfile::TempDir::new().expect("tempdir");
        let db = Database::open(&tmp.path().join("palace.db")).expect("open db");
        ensure_historical_rejudge_checkpoint_storage(&db).expect("ensure checkpoint storage");
        let options = rebind_options();
        let checkpoint = legacy_checkpoint(options);
        save_historical_rejudge_checkpoint(&db, &checkpoint).expect("save checkpoint");
        let judge_model = current_judge_model();
        let mut rebound = prepare_historical_rejudge_rebind(
            &db,
            options,
            None,
            "current-config-version",
            Some(&judge_model),
        )
        .expect("preflight");

        let writer_lease = acquire_historical_rejudge_writer_lease(&db).expect("writer lease");
        assert!(
            acquire_historical_rejudge_writer_lease(&db).is_err(),
            "a concurrent rebind writer must be rejected before mutation"
        );
        persist_historical_rejudge_rebind(
            &db,
            &mut rebound,
            options,
            None,
            "current-config-version".to_string(),
            Some(judge_model.clone()),
            &writer_lease,
        )
        .expect("persist under writer lease");
        drop(writer_lease);

        let persisted = load_historical_rejudge_checkpoint(&db)
            .expect("load")
            .expect("checkpoint");
        let mut expected_unchanged = serde_json::to_value(&checkpoint).expect("serialize before");
        let mut actual_unchanged = serde_json::to_value(&persisted).expect("serialize after");
        for key in [
            "options_hash",
            "config_version",
            "judge_model",
            "updated_at",
        ] {
            expected_unchanged
                .as_object_mut()
                .expect("before object")
                .remove(key);
            actual_unchanged
                .as_object_mut()
                .expect("after object")
                .remove(key);
        }
        assert_eq!(actual_unchanged, expected_unchanged);
        assert_eq!(persisted.config_version, "current-config-version");
        assert_eq!(persisted.judge_model, Some(judge_model));
        assert_eq!(
            persisted.options_hash,
            historical_rejudge_options_hash(options, None, "current-config-version")
                .expect("current options hash")
        );
    }

    #[test]
    fn rebind_requires_explicit_full_sweep_resume_binding() {
        let error = validate_historical_rejudge_rebind_options(HistoricalRejudgeOptions {
            resume: false,
            ..rebind_options()
        })
        .expect_err("rebind must not bind a new or partial sweep");
        assert!(error.to_string().contains("--all --resume"));
    }

    #[test]
    fn rebind_subcommand_parses_with_outer_binding_flags() {
        let parsed = std::thread::Builder::new()
            .stack_size(32 * 1024 * 1024)
            .spawn(|| {
                Cli::try_parse_from([
                    "mempal",
                    "maintenance",
                    "rejudge",
                    "--all",
                    "--resume",
                    "--page-size",
                    "500",
                    "--proposal-llm-endpoint",
                    "qwen",
                    "--confirm-llm-endpoint",
                    "spark",
                    "--format",
                    "json",
                    "rebind",
                    "--execute",
                ])
                .map(|_| ())
                .map_err(|error| error.to_string())
            })
            .expect("spawn parser thread")
            .join()
            .expect("parser thread");
        if let Err(error) = parsed {
            panic!("rebind subcommand must parse: {error}");
        }
    }
}
