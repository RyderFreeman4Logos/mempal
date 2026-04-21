use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use std::{future::Future, pin::Pin};

use crate::core::{
    db::Database,
    queue::{ClaimedMessage, PendingMessageStore},
    types::{Drawer, SourceType},
    utils::{current_timestamp, synthetic_source_file},
};
use crate::embed::{
    EmbedError, Embedder, build_backend_from_name, global_embed_status,
    retry::{HeartbeatCallback, retry_embed_operation},
};
use crate::ingest::gating::{
    GatingDecision, IngestCandidate, PrototypeClassifier, compile_classifier_from_embedder,
    evaluate_tier1, tier2_enabled,
};
use crate::ingest::novelty::{NoveltyAction, NoveltyCandidate, evaluate as evaluate_novelty};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::daemon_bootstrap::DaemonContext;
use crate::hook::CapturedHookEnvelope;
use crate::hotpatch::generator::{GenerationOptions, suggest_for_drawer};
use crate::session_review::{SessionReviewOutcome, extract_session_review};

pub fn run_command(config_path: PathBuf, foreground: bool) -> Result<()> {
    let context = DaemonContext::bootstrap(config_path, foreground)?;
    context.runtime.block_on(run_loop(&context))
}

async fn run_loop(context: &DaemonContext) -> Result<()> {
    if !context.config.hooks.enabled {
        bail!("hooks not enabled");
    }

    install_shutdown_handlers()?;
    tracing::info!("daemon log path: {}", context.log_path.display());

    let embedder = DaemonEmbedder::from_config(context.config.as_ref())
        .await
        .context("failed to build daemon embedder")?;
    let prototype_classifier =
        compile_classifier_from_embedder(&embedder, &context.config.ingest_gating)
            .await
            .map_err(|error| anyhow::anyhow!(error.to_string()))
            .context("gating prototype init failed")?;
    let worker_id = format!("mempal-daemon-{}", std::process::id());
    let claim_ttl_secs = context.config.hooks.daemon_claim_ttl_secs as i64;
    let poll_interval = Duration::from_millis(context.config.hooks.daemon_poll_interval_ms);
    let reclaimed = context
        .store
        .reclaim_stale(claim_ttl_secs)
        .context("failed to reclaim stale daemon claims")?;
    tracing::info!("daemon startup reclaim_stale reclaimed={reclaimed}");

    loop {
        if shutdown_requested() {
            tracing::info!("shutdown requested; stopping daemon loop");
            break;
        }

        match poll_claim_next(&context.store, &worker_id, claim_ttl_secs, |duration| {
            Box::pin(tokio::time::sleep(duration))
        })
        .await
        {
            ClaimPollResult::Claimed(message) => {
                let message_id = message.id.clone();
                let result = process_claimed_message_with_embedder(
                    &context.db,
                    &context.store,
                    &worker_id,
                    &message,
                    &embedder,
                    DaemonIngestContext {
                        prototype_classifier: prototype_classifier.as_ref(),
                        config: context.config.as_ref(),
                        mempal_home: &context.mempal_home,
                    },
                )
                .await;

                match result {
                    Ok(_) => {
                        context
                            .store
                            .confirm(&message_id)
                            .with_context(|| format!("failed to confirm {message_id}"))?;
                    }
                    Err(error) => {
                        tracing::error!("daemon message {message_id} failed: {error}");
                        context
                            .store
                            .mark_failed(&message_id, &error.to_string())
                            .with_context(|| format!("failed to mark_failed {message_id}"))?;
                    }
                }
            }
            ClaimPollResult::Idle => {
                tokio::time::sleep(poll_interval).await;
            }
            ClaimPollResult::RetryAfterError => continue,
        }
    }

    Ok(())
}

trait ClaimNextSource {
    fn claim_next(
        &self,
        worker_id: &str,
        claim_ttl_secs: i64,
    ) -> crate::core::queue::Result<Option<ClaimedMessage>>;
}

