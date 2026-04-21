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
    utils::{build_drawer_id, current_timestamp, synthetic_source_file},
};
use crate::embed::{
    EmbedError, Embedder, build_backend_from_name, global_embed_status,
    retry::{HeartbeatCallback, retry_embed_operation},
};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::daemon_bootstrap::DaemonContext;
use crate::hook::CapturedHookEnvelope;

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
                    context.config.as_ref(),
                    &context.mempal_home,
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
    config: &crate::core::config::Config,
    mempal_home: &Path,
) -> Result<String> {
    let envelope: CapturedHookEnvelope =
        serde_json::from_str(&message.payload).context("failed to decode queued hook envelope")?;

    let record = build_drawer_record(&envelope, config, mempal_home)?;
    let heartbeat_store = store.clone();
    let heartbeat_message_id = message.id.clone();
    let heartbeat_worker_id = worker_id.to_string();
    let heartbeat = move || -> crate::embed::Result<()> {
        heartbeat_store
            .refresh_heartbeat(&heartbeat_message_id, &heartbeat_worker_id)
            .map_err(|error| EmbedError::Runtime(format!("refresh heartbeat failed: {error}")))?;
        Ok(())
    };

    let vector = embed_text_with_heartbeat(embedder, &record.content, Some(&heartbeat)).await?;
    let drawer_id = build_drawer_id(&record.wing, Some(record.room.as_str()), &record.content);
    let drawer = Drawer {
        id: drawer_id.clone(),
        content: record.content,
        wing: record.wing,
        room: Some(record.room),
        source_file: Some(record.source_file),
        source_type: SourceType::Manual,
        added_at: current_timestamp(),
        chunk_index: Some(0),
        importance: 0,
    };
    db.insert_drawer(&drawer)
        .with_context(|| format!("failed to insert hook drawer {}", drawer.id))?;
    db.insert_vector(&drawer.id, &vector)
        .with_context(|| format!("failed to insert hook vector {}", drawer.id))?;

    Ok(drawer_id)
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
}

fn build_drawer_record(
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

    Ok(DrawerRecord {
        wing: config.hooks.wing.clone(),
        room: envelope.agent.clone(),
        source_file: payload_path,
        content,
    })
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