impl ClaimNextSource for PendingMessageStore {
    fn claim_next(
        &self,
        worker_id: &str,
        claim_ttl_secs: i64,
    ) -> crate::core::queue::Result<Option<ClaimedMessage>> {
        PendingMessageStore::claim_next(self, worker_id, claim_ttl_secs)
    }
}

enum ClaimPollResult {
    Claimed(ClaimedMessage),
    Idle,
    RetryAfterError,
}

type SleepFuture<'a> = Pin<Box<dyn Future<Output = ()> + Send + 'a>>;

pub struct DaemonIngestContext<'a> {
    pub prototype_classifier: Option<&'a PrototypeClassifier>,
    pub config: &'a crate::core::config::Config,
    pub mempal_home: &'a Path,
}

struct DrawerIngestContext<'a, E: Embedder + ?Sized> {
    db: &'a Database,
    store: &'a PendingMessageStore,
    worker_id: &'a str,
    message: &'a ClaimedMessage,
    embedder: &'a E,
    daemon: &'a DaemonIngestContext<'a>,
    envelope: &'a CapturedHookEnvelope,
}

async fn poll_claim_next<'a, S>(
    store: &impl ClaimNextSource,
    worker_id: &str,
    claim_ttl_secs: i64,
    sleep_on_error: S,
) -> ClaimPollResult
where
    S: Fn(Duration) -> SleepFuture<'a>,
{
    match store.claim_next(worker_id, claim_ttl_secs) {
        Ok(Some(message)) => ClaimPollResult::Claimed(message),
        Ok(None) => ClaimPollResult::Idle,
        Err(error) => {
            tracing::warn!(?error, "claim_next failed");
            sleep_on_error(Duration::from_secs(1)).await;
            ClaimPollResult::RetryAfterError
        }
    }
}

pub async fn process_claimed_message_with_embedder<E: Embedder + ?Sized>(
    db: &Database,
    store: &PendingMessageStore,
    worker_id: &str,
    message: &ClaimedMessage,
    embedder: &E,
    context: DaemonIngestContext<'_>,
) -> Result<String> {
    let envelope: CapturedHookEnvelope =
        serde_json::from_str(&message.payload).context("failed to decode queued hook envelope")?;
    let records = build_drawer_records(&envelope, context.config, context.mempal_home)?;
    let drawer_context = DrawerIngestContext {
        db,
        store,
        worker_id,
        message,
        embedder,
        daemon: &context,
        envelope: &envelope,
    };
    let mut last_drawer_id = None;
    for record in records {
        let drawer_id = ingest_drawer_record(&drawer_context, record).await?;
        if let Err(error) = suggest_for_drawer(
            db,
            context.config,
            context.mempal_home,
            &drawer_id,
            GenerationOptions::default(),
        ) {
            tracing::warn!(?error, drawer_id, "hotpatch suggestion generation failed");
        }
        last_drawer_id = Some(drawer_id);
    }

    Ok(last_drawer_id.unwrap_or_else(|| message.id.clone()))
}

fn build_gating_candidate(
    envelope: &CapturedHookEnvelope,
    record: &DrawerRecord,
) -> IngestCandidate {
    let mut tool_name = None;
    let mut exit_code = None;

    if envelope.event == crate::hook::HookEvent::PostToolUse.display_name()
        && let Some(payload) = envelope.payload.as_deref()
        && let Ok(value) = serde_json::from_str::<Value>(payload)
    {
        tool_name = value
            .get("tool_name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        exit_code = value
            .get("exit_code")
            .and_then(Value::as_i64)
            .and_then(|value| i32::try_from(value).ok());
    }

    IngestCandidate {
        content: record.content.clone(),
        tool_name,
        exit_code,
    }
}

async fn embed_text_with_heartbeat<E: Embedder + ?Sized>(
    embedder: &E,
    content: &str,
    heartbeat: Option<&HeartbeatCallback>,
) -> crate::embed::Result<Vec<f32>> {
    let status = global_embed_status();
    let texts = [content];
    let vectors =
        retry_embed_operation(status, heartbeat, || async { embedder.embed(&texts).await }).await?;
    status.record_primary_success();
    vectors
        .into_iter()
        .next()
        .ok_or_else(|| EmbedError::Runtime("embedder returned no vectors".to_string()))
}

#[derive(Debug)]
struct DrawerRecord {
    wing: String,
    room: String,
    source_file: String,
    content: String,
    importance: i32,
    bypass_novelty: bool,
}

fn build_drawer_records(
    envelope: &CapturedHookEnvelope,
    config: &crate::core::config::Config,
    mempal_home: &Path,
) -> Result<Vec<DrawerRecord>> {
    let mut records = vec![build_audit_drawer_record(envelope, config, mempal_home)?];
    if envelope.event == crate::hook::HookEvent::SessionEnd.display_name() {
        let session_review_payload = if config.hooks.session_end.extract_self_review {
            load_session_review_payload(envelope)?
        } else {
            None
        };
        match extract_session_review(
            session_review_payload.as_deref(),
            &envelope.agent,
            &config.hooks.session_end,
        )? {
            SessionReviewOutcome::Review(review) => records.push(DrawerRecord {
                wing: review.wing,
                room: review.room,
                source_file: review.source_file,
                content: config.scrub_content(&review.content),
                importance: review.importance,
                bypass_novelty: true,
            }),
            SessionReviewOutcome::Skipped(reason) => {
                tracing::info!(?reason, "session self-review skipped");
            }
        }
    }

    Ok(records)
}

fn load_session_review_payload(envelope: &CapturedHookEnvelope) -> Result<Option<String>> {
    if !envelope.truncated {
        return Ok(envelope.payload.clone());
    }

    let payload_path = envelope
        .payload_path
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("truncated session_end missing payload_path"))?;
    fs::read_to_string(payload_path).map(Some).with_context(|| {
        format!(
            "failed to read truncated session_end payload {}",
            payload_path
        )
    })
}

fn build_audit_drawer_record(
    envelope: &CapturedHookEnvelope,
    config: &crate::core::config::Config,
    mempal_home: &Path,
) -> Result<DrawerRecord> {
    if envelope.truncated {
        let preview = config.scrub_content(envelope.payload_preview.as_deref().unwrap_or_default());
        let content = serde_json::to_string(&json!({
            "_truncated": true,
            "event": envelope.event,
            "agent": envelope.agent,
            "captured_at": envelope.captured_at,
            "claude_cwd": envelope.claude_cwd,
            "original_size_bytes": envelope.original_size_bytes,
            "payload_preview": preview,
            "payload_path": envelope.payload_path,
        }))
        .context("failed to serialize truncated hook marker")?;
        let source_file = envelope
            .payload_path
            .clone()
            .unwrap_or_else(|| synthetic_source_file("hook-truncated"));
        return Ok(DrawerRecord {
            wing: "hooks-raw".to_string(),
            room: "truncated".to_string(),
            source_file,
            content,
            importance: 0,
            bypass_novelty: false,
        });
    }

    let raw_payload = envelope.payload.as_deref().unwrap_or_default();
    let preview = config.scrub_content(&preview_for_event(&envelope.event, raw_payload));
    let payload_path = persist_raw_payload(raw_payload, mempal_home)?;
    let content = serde_json::to_string(&json!({
        "event": envelope.event,
        "agent": envelope.agent,
        "captured_at": envelope.captured_at,
        "claude_cwd": envelope.claude_cwd,
        "preview": preview,
        "meta": {
            "hook_payload_path": payload_path,
            "original_size_bytes": envelope.original_size_bytes,
        }
    }))
    .context("failed to serialize hook diary drawer")?;
    let (wing, room) = audit_target_for_event(&envelope.event, raw_payload, config);

    Ok(DrawerRecord {
        wing,
        room,
        source_file: payload_path,
        content,
        importance: 0,
        bypass_novelty: false,
    })
}

fn audit_target_for_event(
    event: &str,
    raw_payload: &str,
    config: &crate::core::config::Config,
) -> (String, String) {
    match event {
        "PostToolUse" => (
            "hooks-raw".to_string(),
            serde_json::from_str::<Value>(raw_payload)
                .ok()
                .and_then(|value| {
                    value
                        .get("tool_name")
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .unwrap_or_else(|| "unknown-tool".to_string()),
        ),
        "UserPromptSubmit" => ("hooks-raw".to_string(), "user-prompt".to_string()),
        "SessionStart" | "SessionEnd" => ("hooks-raw".to_string(), "session-lifecycle".to_string()),
        _ => (
            config.hooks.wing.clone(),
            envelope_agent_fallback(raw_payload),
        ),
    }
}

fn envelope_agent_fallback(raw_payload: &str) -> String {
    serde_json::from_str::<Value>(raw_payload)
        .ok()
        .and_then(|value| {
            value
                .get("agent")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| "unknown-agent".to_string())
}

async fn ingest_drawer_record<E: Embedder + ?Sized>(
    context: &DrawerIngestContext<'_, E>,
    record: DrawerRecord,
) -> Result<String> {
    let (drawer_id, exists) = context
        .db
        .resolve_ingest_drawer_id(
            &record.wing,
            Some(record.room.as_str()),
            &record.content,
            None,
        )
        .with_context(|| {
            format!(
                "failed to resolve drawer identity for {}/{}",
                record.wing, record.room
            )
        })?;
    if exists {
        return Ok(drawer_id);
    }

    let candidate = build_gating_candidate(context.envelope, &record);
    let mut gating_decision = evaluate_tier1(&candidate, &context.daemon.config.ingest_gating);
    if gating_decision.is_none() && !tier2_enabled(&context.daemon.config.ingest_gating) {
        gating_decision = Some(GatingDecision::accepted(
            0,
            Some("tier2_disabled".to_string()),
            None,
        ));
    }
    if let Some(decision) = gating_decision.as_ref()
        && decision.is_rejected()
    {
        context
            .db
            .record_gating_audit(&drawer_id, decision, None)
            .with_context(|| format!("failed to record gating audit {}", drawer_id))?;
        return Ok(drawer_id);
    }

    let heartbeat_store = context.store.clone();
    let heartbeat_message_id = context.message.id.clone();
    let heartbeat_worker_id = context.worker_id.to_string();
    let heartbeat = move || -> crate::embed::Result<()> {
        heartbeat_store
            .refresh_heartbeat(&heartbeat_message_id, &heartbeat_worker_id)
            .map_err(|error| EmbedError::Runtime(format!("refresh heartbeat failed: {error}")))?;
        Ok(())
    };

    let mut vector = None;
    let mut gating_audit_recorded = false;
    if gating_decision.is_none()
        && let Some(classifier) = context.daemon.prototype_classifier
    {
        let candidate_vector =
            embed_text_with_heartbeat(context.embedder, &record.content, Some(&heartbeat)).await?;
        let decision = classifier.decide(
            &candidate_vector,
            context
                .daemon
                .config
                .ingest_gating
                .embedding_classifier
                .threshold,
        );
        context
            .db
            .record_gating_audit(&drawer_id, &decision, None)
            .with_context(|| format!("failed to record gating audit {}", drawer_id))?;
        gating_audit_recorded = true;
        if decision.is_rejected() {
            return Ok(drawer_id);
        }
        gating_decision = Some(decision);
        vector = Some(candidate_vector);
    }
    if !gating_audit_recorded && let Some(decision) = gating_decision.as_ref() {
        context
            .db
            .record_gating_audit(&drawer_id, decision, None)
            .with_context(|| format!("failed to record gating audit {}", drawer_id))?;
    }

    let vector = match vector {
        Some(vector) => vector,
        None => {
            embed_text_with_heartbeat(context.embedder, &record.content, Some(&heartbeat)).await?
        }
    };
    if record.bypass_novelty {
        insert_drawer_with_vector(context.db, &drawer_id, &record, &vector)?;
        return Ok(drawer_id);
    }

    let novelty = evaluate_novelty(
        context.db,
        &NoveltyCandidate {
            wing: record.wing.clone(),
            room: Some(record.room.clone()),
        },
        &vector,
        &context.daemon.config.ingest_gating.novelty,
    );
    match novelty.action {
        NoveltyAction::Insert => {
            if novelty.should_audit {
                context
                    .db
                    .record_novelty_audit(
                        &drawer_id,
                        NoveltyAction::Insert,
                        novelty.near_drawer_id.as_deref(),
                        novelty.cosine,
                        novelty.audit_decision,
                        None,
                    )
                    .with_context(|| format!("failed to record novelty audit {}", drawer_id))?;
            }
            insert_drawer_with_vector(context.db, &drawer_id, &record, &vector)?;
            Ok(drawer_id)
        }
        NoveltyAction::Drop => {
            if novelty.should_audit {
                context
                    .db
                    .record_novelty_audit(
                        &drawer_id,
                        NoveltyAction::Drop,
                        novelty.near_drawer_id.as_deref(),
                        novelty.cosine,
                        novelty.audit_decision,
                        None,
                    )
                    .with_context(|| format!("failed to record novelty audit {}", drawer_id))?;
            }
            Ok(novelty.near_drawer_id.unwrap_or(drawer_id))
        }
        NoveltyAction::Merge => {
            let target_id = novelty
                .near_drawer_id
                .clone()
                .unwrap_or_else(|| drawer_id.clone());
            let _target_lock = if target_id == drawer_id {
                None
            } else {
                Some(
                    crate::ingest::lock::acquire_source_lock(
                        context.daemon.mempal_home,
                        &target_id,
                        Duration::from_secs(5),
                    )
                    .with_context(|| format!("failed to lock merge target {}", target_id))?,
                )
            };
            let (existing_content, merge_count) = context
                .db
                .drawer_merge_state(&target_id)
                .with_context(|| format!("failed to load merge target {}", target_id))?
                .ok_or_else(|| anyhow::anyhow!("novelty merge target missing: {}", target_id))?;
            let merged_at = current_timestamp();
            let merged_content = format!(
                "{existing_content}\n---\nSUPPLEMENTARY ({merged_at}):\n{}",
                record.content
            );
            let capped = merge_count
                >= context
                    .daemon
                    .config
                    .ingest_gating
                    .novelty
                    .max_merges_per_drawer
                || merged_content.len()
                    > context
                        .daemon
                        .config
                        .ingest_gating
                        .novelty
                        .max_content_bytes_per_drawer;
            if capped {
                context
                    .db
                    .record_novelty_audit(
                        &drawer_id,
                        NoveltyAction::Insert,
                        Some(target_id.as_str()),
                        novelty.cosine,
                        Some("insert_due_to_merge_cap"),
                        None,
                    )
                    .with_context(|| format!("failed to record novelty audit {}", drawer_id))?;
                insert_drawer_with_vector(context.db, &drawer_id, &record, &vector)?;
                Ok(drawer_id)
            } else {
                let merged_vector =
                    embed_text_with_heartbeat(context.embedder, &merged_content, Some(&heartbeat))
                        .await?;
                context
                    .db
                    .record_novelty_audit(
                        &drawer_id,
                        NoveltyAction::Merge,
                        Some(target_id.as_str()),
                        novelty.cosine,
                        novelty.audit_decision,
                        None,
                    )
                    .with_context(|| format!("failed to record novelty audit {}", drawer_id))?;
                let mut db_for_merge = Database::open(context.db.path())
                    .with_context(|| format!("failed to reopen db for merge {}", target_id))?;
                db_for_merge
                    .update_drawer_after_merge(
                        &target_id,
                        &merged_content,
                        &merged_at,
                        &merged_vector,
                    )
                    .with_context(|| format!("failed to merge hook drawer {}", target_id))?;
                Ok(target_id)
            }
        }
    }
}

fn insert_drawer_with_vector(
    db: &Database,
    drawer_id: &str,
    record: &DrawerRecord,
    vector: &[f32],
) -> Result<()> {
    if db
        .drawer_exists(drawer_id)
        .with_context(|| format!("failed to re-check existing drawer {}", drawer_id))?
    {
        return Ok(());
    }

    let drawer = Drawer {
        id: drawer_id.to_string(),
        content: record.content.clone(),
        wing: record.wing.clone(),
        room: Some(record.room.clone()),
        source_file: Some(record.source_file.clone()),
        source_type: SourceType::Manual,
        added_at: current_timestamp(),
        chunk_index: Some(0),
        importance: record.importance,
    };
    db.insert_drawer(&drawer)
        .with_context(|| format!("failed to insert hook drawer {}", drawer.id))?;
    db.insert_vector(&drawer.id, vector)
        .with_context(|| format!("failed to insert hook vector {}", drawer.id))?;
    Ok(())
}

fn preview_for_event(event: &str, raw_payload: &str) -> String {
    let parsed = serde_json::from_str::<Value>(raw_payload).ok();
    match event {
        "UserPromptSubmit" => parsed
            .as_ref()
            .and_then(|value| value.get("prompt"))
            .and_then(Value::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| raw_payload.to_string()),
        "PostToolUse" => {
            let tool_name = parsed
                .as_ref()
                .and_then(|value| value.get("tool_name"))
                .and_then(Value::as_str)
                .unwrap_or("unknown-tool");
            let input = parsed
                .as_ref()
                .and_then(|value| value.get("input"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let output = parsed
                .as_ref()
                .and_then(|value| value.get("output"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            let exit_code = parsed
                .as_ref()
                .and_then(|value| value.get("exit_code"))
                .and_then(Value::as_i64)
                .unwrap_or_default();
            format!("tool={tool_name}\nexit_code={exit_code}\ninput={input}\noutput={output}")
        }
        "SessionStart" | "SessionEnd" => parsed
            .map(|value| {
                serde_json::to_string_pretty(&value).unwrap_or_else(|_| raw_payload.to_string())
            })
            .unwrap_or_else(|| raw_payload.to_string()),
        _ => raw_payload.to_string(),
    }
}

fn persist_raw_payload(raw_payload: &str, mempal_home: &Path) -> Result<String> {
    let payload_dir = mempal_home.join("hook-payloads");
    fs::create_dir_all(&payload_dir)
        .with_context(|| format!("failed to create {}", payload_dir.display()))?;
    let digest = blake3::hash(raw_payload.as_bytes()).to_hex().to_string();
    let path = payload_dir.join(format!("{digest}.json"));
    if !path.exists() {
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .with_context(|| format!("failed to create {}", path.display()))?;
        file.write_all(raw_payload.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
        file.flush()
            .with_context(|| format!("failed to flush {}", path.display()))?;
    }
    Ok(path.to_string_lossy().to_string())
}

struct DaemonEmbedder {
    primary: Box<dyn Embedder>,
    fallback: Option<Box<dyn Embedder>>,
}

#[cfg(unix)]
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn daemon_signal_handler(_signal: i32) {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
}

#[cfg(unix)]
fn install_shutdown_handlers() -> Result<()> {
    SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    // SAFETY: installs a process signal handler that only writes an AtomicBool,
    // which is signal-safe.
    unsafe {
        let handler = daemon_signal_handler as *const () as usize;
        if libc::signal(libc::SIGTERM, handler) == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error())
                .context("failed to install SIGTERM handler");
        }
        if libc::signal(libc::SIGINT, handler) == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error())
                .context("failed to install SIGINT handler");
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn install_shutdown_handlers() -> Result<()> {
    Ok(())
}

#[cfg(unix)]
fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

#[cfg(not(unix))]
fn shutdown_requested() -> bool {
    false
}

impl DaemonEmbedder {
    async fn from_config(config: &crate::core::config::Config) -> crate::embed::Result<Self> {
        let primary = build_backend_from_name(config, config.embed.backend.as_str()).await?;
        let fallback = match config.embed.fallback.as_deref() {
            Some(name) if name.eq_ignore_ascii_case(config.embed.backend.as_str()) => None,
            Some(name) => Some(build_backend_from_name(config, name).await?),
            None => None,
        };
        Ok(Self { primary, fallback })
    }
}

#[async_trait::async_trait]
impl Embedder for DaemonEmbedder {
    async fn embed(&self, texts: &[&str]) -> crate::embed::Result<Vec<Vec<f32>>> {
        let status = global_embed_status();
        if let Some(fallback) = &self.fallback {
            match self.primary.embed(texts).await {
                Ok(vectors) => {
                    status.record_primary_success();
                    Ok(vectors)
                }
                Err(primary_error) => {
                    status.record_failure(&primary_error);
                    let message = format!(
                        "embedder fallback active: {} failed, using {}",
                        self.primary.name(),
                        fallback.name()
                    );
                    let vectors = fallback.embed(texts).await?;
                    status.record_fallback_success(message);
                    Ok(vectors)
                }
            }
        } else {
            self.primary.embed(texts).await
        }
    }

    fn dimensions(&self) -> usize {
        self.primary.dimensions()
    }

    fn name(&self) -> &str {
        self.primary.name()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::core::queue::{ClaimedMessage, QueueError};

    use super::{ClaimNextSource, ClaimPollResult, poll_claim_next};

    struct StubClaimSource {
        responses: Mutex<VecDeque<Result<Option<ClaimedMessage>, QueueError>>>,
    }

    impl StubClaimSource {
        fn new(responses: Vec<Result<Option<ClaimedMessage>, QueueError>>) -> Self {
            Self {
                responses: Mutex::new(VecDeque::from(responses)),
            }
        }
    }

    impl ClaimNextSource for StubClaimSource {
        fn claim_next(
            &self,
            _worker_id: &str,
            _claim_ttl_secs: i64,
        ) -> crate::core::queue::Result<Option<ClaimedMessage>> {
            self.responses
                .lock()
                .expect("responses mutex")
                .pop_front()
                .expect("stub response")
        }
    }

    fn claimed_message(id: &str) -> ClaimedMessage {
        ClaimedMessage {
            id: id.to_string(),
            kind: "hook_user_prompt".to_string(),
            payload: "{}".to_string(),
            retry_count: 0,
            claim_token: "worker:claim".to_string(),
            source_hash: "hash".to_string(),
        }
    }

    #[tokio::test]
    async fn test_daemon_survives_transient_claim_error() {
        let store = StubClaimSource::new(vec![
            Err(QueueError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error {
                    code: rusqlite::ErrorCode::DatabaseBusy,
                    extended_code: rusqlite::ffi::SQLITE_BUSY,
                },
                Some("database is locked".to_string()),
            ))),
            Ok(Some(claimed_message("msg-1"))),
        ]);
        let slept = AtomicUsize::new(0);

        let first = poll_claim_next(&store, "worker-a", 60, |_| {
            slept.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(()))
        })
        .await;
        let second =
            poll_claim_next(&store, "worker-a", 60, |_| Box::pin(std::future::ready(()))).await;

        assert!(matches!(first, ClaimPollResult::RetryAfterError));
        assert_eq!(slept.load(Ordering::SeqCst), 1);
        match second {
            ClaimPollResult::Claimed(message) => assert_eq!(message.id, "msg-1"),
            ClaimPollResult::Idle | ClaimPollResult::RetryAfterError => {
                panic!("expected claimed message on retry")
            }
        }
    }
}
